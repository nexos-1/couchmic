//! glass-mic, the Windows receiver.
//!
//! Two audio paths from the iPad:
//! - WebRTC (preferred): Safari sends an Opus audio track over UDP (DTLS/SRTP). Signaling runs
//!   over the WebSocket (JSON offer/answer), see webrtc_rx.rs.
//! - PCM over the WebSocket (fallback): 8-byte header (u32 LE sample_rate, u16 LE channels,
//!   u16 LE seq) followed by i16 LE mono samples, 10 ms blocks. See docs/PROTOCOL.md.
//!
//! Both feed the same jitter buffer, which cpal/WASAPI plays into "CABLE Input" (VB-CABLE);
//! "CABLE Output" then shows up as a microphone for every app. While an iPad is connected it
//! becomes the default microphone, afterwards the previous one comes back.
//!
//! The WebSocket is also the control channel: ping/pong for the latency display, hello, logs.
//! On the PC: tray icon with status and menu, toast on connect/disconnect.
//!
//! Several devices at once (iPad and iPhone): only one source plays into the jitter buffer,
//! otherwise two streams would mix into crackle. Whoever starts sending last takes over; when
//! the active source goes away, the next sending one continues.

use crate::access;
use crate::audio::{JitterBuffer, SharedBuffer, SourceGate, Stats};
use crate::i18n::{Lang, Texts};
use crate::logfile;
use crate::micswitch::MicSwitch;
use crate::tray::{self, TrayCommand, TrayHandle, TrayState};
use crate::webrtc_rx::{self, RtcSession, SharedRtcStats};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Valid sample rates on the PCM path; anything else is dropped (Safari sends 44.1 or 48 kHz,
/// Bluetooth headsets 8 to 16 kHz).
const PCM_RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;
/// At most this many channels in the PCM header; only the first one is used.
const PCM_MAX_CHANNELS: usize = 8;
/// The log file is rotated to `<name>.1` once it grows beyond this (checked on every write).
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;
/// Largest WebSocket message and frame. A real PCM block (10 ms at 48 kHz mono) is about 1 KB,
/// an SDP offer a few KB; the tungstenite default of 64 MiB would let one client exhaust memory.
const WS_MAX_MESSAGE: usize = 64 * 1024;
/// Text (JSON control) messages beyond this are ignored without parsing.
const TEXT_MAX: usize = 16 * 1024;
/// Concurrent WebSocket connections.
const MAX_CONNECTIONS: usize = 4;
/// A connection with no WebSocket message and no WebRTC audio for this long is closed, so a
/// silent client cannot keep the default microphone switched. The page pings every 2 s.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest PCM block accepted (real blocks are 10 ms).
const PCM_MAX_FRAME_MS: usize = 100;
/// Client "log" messages: at most this many per window, the rest is counted.
const CLIENT_LOG_BURST: u32 = 20;
const CLIENT_LOG_WINDOW: Duration = Duration::from_secs(10);
/// Client-supplied strings are cut to this many characters in the log.
const CLIENT_FIELD_MAX: usize = 200;
/// Written when the user quits from the tray; the watchdog then leaves glass-mic stopped until
/// the next Windows start or a manual start.
const STOP_MARKER: &str = "stopped-by-user";

const INDEX_HTML: &str = include_str!("web/index.html");
const MANIFEST: &str = include_str!("web/manifest.webmanifest");
const ICON_180: &[u8] = include_bytes!("web/icon-180.png");
const ICON_512: &[u8] = include_bytes!("web/icon-512.png");
const ICON_192: &[u8] = include_bytes!("web/icon-192.png");
const ICON_1024: &[u8] = include_bytes!("web/icon-1024.png");

#[derive(Parser, Debug)]
#[command(
    name = "glass-mic",
    version,
    about = "Use your iPad or iPhone microphone as a Windows microphone (via VB-CABLE)"
)]
struct Cli {
    /// Output device (substring of its name). "default" uses the Windows default output.
    #[arg(long, default_value = "CABLE Input")]
    device: String,
    /// List output devices and exit
    #[arg(long)]
    list: bool,
    /// Restore the previous default microphone if a killed or crashed run left the target
    /// (CABLE Output) as default, then exit. The uninstaller uses this.
    #[arg(long)]
    restore_mic: bool,
    /// HTTP port on 127.0.0.1 (put `tailscale serve` in front of it for HTTPS)
    #[arg(long, default_value_t = 8321)]
    port: u16,
    /// UDP port for WebRTC (ICE host candidate), must be allowed in the firewall
    #[arg(long, default_value_t = 8322)]
    rtc_port: u16,
    /// Target buffer in ms (latency versus robustness); grows up to 200 ms on trouble
    #[arg(long, default_value_t = 40.0)]
    target_ms: f64,
    /// Maximum buffer in ms, older audio beyond that is dropped
    #[arg(long, default_value_t = 500.0)]
    max_ms: f64,
    /// Recording device that becomes the default microphone while connected (substring)
    #[arg(long, default_value = "CABLE Output")]
    mic_filter: String,
    /// Do not switch the default microphone automatically
    #[arg(long)]
    no_switch: bool,
    /// Seconds after the last client before the previous microphone is restored
    #[arg(long, default_value_t = 5)]
    restore_grace_secs: u64,
    /// No tray icon
    #[arg(long)]
    no_tray: bool,
    /// No Windows notifications on connect/disconnect
    #[arg(long)]
    no_toast: bool,
    /// Append the log to this file. Default without a console: %LOCALAPPDATA%\GlassMic\glass-mic.log
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// Address the tray menu opens (default: https://<tailscale name>/ if known, else
    /// http://127.0.0.1:<port>/)
    #[arg(long)]
    ui_url: Option<String>,
    /// Additional allowed origin for the web page and WebSocket, e.g. "https://mic.example.net"
    /// (repeatable). Loopback and the Tailscale name are always allowed.
    #[arg(long = "allow-origin")]
    allow_origin: Vec<String>,
    /// Accept requests forwarded by `tailscale serve` from any tailnet user or tagged device,
    /// not only from the PC's own Tailscale user. Only for shared setups you trust, or behind a
    /// custom reverse proxy (together with --allow-origin).
    #[arg(long)]
    allow_any_tailnet_user: bool,
    /// Set by the scheduled task: do not start if the user quit glass-mic from the tray since the
    /// last Windows start.
    #[arg(long, hide = true)]
    watchdog: bool,
}

