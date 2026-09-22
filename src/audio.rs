//! Audio output with jitter buffer, sinc resampling and drift compensation.
//!
//! Input: PCM blocks from the network (mono, i16, any rate), from the network thread.
//! Output: cpal stream on the chosen device (f32, device rate, device channels).
//!
//! Structure:
//! - `pending`: input samples at the input rate, waiting for a full resampler block.
//! - Resampler (rubato, windowed sinc interpolation) converts blocks to the output rate,
//!   runs in the network thread, not in the audio callback.
//! - `queue`: finished samples at the output rate, the actual jitter buffer.
//!
//! Rules:
//! - Target buffer `target_ms`. Below it silence is played (underrun counted), never stretched.
//! - Above `max_ms` the oldest samples are dropped (latency must not grow).
//! - iPad and PC clocks drift. Instead of dropping samples the resampling ratio is adjusted by
//!   at most +-0.5 percent (inaudible, no clicks).
//! - Adaptive: after an underrun or packet loss the target grows (up to 200 ms); after 30 s
//!   without trouble it shrinks step by step back to the start value.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Input samples per resampler block (10 ms at 48 kHz).
const CHUNK: usize = 480;
/// Upper bound of the adaptive target buffer.
const TARGET_MAX_MS: f64 = 200.0;
/// Quiet time without trouble after which the target shrinks again.
const CALM_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub frames_received: u64,
    pub samples_received: u64,
    pub underruns: u64,
    pub dropped_samples: u64,
    pub buffered_ms: f64,
    pub target_ms: f64,
    pub ratio: f64,
    pub input_rate: u32,
    pub output_rate: u32,
}

pub struct JitterBuffer {
    pending: Vec<f32>,
    scratch: Vec<f32>,
    queue: VecDeque<f32>,
    resampler: Option<Async<f32>>,
    input_rate: u32,
    output_rate: u32,
    base_target_ms: f64,
    target_ms: f64,
    max_ms: f64,
    ratio: f64,
    stats: Stats,
    primed: bool,
    last_trouble: Option<Instant>,
    last_shrink: Instant,
    last_push: Option<Instant>,
    /// Last underrun: (time, target before). If the source ends normally, the last underrun
    /// was just the buffer running dry and is taken back on disconnect.
    last_underrun: Option<(Instant, f64)>,
}

fn make_resampler(input_rate: u32, output_rate: u32) -> Option<Async<f32>> {
    // 128 taps, Blackman-Harris window: > 90 dB attenuation, 1.3 ms delay at 48 kHz.
    let params = SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2)
        .oversampling_factor(256)
        .interpolation(SincInterpolationType::Quadratic);
    let ratio = output_rate as f64 / input_rate as f64;
    match Async::<f32>::new_sinc(ratio, 1.05, &params, CHUNK, 1, FixedAsync::Input) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::error!("resampler init failed ({input_rate} -> {output_rate}): {e}");
            None
        }
    }
}

impl JitterBuffer {
    pub fn new(output_rate: u32, target_ms: f64, max_ms: f64) -> Self {
        let input_rate = 48_000;
        let resampler = make_resampler(input_rate, output_rate);
        let scratch_len = resampler
            .as_ref()
            .map(|r| r.output_frames_max() + 16)
            .unwrap_or(CHUNK * 2);
        Self {
            pending: Vec::with_capacity(CHUNK * 4),
            scratch: vec![0.0; scratch_len],
            queue: VecDeque::with_capacity(output_rate as usize),
            resampler,
            input_rate,
            output_rate,
            base_target_ms: target_ms,
            target_ms,
            max_ms,
            ratio: 1.0,
            stats: Stats {
                output_rate,
                input_rate,
                ratio: 1.0,
                target_ms,
                ..Default::default()
            },
            primed: false,
            last_trouble: None,
            last_shrink: Instant::now(),
            last_push: None,
            last_underrun: None,
        }
    }

