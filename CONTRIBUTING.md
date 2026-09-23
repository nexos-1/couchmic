# Contributing

Thanks for helping. Issues and pull requests are welcome; for larger changes please open an
issue first so we can agree on the approach.

## Build and test

- Rust 1.95 (`rust-toolchain.toml` selects it) and CMake (libopus is built from source).
- `cargo build --release`, `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check`. CI runs these on Windows and the platform independent tests on Linux.
- `cargo deny check` for licenses, advisories and sources.
- Try it end to end: run `target\release\glass-mic.exe --no-switch --port 8391 --rtc-port 8392`
  and open `http://127.0.0.1:8391/?tone=1` in Chrome or Edge (a test tone instead of the
  microphone), or use your iPad through `tailscale serve`. When Windows asks whether to allow
  the development build through the firewall, choose **Cancel**: that prompt creates broad
  rules for all ports (the installed copy gets a narrow rule from `install.ps1`).

## Rules

- **Dependencies:** no version that was published less than 14 days ago (supply-chain cooling
  off). Mention the version and its publish date in the pull request. Only licenses from the
  allowlist in `deny.toml`.
- **Scope:** glass-mic does one thing (iPad microphone into a Windows microphone). Features that
  keep it small and dependable are preferred over new targets.
- **Protocol:** changes to the WebSocket or PCM format update `docs/PROTOCOL.md` in the same pull
  request, and old pages must keep working with a new receiver where possible.
- **Texts:** user-facing strings exist in German and English (`src/i18n.rs` for the PC,
  the `I18N` table in `src/web/index.html` for the page). Code, comments, logs and docs are in
  English.
- **Style:** `cargo fmt`, no warnings. Use a plain hyphen "-" instead of long dashes in texts.

## License

By contributing you agree that your contribution is licensed under the MIT license of this
project.
