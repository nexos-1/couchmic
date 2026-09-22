# Protocol

Everything the page (`src/web/index.html`) and glass-mic exchange. Useful if you want to write
your own sender (a native app, another browser page) or read the status from a script.

## HTTP routes (127.0.0.1:8321, behind `tailscale serve`)

| Route | Content |
| --- | --- |
| `GET /` | The sender page |
| `GET /manifest.webmanifest`, `/icon-*.png` | Home Screen icon and manifest |
| `GET /ws` | WebSocket: signaling, control and PCM audio |
| `GET /api/stats` | Status as JSON (see below) |

All routes require an allowed `Host` header; browser requests with an `Origin` header must come
from an allowed origin (see [SECURITY.md](../SECURITY.md)). Otherwise the answer is `403`.

## Page parameters

| Parameter | Effect |
| --- | --- |
| `?autostart=1` | Start the microphone on load (after the first manual start; Safari still wants one tap) |
| `?stop=1` | Stop the recording in another tab of the same browser (BroadcastChannel), for Shortcuts |
| `?tone=1` | Send a 440 Hz tone instead of the microphone (tests) |
| `?lang=de` / `?lang=en` | Force the language (default: device language) |
| `?appearance=light` / `?appearance=dark` | Force the appearance (default: system) |

## WebSocket `/ws`

One connection per sender. Text frames are JSON control messages, binary frames are PCM audio.

### Sender to glass-mic

| Message | Meaning |
| --- | --- |
| `{"type":"hello","ua":"...","standalone":false}` | First message; logged |
| `{"type":"ping","t":123.4}` | Latency probe; `t` is echoed back |
| `{"type":"offer","sdp":"..."}` | WebRTC offer with one audio track. Non-trickle: the SDP must contain the ICE candidates (the page waits for gathering to complete) |
| `{"type":"log", ...}` | Free-form telemetry, written to the local log only |

### glass-mic to sender

| Message | Meaning |
| --- | --- |
| `{"type":"welcome","host":"PC","rtc":true,"version":"0.1.0","server_t":...}` | Answer to `hello` |
| `{"type":"pong","t":123.4,"server_t":...}` | Answer to `ping` |
| `{"type":"answer","sdp":"..."}` | WebRTC answer with host candidates on `--rtc-port` |
| `{"type":"error","message":"..."}` | The offer was rejected |

### WebRTC path (preferred)

- Opus only, 48 kHz, mono, `useinbandfec=1`. glass-mic offers no other codec.
- ICE host candidates only (no STUN/TURN); inside a tailnet both sides reach each other directly.
- Closing the WebSocket tears the peer connection down.

### PCM path (fallback)

Binary frames: an 8-byte little-endian header followed by 16-bit samples.

| Offset | Type | Field |
| --- | --- | --- |
| 0 | u32 LE | sample rate (8000 to 192000) |
| 4 | u16 LE | channels (1 to 8; only the first channel is used) |
| 6 | u16 LE | sequence number (wraps) |
| 8 | i16 LE[] | interleaved samples, 10 ms per frame recommended |

Frames with an invalid header are dropped.

### Several senders

Only one connection plays at a time. A connection becomes the active source when it sends its
first PCM frame or its WebRTC offer (the newest wins). When the active connection closes, the
next connection that sends becomes active.

## `/api/stats`

```json
{
  "version": "0.1.0",
  "device": "CABLE Input (VB-Audio Virtual Cable)",
  "clients": 1,
  "mic_switched": true,
  "switching_enabled": true,
  "uptime_s": 3600,
  "frames_received": 12000,
  "samples_received": 5760000,
  "underruns": 0,
  "dropped_samples": 0,
  "buffered_ms": 41.2,
  "target_ms": 40.0,
  "ratio": 1.0002,
  "input_rate": 48000,
  "output_rate": 48000,
  "rtc": {
    "connected": true,
    "packets": 12000,
    "lost": 3,
    "fec_recovered": 3,
    "plc_frames": 0,
    "decode_errors": 0
  }
}
```

Counters are cumulative since start. `clients` counts open WebSockets.