#[derive(Clone)]
struct AppState {
    buffer: SharedBuffer,
    device_name: Arc<String>,
    mic_name: Arc<String>,
    clients: Arc<Mutex<u32>>,
    switch: Arc<Mutex<Option<MicSwitch>>>,
    switch_enabled: Arc<AtomicBool>,
    restore_grace: Duration,
    rtc_stats: SharedRtcStats,
    rtc_udp_addr: Arc<String>,
    started_at: SystemTime,
    tray: Arc<Option<TrayHandle>>,
    toast: bool,
    texts: Arc<Texts>,
    /// Time of the last trouble (underrun/loss), for the yellow tray icon.
    last_trouble: Arc<Mutex<Option<Instant>>>,
    /// Connection whose audio currently plays into the buffer (0 = none), see SourceGate.
    active_source: Arc<AtomicU64>,
    next_conn_id: Arc<AtomicU64>,
    /// Open WebSocket connections (capped at MAX_CONNECTIONS).
    connections: Arc<AtomicUsize>,
}

fn device_label(d: &cpal::Device) -> String {
    d.description()
        .map(|x| x.name().to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Prints a line to stdout and ignores errors: the caller may have closed the pipe already
/// (PowerShell 5.1 does not wait for GUI-subsystem programs), and `println!` would panic then.
fn say(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Like `say`, for stderr.
fn say_err(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

fn list_devices() {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devs) => {
            for d in devs {
                let cfg = d
                    .default_output_config()
                    .map(|c| format!("{} Hz, {} ch", c.sample_rate(), c.channels()))
                    .unwrap_or_default();
                say(&format!("{}  [{cfg}]", device_label(&d)));
            }
        }
        Err(e) => say_err(&format!("cannot list devices: {e}")),
    }
}

fn pick_device(filter: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if filter.eq_ignore_ascii_case("default") {
        return host.default_output_device();
    }
    let f = filter.to_lowercase();
    host.output_devices()
        .ok()?
        .find(|d| device_label(d).to_lowercase().contains(&f))
}

fn start_output(
    device: &cpal::Device,
    buffer: SharedBuffer,
) -> Result<(cpal::Stream, u32, u16), String> {
    let cfg = device.default_output_config().map_err(|e| e.to_string())?;
    let rate: u32 = cfg.sample_rate();
    let channels = cfg.channels();
    let stream_cfg = cpal::StreamConfig {
        channels,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    {
        let mut b = buffer.lock().unwrap();
        *b = JitterBuffer::new(rate, b.base_target_ms(), b.max_ms());
    }
    let buf = buffer.clone();
    let stream = device
        .build_output_stream(
            stream_cfg,
            move |out: &mut [f32], _| {
                let mut b = buf.lock().unwrap();
                b.pull(out, channels as usize);
            },
            |e| tracing::error!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, rate, channels))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        MANIFEST,
    )
}

fn png(bytes: &'static [u8]) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        bytes,
    )
}

