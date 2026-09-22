# Security

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private reporting instead:
**Security** tab of this repository, **Report a vulnerability**. Please include the glass-mic
version (`glass-mic.exe --version`), Windows version and steps to reproduce.

Only the latest release is supported with fixes.

## Threat model

glass-mic receives audio from your own devices and plays it into a virtual microphone on your
PC. The relevant risks are someone else injecting audio into that microphone, switching your
default microphone, or reading the status endpoint.

What protects it:

- **Network:** the HTTP server binds to `127.0.0.1` only. Remote access exists only through
  `tailscale serve`, so only devices in your tailnet can reach it. Tailscale encrypts the traffic
  (WireGuard); WebRTC audio is additionally DTLS-SRTP encrypted.
- **Browsers:** every request must carry an allowed Host header (loopback, the PC's Tailscale
  name, or `--allow-origin` entries) against DNS rebinding, and a browser Origin must be the
  glass-mic page itself, against cross-site WebSockets from other web pages. Violations get 403.
- **WebRTC:** the UDP port only accepts ICE/DTLS peers that completed the offer/answer exchange
  over the protected WebSocket.
- **Input validation:** PCM headers with invalid sample rates or channel counts are dropped.
- **No secrets:** there are no accounts, tokens or stored credentials.

Out of scope: other users of your own tailnet (restrict them with Tailscale ACLs), and malware
already running on the PC (it can use the microphone directly anyway).

## Supply chain

- Dependencies are pinned in `Cargo.lock`; new versions are only adopted once they are at least
  14 days old. `cargo deny` (licenses, advisories, sources) runs in CI.
- GitHub Actions are pinned to commit SHAs.
- Release binaries are built by GitHub Actions from the tagged commit; the release lists their
  SHA-256. They are not code-signed yet.
- Known advisories that are not reachable in glass-mic are documented with a reason in
  `deny.toml`.
