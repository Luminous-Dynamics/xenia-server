# Changelog

All notable changes to `xenia-server` and its sub-crates are documented
here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.0.0-m0] — 2026-04-18

Initial milestone. Workspace scaffold + `xenia-server-core` crate with
the minimum necessary to exchange sealed-RGBA-over-TCP on localhost.
**Not published to crates.io** — the crate is `publish = false` until
at least M1 makes it useful.

### Added

- `xenia-server-core` crate:
  - `RawFrame` + `RawInput` framing types (raw RGBA only; encoded
    formats reserved).
  - `Session` thin wrapper around `xenia_wire::Session` with
    server/viewer role tracking and monotonic frame/input counters.
  - `Transport` trait + `TcpTransport` implementation
    (length-prefixed envelopes over `tokio::net::TcpStream`).
  - Integration test `hundred_frames_plus_inputs_roundtrip_over_tcp`
    exchanging 100 frames + 10 inputs end-to-end through real
    tokio TCP sockets.
  - Tests for replay protection across the real transport and for
    oversize-envelope safety (16 MiB cap).

### Design decisions locked in

- Codec: H.264 primary / VP9 fallback / HEVC bonus.
- Codec wrapper: `ffmpeg-next`.
- Capture: per-platform behind `xenia-capture` trait.
- Input: per-platform behind `xenia-inject` trait.
- Transports: Iroh QUIC primary + `tokio-tungstenite` WebSocket
  fallback.
- Browser viewer: WebCodecs + WebGL2.
- Native viewer: egui → Tauri progression.

### Deviations from plan

- `plans/VIEWER_PLAN.md` §3 M0 originally framed as an 11-file
  extraction from Symthaea's `rdp_*.rs`. Actual execution: fresh
  code on top of `xenia-wire`, with Symthaea's rdp_* as reference
  material. Result: smaller + cleaner than the extraction path
  would have produced.

### Known limitations

- No screen capture.
- No video encoding.
- No input injection.
- No QUIC transport (TCP-only for M0).
- No WebSocket transport.
- No browser client beyond what `xenia-viewer-web` in the
  `xenia-wire` repo already provides (demo-quality).

[Unreleased]: https://github.com/Luminous-Dynamics/xenia-server/compare/v0.0.0-m0...HEAD
[0.0.0-m0]: https://github.com/Luminous-Dynamics/xenia-server/releases/tag/v0.0.0-m0
