# CouchMic

Use your iPad or iPhone as a microphone for your Windows PC. No app to install on the iPad:
Safari sends the microphone over WebRTC through your [Tailscale](https://tailscale.com) network,
CouchMic plays it into [VB-CABLE](https://vb-audio.com/Cable/), and every Windows app can use
"CABLE Output" as its microphone.

Built for remote desktop setups (Moonlight/Sunshine, Jump Desktop, Parsec) where the iPad is the
screen and keyboard, but the PC has no microphone of its own or it is in another room.

[Deutsche Version](README.de.md)

```
iPad / iPhone (Safari)                          Windows PC
+----------------------+   Tailscale (WireGuard)  +--------------------------------------+
| getUserMedia         |  WebRTC, Opus, UDP 8322  | couchmic.exe                         |
| echo/noise cancel    | -----------------------> |  Opus decode, FEC/PLC, jitter buffer |
| web page from the PC | <---- HTTPS (signaling)  |  -> "CABLE Input" (VB-CABLE)         |
+----------------------+   tailscale serve :443   |  -> "CABLE Output" = microphone      |
                                                  +--------------------------------------+
```

## Features

- Opus over WebRTC with in-band FEC and packet loss concealment; PCM over WebSocket as a fallback.
- Adaptive jitter buffer (40 ms target, grows to 200 ms on a bad network, shrinks back), clock
  drift compensation with a sinc resampler, no clicks.
- Switches the Windows default microphone to CABLE Output while you are connected and back
  afterwards (also after a crash).
- Keeps recording while Safari is in the background (red status bar on iOS).
- Tray icon with status, Windows notifications, German and English UI.
- Only one device plays at a time: start sending on the iPhone and it takes over from the iPad.
- Local stats endpoint (`/api/stats`) for scripts and other tools.

## Requirements

- Windows 11 x64 (tested). Windows 10 x64 should work, but is untested.
- [VB-CABLE](https://vb-audio.com/Cable/) (free, donationware). Install it and reboot.
- [Tailscale](https://tailscale.com/download) on the PC and on the iPad/iPhone, signed in to the
  same tailnet. In the [Tailscale admin console](https://login.tailscale.com/admin/dns) enable
  **MagicDNS** and **HTTPS certificates**. Safari only allows the microphone on HTTPS pages, and
  `tailscale serve` provides that certificate.
- iPad or iPhone with Safari (tested with current iPadOS and iOS).

## Install

1. Download `couchmic-<version>-windows-x64.zip` from the
   [releases](https://github.com/nexos-1/couchmic/releases) and unzip it.
2. Open PowerShell in that folder and run:
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```
   The script checks for VB-CABLE, copies CouchMic to `%LOCALAPPDATA%\CouchMic`, adds a firewall
   rule for UDP 8322 (one admin prompt), registers a scheduled task that starts CouchMic at logon
   and restarts it if needed, and runs `tailscale serve`. At the end it prints the address for
   the iPad, e.g. `https://my-pc.tail1234.ts.net/`.
3. On the iPad: open that address in Safari, tap **Start microphone**, allow microphone access.
4. Optional: Share, **Add to Home Screen**. The icon opens the page in Safari (on purpose: iOS
   freezes standalone web apps in the background, a Safari tab keeps recording).

Update: download the new zip and run `install.ps1` again.
Uninstall: `powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall` (restores the
previous default microphone, removes the task, the firewall rule and `tailscale serve`; VB-CABLE
and Tailscale stay installed).

The binary is not code-signed yet, so Windows SmartScreen may warn on first start ("More info",
"Run anyway"). Check the SHA-256 from the release page if in doubt.

## Use

- Start the microphone on the iPad, then speak: "CABLE Output" is now the default microphone.
  Apps that let you pick a microphone can also select "CABLE Output" directly.
- Automation with Shortcuts (iPad/iPhone): "When app is opened" (e.g. Moonlight), action "Open
  URL" `https://<pc>.<tailnet>.ts.net/?autostart=1`. "When app is closed", "Open URL"
  `https://<pc>.<tailnet>.ts.net/?stop=1` stops the recording in the running tab.
- The **Advanced** section on the page shows path, latency, buffer, loss and underruns.
- Test without speaking: `https://<pc>.<tailnet>.ts.net/?tone=1` sends a 440 Hz tone.
- Tray menu: status, "Switch default microphone automatically" (on/off), open web page, open log
  folder, quit. After "Quit" the watchdog leaves CouchMic stopped until you sign out, restart
  or shut down Windows (or until you start `couchmic.exe` by hand).

## Command line

`couchmic.exe --help` lists all options. The most useful:

| Option | Default | Meaning |
| --- | --- | --- |
| `--device <name>` | `CABLE Input` | Output device (substring). `default` = Windows default output |
| `--mic-filter <name>` | `CABLE Output` | Recording device that becomes the default microphone |
| `--no-switch` | off | Do not change the default microphone |
| `--port <n>` | `8321` | HTTP port on 127.0.0.1 (behind `tailscale serve`) |
| `--rtc-port <n>` | `8322` | UDP port for WebRTC (needs the firewall rule) |
| `--target-ms <ms>` | `40` | Jitter buffer target |
| `--allow-origin <url>` | | Allow another origin, e.g. your own HTTPS reverse proxy |
| `--allow-any-tailnet-user` | off | Accept every tailnet user, not only the PC's owner |
| `--no-tray`, `--no-toast` | off | No tray icon, no notifications |
| `--log-file <path>` | see below | Log file (rotated at 5 MB) |
| `--list` | | List output devices |
| `--restore-mic` | | Restore the previous default microphone after a crash, then exit |

Logs: `%LOCALAPPDATA%\CouchMic\couchmic.log` (the tray menu opens the folder).

## Integrations

`GET http://127.0.0.1:8321/api/stats` returns JSON with `clients`, `mic_switched`,
`buffered_ms`, `underruns`, `rtc.packets`, `rtc.lost` and more. A dictation tool can, for
example, check `clients >= 1` and then record from "CABLE Output". [LocalFlow](https://github.com/nexos-1/localflow)
does exactly that. The WebSocket protocol is documented in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Security and privacy

- The web server (page, WebSocket, `/api/stats`) only listens on `127.0.0.1`. Other devices
  reach it only through `tailscale serve`, that is, only from your own tailnet.
- Only your own Tailscale user gets in: requests forwarded by `tailscale serve` must carry the
  `Tailscale-User-Login` of the PC's owner (Tailscale sets and protects that header). Other
  users of a shared tailnet and tagged devices are rejected; `--allow-any-tailnet-user`
  turns this off.
- **Tailscale Funnel is always rejected.** Funnel works per port, so turning it on for port 443
  for anything else would otherwise publish CouchMic to the internet; `install.ps1` also refuses
  to set up while Funnel is on for that port.
- Browsers must come from the CouchMic page itself: host allowlist (against DNS rebinding),
  origin check (against cross-site WebSockets), `Sec-Fetch-Site` (against links and embeds from
  other sites) and `frame-ancestors 'none'` (the page cannot be framed). Rejected requests get
  403 and a rate-limited warning in the log.
- WebRTC audio uses UDP 8322 while a session runs. The firewall rule from `install.ps1` allows it
  only from Tailscale addresses (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), so audio always runs
  inside the WireGuard tunnel and is additionally DTLS-SRTP encrypted. ICE and DTLS only accept
  the peer that did the offer/answer exchange over the protected WebSocket.
- Limits against misuse: messages up to 64 KiB, 4 connections, audio blocks up to 100 ms, one
  active audio source, idle connections closed after 2 minutes, client log messages cut and
  rate-limited, log files rotated at 5 MB.
- There is no account and no cloud. Audio goes from the iPad to your PC and nowhere else. The
  page sends a few status events (visibility, connection state, browser user agent) to the PC;
  they only end up in the local log file.

Report security issues as described in [SECURITY.md](SECURITY.md).

## Troubleshooting

- **White page on the iPad:** CouchMic is not running (tailscale serve answers 502). The
  scheduled task restarts it within 5 minutes; or start the "CouchMic" task manually.
- **"PC not reachable":** Tailscale disconnected on the iPad, or HTTPS certificates are not
  enabled in the Tailscale admin console.
- **Path shows "PCM" instead of "WebRTC":** UDP 8322 is blocked (firewall rule missing or a
  third-party firewall). PCM works, but uses more bandwidth and has no loss concealment.
- **Muffled voice:** a Bluetooth headset is connected to the iPad; iOS switches it to the
  narrowband phone profile. Use the built-in microphone or a wired headset.
- **Tray icon missing:** Windows 11 hides new icons in the overflow; pin it in the taskbar
  settings.

## Build from source

Needs Rust 1.95 (see `rust-toolchain.toml`) and CMake (for libopus):

```powershell
cargo build --release
.\install.ps1   # picks up target\release\couchmic.exe
```

`cargo test` runs the unit tests; on Linux/macOS only a stub binary is built, and the platform
independent modules (access control, jitter buffer, texts, log rotation) are tested there too.
See [CONTRIBUTING.md](CONTRIBUTING.md).

## Limitations

- Windows only (the receiver). The sender is any Safari on iOS/iPadOS; other browsers work on a
  desktop for testing.
- Switching the default microphone uses the undocumented `IPolicyConfig` COM interface (the same
  one the Windows sound settings use). If Microsoft changes it, only switching stops working.
- One audio source at a time, mono, 48 kHz.

## License

MIT, see [LICENSE](LICENSE). Release binaries include third-party code (libopus BSD-3-Clause,
Rust crates under MIT/Apache-2.0/BSD/ISC); their notices ship as `THIRD-PARTY-LICENSES.html`.
VB-CABLE is a separate product by VB-Audio and is not included. Tailscale is a trademark of
Tailscale Inc.; iPad, iPhone and Safari are trademarks of Apple Inc.; Windows is a trademark of
Microsoft Corporation. CouchMic is not affiliated with any of them.
