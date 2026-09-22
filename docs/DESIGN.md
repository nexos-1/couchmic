# Design notes

## Sender page (iPad)

The page follows Apple's Human Interface Guidelines, translated to the web:

- Colors by role with a light and a dark set (follows the system): tint for interactive
  elements, orange for "microphone live" (like the iOS microphone dot), red for errors,
  everything else neutral. Contrast: label 16:1, secondary text 4.9:1 light and 6.4:1 dark,
  white on tint 5.7:1 light and 3.6:1 dark (labels are bold, 17 pt).
- System font (SF Pro): 17 pt body, 15 pt subline, 13 pt footnotes. Large title and state word in
  SF Pro Rounded (`ui-rounded`).
- Layout: large title, then the "pane" (state word plus level; the light behind it breathes with
  the voice), grouped lists like iOS Settings, "Advanced" collapsible. The control bar floats at
  the bottom as the only glass surface (`backdrop-filter`), within thumb reach; buttons are at
  least 44 pt.
- Motion: press feedback `scale(.97)` in 160 ms, state changes as a short crossfade with blur,
  level only through transform/opacity. `prefers-reduced-motion`, `prefers-reduced-transparency`
  and `prefers-contrast` each have an answer. State is never color only: dot (filled, hollow,
  spinner) plus word.

## Why Safari and not a standalone web app

The Home Screen icon opens the page in Safari (`display: browser`, no
`apple-mobile-web-app-capable`). A Safari tab keeps recording in the background (red status bar);
iOS freezes a standalone Home Screen web app as soon as you switch to another app, which kills the
microphone. Confirmed on iPad and iPhone.

## App icon

`icon-1024.png` (scaled to 512/192/180): blue gradient with a translucent glass pane, three level
bars. No text, filled shapes, centered, no own mask (iOS rounds the corners). The PNGs are the
source.

## Audio path

- Jitter buffer target 40 ms. After an underrun or packet loss the target grows (up to 200 ms);
  after 30 s without trouble it shrinks back step by step. Above `--max-ms` (500) the oldest audio
  is dropped. Running dry after the last packet on a normal disconnect does not count as trouble.
- Clock drift between iPad and PC: the resampling ratio is adjusted by at most +-0.5 percent.
- Resampler: rubato, windowed sinc interpolation (128 taps, Blackman-Harris), runs in the
  network thread; the audio callback only copies.
- Opus loss: a gap of 1 to 9 packets is bridged by in-band FEC from the next packet (first
  missing packet) and PLC (up to 5 frames); larger gaps reset the decoder.
- Only Opus is offered. With the default codec list Chrome and Safari negotiated G.711 (PCMA).