async fn icon_180() -> impl IntoResponse {
    png(ICON_180)
}

async fn icon_192() -> impl IntoResponse {
    png(ICON_192)
}

async fn icon_512() -> impl IntoResponse {
    png(ICON_512)
}

async fn icon_1024() -> impl IntoResponse {
    png(ICON_1024)
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

async fn stats(State(st): State<AppState>) -> impl IntoResponse {
    let s: Stats = st.buffer.lock().unwrap().stats();
    let r = *st.rtc_stats.lock().unwrap();
    let mic_active = st
        .switch
        .lock()
        .unwrap()
        .as_ref()
        .map(|m| m.is_active())
        .unwrap_or(false);
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "device": *st.device_name,
        "clients": *st.clients.lock().unwrap(),
        "mic_switched": mic_active,
        "switching_enabled": st.switch_enabled.load(Ordering::Relaxed),
        "uptime_s": st.started_at.elapsed().map(|d| d.as_secs()).unwrap_or(0),
        "frames_received": s.frames_received,
        "samples_received": s.samples_received,
        "underruns": s.underruns,
        "dropped_samples": s.dropped_samples,
        "buffered_ms": s.buffered_ms,
        "target_ms": s.target_ms,
        "ratio": s.ratio,
        "input_rate": s.input_rate,
        "output_rate": s.output_rate,
        "rtc": {
            "connected": r.connected,
            "packets": r.packets,
            "lost": r.lost,
            "fec_recovered": r.fec_recovered,
            "plc_frames": r.plc_frames,
            "decode_errors": r.decode_errors,
        }
    }))
}

/// Frees a connection slot when the WebSocket task (or a failed upgrade) ends.
struct ConnSlot(Arc<AtomicUsize>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    // Take the slot before the upgrade so the cap also holds under a burst of connections.
    if st.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
        st.connections.fetch_sub(1, Ordering::SeqCst);
        tracing::warn!("too many connections, rejected");
        return (StatusCode::SERVICE_UNAVAILABLE, "too many connections").into_response();
    }
    let slot = ConnSlot(st.connections.clone());
    ws.max_message_size(WS_MAX_MESSAGE)
        .max_frame_size(WS_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            handle_ws(socket, st).await
        })
        .into_response()
}

fn activate_switch(st: &AppState) {
    if !st.switch_enabled.load(Ordering::Relaxed) {
        return;
    }
    let sw = st.switch.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(m) = sw.lock().unwrap().as_mut() {
            m.activate();
        }
    });
}

async fn restore_switch(st: &AppState) {
    let sw = st.switch.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(m) = sw.lock().unwrap().as_mut() {
            m.restore();
        }
    })
    .await
    .ok();
}

fn notify(st: &AppState, body: String) {
    if !st.toast {
        return;
    }
    tokio::task::spawn_blocking(move || tray::toast("Glass Mic", &body));
}

fn on_client_connected(st: &AppState) {
    let n = {
        let mut c = st.clients.lock().unwrap();
        *c += 1;
        *c
    };
    tracing::info!(clients = n, "mic client connected");
    if n == 1 {
        activate_switch(st);
        let switched = st.switch_enabled.load(Ordering::Relaxed);
        notify(st, st.texts.toast_connected(&st.mic_name, switched));
    }
}

fn on_client_disconnected(st: &AppState) {
    let n = {
        let mut c = st.clients.lock().unwrap();
        *c = c.saturating_sub(1);
        *c
    };
    tracing::info!(clients = n, "mic client disconnected");
    if n == 0 {
        let st2 = st.clone();
        tokio::spawn(async move {
            tokio::time::sleep(st2.restore_grace).await;
            if *st2.clients.lock().unwrap() == 0 {
                let was_active = st2
                    .switch
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|m| m.is_active())
                    .unwrap_or(false);
                restore_switch(&st2).await;
                notify(&st2, st2.texts.toast_disconnected(was_active).to_string());
            }
        });
    }
}

/// Rate limit for client "log" messages per connection.
struct LogBudget {
    window_start: Instant,
    used: u32,
    dropped: u64,
}

impl LogBudget {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            used: 0,
            dropped: 0,
        }
    }

    fn allow(&mut self) -> bool {
        if self.window_start.elapsed() >= CLIENT_LOG_WINDOW {
            if self.dropped > 0 {
                tracing::info!(
                    dropped = self.dropped,
                    "client log messages dropped (rate limit)"
                );
            }
            self.window_start = Instant::now();
            self.used = 0;
            self.dropped = 0;
        }
        if self.used < CLIENT_LOG_BURST {
            self.used += 1;
            true
        } else {
            self.dropped += 1;
            false
        }
    }
}