    /// The source disconnected normally: clear the buffer, and if the last underrun was just the
    /// buffer running dry after the last packet, it does not count as trouble.
    pub fn source_disconnected(&mut self) {
        if let (Some((t_ur, target_before)), Some(t_push)) = (self.last_underrun, self.last_push) {
            if t_ur >= t_push {
                self.stats.underruns = self.stats.underruns.saturating_sub(1);
                self.target_ms = target_before;
                self.stats.target_ms = target_before;
                self.last_trouble = None;
            }
        }
        self.last_underrun = None;
        self.queue.clear();
        self.pending.clear();
        self.primed = false;
        self.stats.buffered_ms = 0.0;
    }

    pub fn set_input_rate(&mut self, rate: u32) {
        let rate = rate.clamp(8_000, 192_000);
        if rate != self.input_rate {
            self.input_rate = rate;
            self.stats.input_rate = self.input_rate;
            self.pending.clear();
            self.resampler = make_resampler(self.input_rate, self.output_rate);
            if let Some(r) = &self.resampler {
                self.scratch.resize(r.output_frames_max() + 16, 0.0);
            }
        }
    }

    /// Trouble from outside (e.g. RTP packet loss): raise the target buffer slightly.
    pub fn note_loss(&mut self, packets: u64) {
        let add = 10.0 * packets.min(3) as f64;
        self.grow_target(add);
    }

    fn grow_target(&mut self, add_ms: f64) {
        self.target_ms = (self.target_ms + add_ms).min(TARGET_MAX_MS.max(self.base_target_ms));
        self.stats.target_ms = self.target_ms;
        self.last_trouble = Some(Instant::now());
        self.last_shrink = Instant::now();
    }

    fn maybe_shrink_target(&mut self) {
        if self.target_ms <= self.base_target_ms {
            return;
        }
        let now = Instant::now();
        let calm = self
            .last_trouble
            .map(|t| now.duration_since(t) >= CALM_AFTER)
            .unwrap_or(true);
        if calm && now.duration_since(self.last_shrink) >= CALM_AFTER {
            self.target_ms = (self.target_ms * 0.8).max(self.base_target_ms);
            self.stats.target_ms = self.target_ms;
            self.last_shrink = now;
        }
    }

    /// Takes mono i16, resamples block by block to the output rate and appends to the buffer.
    pub fn push_i16(&mut self, samples: &[i16]) {
        self.stats.frames_received += 1;
        self.stats.samples_received += samples.len() as u64;
        self.last_push = Some(Instant::now());
        self.pending
            .extend(samples.iter().map(|s| *s as f32 / 32768.0));
        self.maybe_shrink_target();

        // Drift control: buffer above target -> slightly fewer output samples per block (ratio < 1),
        // below it slightly more. Ramp on, so the change stays inaudible.
        let err = self.buffered_ms() - self.target_ms;
        let rel = (1.0 - err * 0.0005).clamp(0.995, 1.005);
        self.ratio = 1.0 / rel;
        self.stats.ratio = self.ratio;
        if let Some(r) = self.resampler.as_mut() {
            let _ = r.set_resample_ratio_relative(rel, true);
        }

        while self.pending.len() >= CHUNK {
            let produced = match self.resampler.as_mut() {
                Some(r) => {
                    let input = InterleavedSlice::new(&self.pending[..CHUNK], 1, CHUNK);
                    let out_len = self.scratch.len();
                    let output = InterleavedSlice::new_mut(&mut self.scratch[..], 1, out_len);
                    match (input, output) {
                        (Ok(input), Ok(mut output)) => {
                            match r.process_into_buffer(&input, &mut output, None) {
                                Ok((_, n)) => n,
                                Err(e) => {
                                    tracing::warn!("resample error: {e}");
                                    0
                                }
                            }
                        }
                        _ => 0,
                    }
                }
                None => {
                    // No resampler (should not happen): pass through 1:1.
                    self.scratch[..CHUNK].copy_from_slice(&self.pending[..CHUNK]);
                    CHUNK
                }
            };
            self.queue.extend(self.scratch[..produced].iter().copied());
            self.pending.drain(..CHUNK);
        }

        let max_samples = (self.max_ms / 1000.0 * self.output_rate as f64) as usize;
        if self.queue.len() > max_samples {
            let drop = self.queue.len() - max_samples;
            self.queue.drain(..drop);
            self.stats.dropped_samples += drop as u64;
        }
        let target_samples = (self.target_ms / 1000.0 * self.output_rate as f64) as usize;
        if !self.primed && self.queue.len() >= target_samples {
            self.primed = true;
        }
    }

