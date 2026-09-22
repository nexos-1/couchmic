# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/),
versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0] - unreleased

First public version.

- Safari on iPad/iPhone sends the microphone over WebRTC (Opus, in-band FEC, PLC) with a PCM over
  WebSocket fallback; glass-mic plays it into VB-CABLE.
- Adaptive jitter buffer with drift compensation (sinc resampler).
- Switches the Windows default microphone to CABLE Output while connected and restores it
  afterwards, also after a crash (`--restore-mic`).
- Access control: host allowlist (DNS rebinding) and origin check (cross-site WebSockets).
- One active audio source at a time; the newest sender takes over.
- Tray icon, Windows notifications under their own app id, German and English UI.
- Log file with rotation at 5 MB; stats are only logged while a client is connected.
- `install.ps1` for install, update and uninstall (scheduled task with watchdog, firewall rule,
  `tailscale serve`).
- `/api/stats` for integrations.
