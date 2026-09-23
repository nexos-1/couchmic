//! WebRTC receiver: Safari sends the microphone as an Opus audio track (RTP over DTLS/SRTP),
//! which is decoded here and pushed into the jitter buffer.
//!
//! Compared to the PCM WebSocket path: Opus (32 to 64 instead of 770 kbit/s), UDP instead of TCP
//! (no head-of-line blocking), Opus PLC and in-band FEC on loss. The WebSocket stays open as the
//! signaling and control channel; when it closes, the peer connection is torn down.
//!
//! ICE: host candidates only, on a fixed UDP port (`--rtc-port`), no STUN/TURN servers. In a
//! tailnet both sides reach each other directly; the PC learns the iPad address from the ICE
//! binding requests (prflx), even if Safari only offers mDNS candidates.

use crate::audio::{SharedBuffer, SourceGate};
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use std::sync::{Arc, Mutex};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::runtime::{channel, default_runtime, Receiver, Runtime, Sender};

/// Running counters of the RTC path, for /api/stats.
#[derive(Debug, Default, Clone, Copy)]
pub struct RtcStats {
    pub packets: u64,
    pub lost: u64,
    pub fec_recovered: u64,
    pub plc_frames: u64,
    pub decode_errors: u64,
    pub connected: bool,
}

pub type SharedRtcStats = Arc<Mutex<RtcStats>>;

struct Handler {
    runtime: Arc<dyn Runtime>,
    buffer: SharedBuffer,
    stats: SharedRtcStats,
    gate: SourceGate,
    gather_complete_tx: Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_complete_tx.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::info!(?state, "rtc connection state");
        // A superseded connection must not overwrite the state of the active one.
        if self.gate.is_active() {
            let mut s = self.stats.lock().unwrap();
            s.connected = matches!(state, RTCPeerConnectionState::Connected);
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let buffer = self.buffer.clone();
        let stats = self.stats.clone();
        let gate = self.gate.clone();
        self.runtime.spawn(Box::pin(async move {
            if let Err(e) = decode_loop(track, buffer, stats, gate).await {
                tracing::warn!("rtc decode loop ended: {e}");
            }
        }));
    }
}

/// RTP -> Opus -> PCM 48 kHz mono -> jitter buffer. Loss: the first missing packet is rebuilt
/// from the next packet via in-band FEC, larger gaps via PLC (up to 5 frames).
async fn decode_loop(
    track: Arc<dyn TrackRemote>,
    buffer: SharedBuffer,
    stats: SharedRtcStats,
    gate: SourceGate,
) -> Result<(), String> {
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).map_err(|e| e.to_string())?;
    let mut pcm = vec![0i16; 5760]; // up to 120 ms at 48 kHz
    let mut expected: Option<u16> = None;
    let mut last_frame: usize = 960; // 20 ms default, updated from the packets
    let mut claimed = false;
    tracing::info!("rtc audio track: opus decoder ready");
    {
        buffer.lock().unwrap().set_input_rate(48_000);
    }
    while let Some(evt) = track.poll().await {
        match evt {
            TrackRemoteEvent::OnRtpPacket(pkt) => {
                let seq = pkt.header.sequence_number;
                let payload: &[u8] = &pkt.payload;
                // The first RTP packet proves the peer connection works: only now take over the
                // source (a failed or bogus offer must not silence the device that is playing).
                // Superseded later (another device is sending): decode nothing, but track the
                // sequence so the pause does not count as loss when this source comes back.
                if !claimed {
                    claimed = true;
                    gate.take_over();
                    let mut s = stats.lock().unwrap();
                    *s = RtcStats::default();
                    s.connected = true;
                } else if !gate.allowed() {
                    expected = Some(seq.wrapping_add(1));
                    continue;
                }
                if payload.is_empty() {
                    expected = Some(seq.wrapping_add(1));
                    continue;
                }
                if let Ok(n) = dec.get_nb_samples(payload) {
                    if n > 0 && n <= pcm.len() {
                        last_frame = n;
                    }
                }
                if let Some(exp) = expected {
                    let gap = seq.wrapping_sub(exp);
                    if (1..10).contains(&gap) {
                        let mut s = stats.lock().unwrap();
                        s.lost += gap as u64;
                        drop(s);
                        buffer.lock().unwrap().note_loss(gap as u64);
                        // First missing packet: FEC from the current packet.
                        let fec_ok = gap >= 1
                            && dec
                                .decode(payload, &mut pcm[..last_frame], true)
                                .map(|n| {
                                    buffer.lock().unwrap().push_i16(&pcm[..n]);
                                    stats.lock().unwrap().fec_recovered += 1;
                                    true
                                })
                                .unwrap_or(false);
                        let plc_needed = if fec_ok { gap - 1 } else { gap }.min(5);
                        for _ in 0..plc_needed {
                            if let Ok(n) = dec.decode(&[], &mut pcm[..last_frame], false) {
                                buffer.lock().unwrap().push_i16(&pcm[..n]);
                                stats.lock().unwrap().plc_frames += 1;
                            }
                        }
                    } else if (10..0x8000).contains(&gap) {
                        // Large gap: resync the decoder, no PLC storm.
                        let _ = dec.reset_state();
                        stats.lock().unwrap().lost += gap as u64;
                        buffer.lock().unwrap().note_loss(3);
                    }
                }
                match dec.decode(payload, &mut pcm, false) {
                    Ok(n) => {
                        buffer.lock().unwrap().push_i16(&pcm[..n]);
                        stats.lock().unwrap().packets += 1;
                    }
                    Err(e) => {
                        let n = {
                            let mut s = stats.lock().unwrap();
                            s.decode_errors += 1;
                            s.decode_errors
                        };
                        if n <= 5 || n % 500 == 0 {
                            tracing::warn!(
                                pt = pkt.header.payload_type,
                                ssrc = pkt.header.ssrc,
                                seq,
                                len = payload.len(),
                                toc = format!("{:02x}", payload[0]),
                                ext = pkt.header.extension,
                                pad = pkt.header.padding,
                                "opus decode error #{n}: {e}"
                            );
                        }
                    }
                }
                expected = Some(seq.wrapping_add(1));
            }
            TrackRemoteEvent::OnEnded => break,
            _ => {}
        }
    }
    Ok(())
}