    /// Buffered time in ms (finished samples plus the waiting partial block).
    pub fn buffered_ms(&self) -> f64 {
        self.queue.len() as f64 * 1000.0 / self.output_rate as f64
            + self.pending.len() as f64 * 1000.0 / self.input_rate as f64
    }

    fn underrun(&mut self) {
        self.stats.underruns += 1;
        self.primed = false;
        self.last_underrun = Some((Instant::now(), self.target_ms));
        // Adaptive: more headroom next time.
        let grow = (self.target_ms * 0.5).max(10.0);
        self.grow_target(grow);
    }

    /// Fills `out` (interleaved, `channels` channels) at the output rate. Runs in the audio callback.
    pub fn pull(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let frames = out.len() / channels;
        self.stats.buffered_ms = self.buffered_ms();
        if !self.primed || self.queue.is_empty() {
            out.fill(0.0);
            if self.primed {
                self.underrun();
            }
            return;
        }
        for f in 0..frames {
            let Some(sample) = self.queue.pop_front() else {
                // Ran dry in the middle of the callback: one underrun, rest silence, prime again.
                self.underrun();
                out[f * channels..].fill(0.0);
                return;
            };
            for c in 0..channels {
                out[f * channels + c] = sample;
            }
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn base_target_ms(&self) -> f64 {
        self.base_target_ms
    }

    pub fn max_ms(&self) -> f64 {
        self.max_ms
    }
}

pub type SharedBuffer = Arc<Mutex<JitterBuffer>>;

/// Decides which connection may play into the jitter buffer. `active` is shared and holds the
/// id of the active connection (0 = none), `id` is our own.
#[derive(Clone)]
pub struct SourceGate {
    active: Arc<AtomicU64>,
    id: u64,
}

impl SourceGate {
    pub fn new(active: Arc<AtomicU64>, id: u64) -> Self {
        debug_assert!(id != 0, "0 means no source");
        Self { active, id }
    }

    /// This connection starts sending and becomes the active source (the newest wins).
    pub fn take_over(&self) {
        let prev = self.active.swap(self.id, Ordering::AcqRel);
        if prev != 0 && prev != self.id {
            tracing::info!(from = prev, to = self.id, "new audio source takes over");
        }
    }

    /// May this connection play right now? If nobody is active, it becomes active.
    pub fn allowed(&self) -> bool {
        match self
            .active
            .compare_exchange(0, self.id, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(current) => current == self.id,
        }
    }

    /// The connection ends. Returns true if it was the active source (then none is active now).
    pub fn release(&self) -> bool {
        self.active
            .compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_until_primed_then_audio() {
        let mut jb = JitterBuffer::new(48_000, 20.0, 200.0);
        let mut out = vec![0.0f32; 480 * 2];
        jb.pull(&mut out, 2);
        assert!(out.iter().all(|s| *s == 0.0), "only silence before priming");
        let tone: Vec<i16> = (0..4800)
            .map(|i| ((i % 48) as f32 / 48.0 * 20000.0) as i16)
            .collect();
        jb.push_i16(&tone);
        // The first block contains the resampler delay (silence), so read two callbacks.
        jb.pull(&mut out, 2);
        jb.pull(&mut out, 2);
        assert!(out.iter().any(|s| *s != 0.0), "signal after priming");
        assert_eq!(out[0], out[1], "mono is copied to both channels");
    }

    #[test]
    fn drops_when_over_max() {
        let mut jb = JitterBuffer::new(48_000, 20.0, 100.0);
        jb.push_i16(&vec![100i16; 48_000]);
        assert!(jb.buffered_ms() <= 100.0 + 0.1, "{}", jb.buffered_ms());
        assert!(jb.stats().dropped_samples > 0);
    }

    #[test]
    fn resamples_44100_to_48000() {
        let mut jb = JitterBuffer::new(48_000, 10.0, 5000.0);
        jb.set_input_rate(44_100);
        jb.push_i16(&vec![1000i16; 44_100]);
        let before = jb.buffered_ms();
        assert!(
            (before - 1000.0).abs() < 25.0,
            "one second of input gives about one second of output, here {before} ms"
        );
        let mut out = vec![0.0f32; 48_000];
        jb.pull(&mut out, 1);
        assert!(jb.buffered_ms() < 30.0, "rest {} ms", jb.buffered_ms());
        // Sinc resampling of a DC signal stays constant in the middle.
        let mid = out[24_000];
        assert!((mid - 1000.0 / 32768.0).abs() < 0.002, "{mid}");
    }

    #[test]
    fn ratio_stays_inaudible() {
        let mut jb = JitterBuffer::new(48_000, 20.0, 400.0);
        jb.push_i16(&vec![0i16; 48_000 / 4]);
        let mut out = vec![0.0f32; 480];
        jb.pull(&mut out, 1);
        let r = jb.stats().ratio;
        assert!((0.995..=1.0051).contains(&r), "{r}");
    }

    #[test]
    fn tail_underrun_is_forgiven_on_disconnect() {
        let mut jb = JitterBuffer::new(48_000, 40.0, 500.0);
        jb.push_i16(&vec![0i16; 4800]);
        let mut out = vec![0.0f32; 48_000];
        jb.pull(&mut out, 1); // source is gone, buffer runs dry
        assert_eq!(jb.stats().underruns, 1);
        jb.source_disconnected();
        assert_eq!(
            jb.stats().underruns,
            0,
            "running dry after the end does not count"
        );
        assert_eq!(jb.stats().target_ms, 40.0);
        assert_eq!(jb.buffered_ms(), 0.0);
    }

    #[test]
    fn source_gate_newest_sender_wins_and_next_takes_over() {
        let active = Arc::new(AtomicU64::new(0));
        let ipad = SourceGate::new(active.clone(), 1);
        let iphone = SourceGate::new(active.clone(), 2);
        // Nobody active: whoever sends first plays.
        assert!(ipad.allowed());
        assert!(!iphone.allowed());
        // The iPhone starts sending and takes over.
        iphone.take_over();
        assert!(iphone.allowed());
        assert!(!ipad.allowed());
        // The silent connection disconnects: no effect on the active one.
        assert!(!ipad.release());
        assert!(iphone.allowed());
        // The active one disconnects: free for the next sending one.
        assert!(iphone.release());
        let ipad_again = SourceGate::new(active.clone(), 3);
        assert!(ipad_again.allowed());
    }

    #[test]
    fn input_rate_is_clamped() {
        let mut jb = JitterBuffer::new(48_000, 20.0, 200.0);
        jb.set_input_rate(u32::MAX);
        assert_eq!(jb.stats().input_rate, 192_000);
        jb.set_input_rate(0);
        assert_eq!(jb.stats().input_rate, 8_000);
    }

    #[test]
    fn target_grows_on_underrun_and_loss() {
        let mut jb = JitterBuffer::new(48_000, 40.0, 500.0);
        jb.push_i16(&vec![0i16; 4800]);
        let mut out = vec![0.0f32; 48_000];
        jb.pull(&mut out, 1); // runs dry -> underrun
        assert_eq!(jb.stats().underruns, 1);
        assert!(jb.stats().target_ms > 40.0, "{}", jb.stats().target_ms);
        let t = jb.stats().target_ms;
        jb.note_loss(2);
        assert!(jb.stats().target_ms > t);
        assert!(jb.stats().target_ms <= TARGET_MAX_MS);
        assert_eq!(jb.base_target_ms(), 40.0);
    }
}