/// Only these fields of client telemetry are logged, each cut and escaped, so a client can
/// neither forge log lines (newlines) nor write large amounts of text.
const CLIENT_LOG_FIELDS: [&str; 15] = [
    "ev",
    "vis",
    "standalone",
    "ctx",
    "running",
    "path",
    "state",
    "ice",
    "code",
    "reason",
    "enabled",
    "ready",
    "muted",
    "ua",
    "t",
];

fn client_fields(v: &serde_json::Value) -> String {
    let mut out = Vec::new();
    for key in CLIENT_LOG_FIELDS {
        let Some(val) = v.get(key) else { continue };
        let text = match val {
            serde_json::Value::String(s) => s.chars().take(CLIENT_FIELD_MAX).collect::<String>(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        out.push(format!("{key}={}", text.escape_debug()));
    }
    out.join(" ")
}

/// Control messages (JSON) from the client. Returns the reply as a JSON string, if any.
async fn handle_control(
    v: &serde_json::Value,
    st: &AppState,
    session: &mut Option<RtcSession>,
    gate: &SourceGate,
    log_budget: &mut LogBudget,
) -> Option<String> {
    match v.get("type").and_then(|t| t.as_str()) {
        Some("ping") => Some(
            serde_json::json!({
                "type": "pong",
                "t": v.get("t").cloned().unwrap_or_default(),
                "server_t": now_ms()
            })
            .to_string(),
        ),
        Some("offer") => {
            let sdp = v.get("sdp")?.as_str()?.to_string();
            if let Some(old) = session.take() {
                old.close().await;
            }
            // The source is taken over only when the first RTP packet arrives (decode loop), so
            // a failed or bogus offer cannot silence the device that is playing.
            match webrtc_rx::accept_offer(
                sdp,
                st.buffer.clone(),
                st.rtc_stats.clone(),
                gate.clone(),
                &st.rtc_udp_addr,
            )
            .await
            {
                Ok((answer, s)) => {
                    *session = Some(s);
                    tracing::info!("rtc: answer sent");
                    Some(serde_json::json!({ "type": "answer", "sdp": answer }).to_string())
                }
                Err(e) => {
                    tracing::warn!("rtc: offer rejected: {e}");
                    Some(serde_json::json!({ "type": "error", "message": e }).to_string())
                }
            }
        }
        Some("log") => {
            // Telemetry from the device (visibility, track state, peer connection). It only goes
            // into the local log file on this PC, nowhere else.
            if log_budget.allow() {
                tracing::info!(client = %client_fields(v), "client log");
            }
            None
        }
        Some("hello") => {
            tracing::info!(client = %client_fields(v), "client hello");
            Some(
                serde_json::json!({
                    "type": "welcome",
                    "host": hostname(),
                    "rtc": true,
                    "version": env!("CARGO_PKG_VERSION"),
                    "server_t": now_ms()
                })
                .to_string(),
            )
        }
        _ => None,
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into())
}

async fn handle_ws(mut socket: WebSocket, st: AppState) {
    let gate = SourceGate::new(
        st.active_source.clone(),
        st.next_conn_id.fetch_add(1, Ordering::Relaxed) + 1,
    );
    let mut last_seq: Option<u16> = None;
    let mut session: Option<RtcSession> = None;
    // A connection only counts as a client (and switches the default microphone) once it sends
    // audio or a WebRTC offer; a silent connection changes nothing.
    let mut counted = false;
    let mut pcm_started = false;
    let mut pcm_rate: Option<u32> = None;
    let mut rejected_frames: u64 = 0;
    let mut oversized_text: u64 = 0;
    let mut log_budget = LogBudget::new();
    let mut last_rtc_packets: u64 = 0;
    loop {
        let msg = match tokio::time::timeout(IDLE_TIMEOUT, socket.recv()).await {
            Ok(Some(Ok(m))) => m,
            Ok(_) => break,
            Err(_) => {
                // Nothing on the WebSocket for IDLE_TIMEOUT. Safari may throttle the page's ping
                // timer in the background, so live WebRTC audio of this connection also counts.
                let packets = st.rtc_stats.lock().unwrap().packets;
                if gate.is_active() && packets != last_rtc_packets {
                    last_rtc_packets = packets;
                    continue;
                }
                tracing::info!("closing idle connection");
                break;
            }
        };
        match msg {
            Message::Binary(b) => {
                if b.len() < 8 {
                    continue;
                }
                let rate = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let channels = u16::from_le_bytes([b[4], b[5]]) as usize;
                let seq = u16::from_le_bytes([b[6], b[7]]);
                let valid_header =
                    PCM_RATES.contains(&rate) && channels > 0 && channels <= PCM_MAX_CHANNELS;
                let frames = (b.len() - 8) / 2 / channels.max(1);
                let too_long = frames > rate as usize * PCM_MAX_FRAME_MS / 1000;
                // The rate is fixed per connection: switching it per block would rebuild the
                // resampler each time and burn CPU under the audio lock.
                let rate_changed = pcm_rate.is_some_and(|r| r != rate);
                if !valid_header || too_long || rate_changed {
                    rejected_frames += 1;
                    if rejected_frames == 1 {
                        tracing::warn!(
                            rate,
                            channels,
                            frames,
                            rate_changed,
                            "dropped invalid PCM block"
                        );
                    }
                    continue;
                }
                pcm_rate = Some(rate);
                if !counted {
                    counted = true;
                    on_client_connected(&st);
                }
                // The first PCM block of this connection takes over the source; after that it
                // only plays while no other connection has taken over.
                if !pcm_started {
                    pcm_started = true;
                    gate.take_over();
                } else if !gate.allowed() {
                    continue;
                }
                if let Some(prev) = last_seq {
                    if seq != prev.wrapping_add(1) {
                        tracing::debug!("seq gap {prev} -> {seq}");
                    }
                }
                last_seq = Some(seq);
                let pcm = &b[8..];
                let mut mono = Vec::with_capacity(frames);
                let mut i = 0;
                while i + 1 < pcm.len() {
                    mono.push(i16::from_le_bytes([pcm[i], pcm[i + 1]]));
                    i += 2 * channels;
                }
                let mut buf = st.buffer.lock().unwrap();
                buf.set_input_rate(rate);
                buf.push_i16(&mono);
            }
            Message::Text(t) => {
                if t.len() > TEXT_MAX {
                    oversized_text += 1;
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else {
                    continue;
                };
                if !counted && v.get("type").and_then(|x| x.as_str()) == Some("offer") {
                    counted = true;
                    on_client_connected(&st);
                }
                if let Some(reply) =
                    handle_control(&v, &st, &mut session, &gate, &mut log_budget).await
                {
                    if socket.send(Message::Text(reply.into())).await.is_err() {
                        break;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    if let Some(s) = session.take() {
        s.close().await;
    }
    // Only the active source clears the buffer and the RTC state; a silent second connection
    // must not empty the running stream when it disconnects.
    if gate.release() {
        st.rtc_stats.lock().unwrap().connected = false;
        st.buffer.lock().unwrap().source_disconnected();
    }
    if rejected_frames > 1 || oversized_text > 0 {
        tracing::warn!(rejected_frames, oversized_text, "dropped invalid messages");
    }
    if counted {
        on_client_disconnected(&st);
    }
}

/// Derive the tray status from the counters once per second.
async fn tray_status_loop(st: AppState) {
    let Some(tray) = st.tray.as_ref() else {
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_counts: Option<(u64, u64)> = None;
    loop {
        tick.tick().await;
        let s = st.buffer.lock().unwrap().stats();
        let r = *st.rtc_stats.lock().unwrap();
        let clients = *st.clients.lock().unwrap();
        let counts = (s.underruns, r.lost);
        if let Some(prev) = last_counts {
            if counts != prev && clients > 0 {
                *st.last_trouble.lock().unwrap() = Some(Instant::now());
            }
        }
        last_counts = Some(counts);
        let trouble = st
            .last_trouble
            .lock()
            .unwrap()
            .map(|t| t.elapsed() < Duration::from_secs(60))
            .unwrap_or(false);
        if clients == 0 {
            tray.set_state(
                TrayState::Idle,
                st.texts
                    .tooltip_idle(&st.device_name, st.switch_enabled.load(Ordering::Relaxed)),
            );
        } else {
            let path = if r.connected { "WebRTC/Opus" } else { "PCM" };
            let loss = if r.packets + r.lost > 0 {
                r.lost as f64 * 100.0 / (r.packets + r.lost) as f64
            } else {
                0.0
            };
            tray.set_state(
                TrayState::Connected {
                    path: path.to_string(),
                    clients,
                    trouble,
                },
                st.texts
                    .tooltip_connected(path, s.buffered_ms, s.target_ms, loss, s.underruns),
            );
        }
    }
}

/// Log the counters every 5 s, but only while a client is connected or something changed.
/// Before, this ran forever after the first connection and filled the log while idle.
async fn stats_log_loop(st: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut last: Option<(u64, u64, u64, u64)> = None;
    loop {
        tick.tick().await;
        let s = st.buffer.lock().unwrap().stats();
        let r = *st.rtc_stats.lock().unwrap();
        let clients = *st.clients.lock().unwrap();
        let now = (s.frames_received, r.packets, s.underruns, s.dropped_samples);
        let changed = last.is_some_and(|l| l != now);
        last = Some(now);
        if clients == 0 && !changed {
            continue;
        }
        tracing::info!(
            clients,
            pcm_frames = s.frames_received,
            rtc_packets = r.packets,
            rtc_lost = r.lost,
            fec = r.fec_recovered,
            plc = r.plc_frames,
            buffered_ms = format!("{:.1}", s.buffered_ms),
            target_ms = format!("{:.0}", s.target_ms),
            underruns = s.underruns,
            dropped = s.dropped_samples,
            ratio = format!("{:.4}", s.ratio),
            "mic stats"
        );
    }
}

async fn command_loop(
    st: AppState,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<TrayCommand>,
    quit_tx: tokio::sync::watch::Sender<bool>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            TrayCommand::SetSwitching(on) => {
                st.switch_enabled.store(on, Ordering::Relaxed);
                tracing::info!(enabled = on, "microphone switching changed from tray");
                if on {
                    if *st.clients.lock().unwrap() > 0 {
                        activate_switch(&st);
                    }
                } else {
                    restore_switch(&st).await;
                }
            }
            TrayCommand::Quit => {
                tracing::info!("quit from tray");
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Err(e) = std::fs::write(data_dir().join(STOP_MARKER), now.to_string()) {
                    tracing::warn!("cannot write stop marker: {e}");
                }
                let _ = quit_tx.send(true);
            }
        }
    }
}

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("GlassMic"))
        .unwrap_or_else(|| std::env::temp_dir().join("GlassMic"))
}

fn init_logging(log_file: Option<&PathBuf>) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // mDNS timeouts are normal in a tailnet (Safari offers .local candidates) and would
        // flood the log as ERROR.
        "info,webrtc=warn,rtc=warn,rtc_ice::agent::agent_proto=off".into()
    });
    if let Some(path) = log_file {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match logfile::RotatingFile::open(path.clone(), LOG_ROTATE_BYTES) {
            Ok(f) => {
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_ansi(false)
                    .with_writer(Mutex::new(f))
                    .init();
                return;
            }
            Err(e) => say_err(&format!("cannot write log file {}: {e}", path.display())),
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Full path of tailscale.exe: the default install location, else the first match on PATH.
/// Never a bare "tailscale", which Windows would also look up in glass-mic's own folder first.
fn tailscale_exe() -> Option<PathBuf> {
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        let p = PathBuf::from(pf).join("Tailscale").join("tailscale.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join("tailscale.exe"))
        .find(|p| p.is_file())
}

/// Parses `tailscale status --json`: the MagicDNS name ("<pc>.<tailnet>.ts.net.") that
/// `tailscale serve` offers HTTPS on, and the login of the Tailscale user owning this PC.
pub(crate) fn parse_tailscale_status(json: &[u8]) -> Option<access::TailscaleSelf> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let me = v.get("Self")?;
    let host = me
        .get("DNSName")?
        .as_str()?
        .trim_end_matches('.')
        .to_string();
    if host.is_empty() {
        return None;
    }
    let login = me
        .get("UserID")
        .and_then(|id| id.as_u64())
        .and_then(|id| {
            v.get("User")?
                .get(id.to_string())?
                .get("LoginName")?
                .as_str()
        })
        .map(str::to_string);
    Some(access::TailscaleSelf { host, login })
}

fn tailscale_self() -> Option<access::TailscaleSelf> {
    use std::os::windows::process::CommandExt;
    // tailscale.exe is a console program; without this flag every lookup would flash a window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(tailscale_exe()?)
        .args(["status", "--json"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_tailscale_status(&out.stdout)
}

/// Seconds since the Unix epoch at which Windows started.
fn boot_time_unix() -> u64 {
    let uptime_ms = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(uptime_ms / 1000)
}

/// True if the user quit from the tray since the last Windows start.
fn stopped_by_user() -> bool {
    let Ok(text) = std::fs::read_to_string(data_dir().join(STOP_MARKER)) else {
        return false;
    };
    // A few seconds of tolerance for the uptime rounding.
    text.trim()
        .parse::<u64>()
        .is_ok_and(|t| t + 5 >= boot_time_unix())
}

/// True if stdout is a pipe or a file (redirected by the caller), not a console or nothing.
fn stdout_redirected() -> bool {
    use windows::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_DISK, FILE_TYPE_PIPE};
    use windows::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    unsafe {
        match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(h) if !h.is_invalid() && !h.0.is_null() => {
                let t = GetFileType(h);
                t == FILE_TYPE_PIPE || t == FILE_TYPE_DISK
            }
            _ => false,
        }
    }
}

#[tokio::main]
pub async fn main() {
    // Output already redirected (pipe or file, e.g. `glass-mic --list | ...` in a script): keep
    // it. Attaching to the parent console would point stdout at that console instead, and the
    // caller would read nothing (install.ps1 checks for VB-CABLE that way).
    // Otherwise, started from a terminal: attach to its console so --list and --help are
    // visible. Without a parent console (Explorer, scheduled task) this fails and logging goes
    // to a file.
    let has_console = stdout_redirected()
        || unsafe {
            windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            )
            .is_ok()
        };
    let cli = Cli::parse();
    // Panics would otherwise only reach stderr, which a scheduled task does not record.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
    if cli.list || cli.restore_mic {
        // One-shot commands log to stderr only and never touch the log file of a running
        // instance.
        init_logging(None);
    }
    if cli.list {
        list_devices();
        return;
    }
    if cli.restore_mic {
        // MicSwitch::new restores from the state file when the target is still the default.
        let state = data_dir().join("previous-mic.txt");
        let had_state = state.exists();
        match MicSwitch::new(&cli.mic_filter, state) {
            Ok(_) if had_state => say("previous microphone checked and restored if needed"),
            Ok(_) => say("nothing to restore"),
            Err(e) => {
                say_err(&format!("cannot access audio endpoints: {e}"));
                std::process::exit(5);
            }
        }
        return;
    }
    if cli.watchdog && stopped_by_user() {
        // The user quit from the tray; the watchdog must not bring glass-mic back.
        return;
    }
    if !cli.watchdog {
        // A manual start (double click, or the task run by hand without --watchdog) clears it.
        let _ = std::fs::remove_file(data_dir().join(STOP_MARKER));
    }
    let addr = format!("127.0.0.1:{}", cli.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            // Usually a second instance. Bind first, before touching the log, audio or the
            // default microphone: a second instance must not restore the "previous" microphone
            // while the first one has an iPad connected.
            say_err(&format!(
                "cannot listen on {addr}, is glass-mic already running? {e}"
            ));
            std::process::exit(4);
        }
    };
    // From here on this is the only instance: log to the file (a second instance must not
    // rotate the log of the running one) and recover the default microphone right away, before
    // anything below can fail and exit.
    let log_file = cli
        .log_file
        .clone()
        .or_else(|| (!has_console).then(|| data_dir().join("glass-mic.log")));
    init_logging(log_file.as_ref());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "glass-mic starting");
    let switch = match MicSwitch::new(&cli.mic_filter, data_dir().join("previous-mic.txt")) {
        Ok(m) => {
            if let Some(cur) = m.current_default_name() {
                tracing::info!(current = %cur, "default microphone at startup");
            }
            Some(m)
        }
        Err(e) => {
            tracing::warn!("microphone switching unavailable: {e}");
            None
        }
    };

    let device = match pick_device(&cli.device) {
        Some(d) => d,
        None => {
            let msg = format!(
                "output device \"{}\" not found. Install VB-CABLE (https://vb-audio.com/Cable/) \
                 or pick another device with --device. Available:",
                cli.device
            );
            tracing::error!("{msg}");
            say_err(&msg);
            list_devices();
            std::process::exit(2);
        }
    };
    let device_name = device_label(&device);
    let buffer: SharedBuffer = Arc::new(Mutex::new(JitterBuffer::new(
        48_000,
        cli.target_ms,
        cli.max_ms,
    )));
    let (_stream, rate, channels) = match start_output(&device, buffer.clone()) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("cannot start audio output: {e}");
            say_err(&format!("cannot start audio output: {e}"));
            std::process::exit(3);
        }
    };
    tracing::info!(device = %device_name, rate, channels, target_ms = cli.target_ms, "audio output running");

    let texts = Arc::new(Texts::new(Lang::detect()));
    let log_dir = log_file
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(data_dir);
    if !cli.no_toast {
        tray::register_toast_app_id(ICON_192, &data_dir());
    }
    let ts_self = tailscale_self();
    match &ts_self {
        Some(t) if t.login.is_some() => tracing::info!(host = %t.host, "Tailscale identity known"),
        Some(t) => {
            tracing::warn!(host = %t.host, "Tailscale user unknown, tailnet requests are rejected until it is")
        }
        None => tracing::info!("Tailscale not available yet, looked up again on demand"),
    }
    let ui_url = cli
        .ui_url
        .clone()
        .or_else(|| ts_self.as_ref().map(|t| format!("https://{}/", t.host)))
        .unwrap_or_else(|| format!("http://127.0.0.1:{}/", cli.port));
    // An explicitly set --ui-url is our own page and therefore allowed.
    let mut allowed_origins = cli.allow_origin.clone();
    if let Some(u) = &cli.ui_url {
        allowed_origins.push(u.clone());
    }
    if cli.allow_any_tailnet_user {
        tracing::warn!("--allow-any-tailnet-user: every tailnet user can send audio");
    }
    let access = access::Access::new(
        access::AllowList::new(
            cli.port,
            ts_self,
            &allowed_origins,
            cli.allow_any_tailnet_user,
        ),
        tailscale_self,
    );

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TrayCommand>();
    let tray_handle = if cli.no_tray {
        None
    } else {
        match tray::start(
            ui_url.clone(),
            log_dir,
            !cli.no_switch,
            texts.clone(),
            cmd_tx.clone(),
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!("tray icon unavailable: {e}");
                None
            }
        }
    };

    let state = AppState {
        buffer: buffer.clone(),
        device_name: Arc::new(device_name),
        mic_name: Arc::new(cli.mic_filter.clone()),
        clients: Arc::new(Mutex::new(0)),
        switch: Arc::new(Mutex::new(switch)),
        switch_enabled: Arc::new(AtomicBool::new(!cli.no_switch)),
        restore_grace: Duration::from_secs(cli.restore_grace_secs),
        rtc_stats: Arc::new(Mutex::new(Default::default())),
        rtc_udp_addr: Arc::new(format!("0.0.0.0:{}", cli.rtc_port)),
        started_at: SystemTime::now(),
        tray: Arc::new(tray_handle),
        toast: !cli.no_toast,
        texts,
        last_trouble: Arc::new(Mutex::new(None)),
        active_source: Arc::new(AtomicU64::new(0)),
        next_conn_id: Arc::new(AtomicU64::new(0)),
        connections: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon-180.png", get(icon_180))
        .route("/icon-512.png", get(icon_512))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-1024.png", get(icon_1024))
        .route("/ws", get(ws_upgrade))
        .route("/api/stats", get(stats))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(access, access::guard));

    tokio::spawn(stats_log_loop(state.clone()));
    tokio::spawn(tray_status_loop(state.clone()));

    let (quit_tx, mut quit_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(command_loop(state.clone(), cmd_rx, quit_tx));

    tracing::info!(%addr, rtc_udp = %state.rtc_udp_addr, ui = %ui_url, "glass-mic listening");

    let shutdown_state = state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = quit_rx.wait_for(|q| *q) => {},
            }
            tracing::info!("shutdown: restoring default microphone");
            restore_switch(&shutdown_state).await;
            if let Some(t) = shutdown_state.tray.as_ref() {
                t.shutdown();
            }
            // Open WebSockets would otherwise keep serve() alive.
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::process::exit(0);
        })
        .await
        .expect("serve");
    // Only reached if the server ends without a shutdown signal: exit with an error so the
    // scheduled task (restart on failure) kicks in.
    tracing::error!("serve ended unexpectedly");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tailscale_status() {
        let json = br#"{"Self":{"DNSName":"pc.tail1234.ts.net.","UserID":42},
            "User":{"42":{"LoginName":"me@example.com"},"7":{"LoginName":"other@x.com"}}}"#;
        let t = parse_tailscale_status(json).unwrap();
        assert_eq!(t.host, "pc.tail1234.ts.net");
        assert_eq!(t.login.as_deref(), Some("me@example.com"));
        let no_user = br#"{"Self":{"DNSName":"pc.tail1234.ts.net.","UserID":42},"User":{}}"#;
        assert_eq!(parse_tailscale_status(no_user).unwrap().login, None);
        assert!(parse_tailscale_status(br#"{"Self":{"DNSName":""}}"#).is_none());
        assert!(parse_tailscale_status(b"garbage").is_none());
    }

    #[test]
    fn client_fields_are_whitelisted_cut_and_escaped() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"type":"log","ev":"a\nFAKE LINE","secret":"x","ua":"UUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUU","running":true,"code":1006}"#,
        )
        .unwrap();
        let f = client_fields(&v);
        assert!(!f.contains('\n'), "no raw newline: {f}");
        assert!(f.contains("ev=a\\nFAKE LINE"), "{f}");
        assert!(!f.contains("secret"));
        assert!(f.contains("running=true") && f.contains("code=1006"));
        let ua = f.split("ua=").nth(1).unwrap().split(' ').next().unwrap();
        assert_eq!(ua.len(), CLIENT_FIELD_MAX);
    }

    #[test]
    fn log_budget_limits_bursts() {
        let mut b = LogBudget::new();
        let allowed = (0..100).filter(|_| b.allow()).count();
        assert_eq!(allowed, CLIENT_LOG_BURST as usize);
    }
}