pub struct RtcSession {
    pub pc: Arc<dyn PeerConnection>,
}

impl RtcSession {
    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

/// Takes Safari's SDP offer, builds the peer connection (recvonly audio) and returns the answer
/// SDP with all host candidates (no trickle ICE needed).
pub async fn accept_offer(
    offer_sdp: String,
    buffer: SharedBuffer,
    stats: SharedRtcStats,
    gate: SourceGate,
    udp_addr: &str,
) -> Result<(String, RtcSession), String> {
    let runtime = default_runtime().ok_or("no webrtc runtime")?;
    let setting_engine = SettingEngine::default();
    let mut media_engine = MediaEngine::default();
    // Offer Opus only. With register_default_codecs() PCMA (PT 8, G.711) ended up in the answer,
    // and Safari/Chrome then actually sent G.711.
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=0;sprop-stereo=0".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
            },
            RtpCodecKind::Audio,
        )
        .map_err(|e| e.to_string())?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .map_err(|e| e.to_string())?;
    let config = RTCConfigurationBuilder::new().build();

    let (gather_complete_tx, mut gather_complete_rx): (Sender<()>, Receiver<()>) = channel::<()>(1);
    let handler = Arc::new(Handler {
        runtime: runtime.clone(),
        buffer,
        stats,
        gate,
        gather_complete_tx,
    });

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_setting_engine(setting_engine)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler as Arc<dyn PeerConnectionEventHandler>)
        .with_runtime(runtime)
        .with_udp_addrs(vec![udp_addr.to_string()])
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);

    pc.add_transceiver_from_kind(
        RtpCodecKind::Audio,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            ..Default::default()
        }),
    )
    .await
    .map_err(|e| e.to_string())?;

    let offer = RTCSessionDescription::offer(offer_sdp).map_err(|e| e.to_string())?;
    pc.set_remote_description(offer)
        .await
        .map_err(|e| e.to_string())?;
    let answer = pc.create_answer(None).await.map_err(|e| e.to_string())?;
    pc.set_local_description(answer)
        .await
        .map_err(|e| e.to_string())?;
    // Host candidates are there right away; still wait for "complete" so they are in the SDP.
    let _ =
        tokio::time::timeout(std::time::Duration::from_secs(3), gather_complete_rx.recv()).await;
    let local = pc.local_description().await.ok_or("no local description")?;
    Ok((local.sdp, RtcSession { pc }))
}
