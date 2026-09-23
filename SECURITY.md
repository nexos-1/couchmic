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

- **Network:** the HTTP server (page, WebSocket, stats) binds to `127.0.0.1` only. Remote access
  exists only through `tailscale serve`, so only devices in your tailnet can reach it.
- **Tailscale identity:** requests forwarded by `tailscale serve` (recognised by
  `X-Forwarded-Host`, which serve always sets) must carry the `Tailscale-User-Login` of the
  PC's own Tailscale user. Tailscale deletes client-supplied copies of these headers and sets
  them itself. Other tailnet users and tagged devices are rejected unless
  `--allow-any-tailnet-user` is given.
- **Funnel:** requests carrying `Tailscale-Funnel-Request` (set by Tailscale for traffic from the
  internet) are always rejected. `install.ps1` refuses to configure `tailscale serve` while Funnel
  is enabled for the port.
- **Browsers:** allowed Host header (loopback, the PC's Tailscale name, `--allow-origin`) against
  DNS rebinding; a present Origin must be readable and be the glass-mic page itself, against
  cross-site WebSockets; `Sec-Fetch-Site` must be `none` or `same-origin`, against links and
  embeds from other sites; `frame-ancestors 'none'` and `X-Frame-Options: DENY` against framing.
- **WebRTC:** UDP 8322 is only bound while a session runs. The firewall rule from `install.ps1`
  only allows Tailscale addresses (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`). ICE checks the
  ufrag/password (MESSAGE-INTEGRITY) and DTLS checks the certificate fingerprint from the SDP,
  so only the peer that did the offer/answer over the protected WebSocket is accepted; RTP
  is SRTP-authenticated.
- **Resource limits:** WebSocket messages up to 64 KiB, JSON control messages up to 16 KiB,
  4 concurrent connections, PCM blocks up to 100 ms at a fixed rate per connection, one active
  audio source (taken over only by a connection that actually delivers audio), idle
  connections closed after 2 minutes.
- **Logs:** client telemetry is reduced to a list of known fields, cut and escaped (no forged
  lines), and rate-limited; rejected requests are logged with cut values and rate-limited; the
  log file rotates at 5 MB while running.
- **Microphone state:** the previous default microphone is saved before switching and restored on
  disconnect, on quit, after a crash (next start) and on uninstall (`--restore-mic`); a manual
  change by the user during a session is kept.
- **No secrets:** there are no accounts, tokens or stored credentials.

Out of scope: malware already running on the PC as your user (it can use the microphone and
VB-CABLE directly anyway), and people you deliberately let in with `--allow-any-tailnet-user`
or `--allow-origin`.

## Supply chain

- Dependencies are pinned in `Cargo.lock`; new versions are only adopted once they are at least
  14 days old. `cargo deny` (licenses, advisories, sources) runs in CI.
- GitHub Actions are pinned to commit SHAs; no job keeps git credentials after checkout.
- Release binaries are built by GitHub Actions from the tagged commit in a job with a read-only
  token and without caches; a separate job creates a build provenance attestation (verify with
  `gh attestation verify glass-mic.exe -R nexos-1/glass-mic`) and another one drafts the release.
  The release lists the SHA-256 of the zip and the exe. The binary is not code-signed yet.
- Known advisories that are not reachable in glass-mic are documented with a reason in
  `deny.toml`.
