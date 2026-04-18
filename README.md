# xenia-server

Server + viewer crates for a real remote-desktop product built on top
of the [xenia-wire](https://github.com/Luminous-Dynamics/xenia-wire)
PQC-sealed protocol.

```text
  ╔═══════════════════════════════════════════════════════════════╗
  ║  PRE-ALPHA, M0 — NOT USABLE AS A PRODUCT YET                  ║
  ║                                                               ║
  ║  This milestone is the workspace scaffold + a clean extraction║
  ║  target. No real screen capture, no video encoding, no input  ║
  ║  injection yet. If you're looking for a working remote-       ║
  ║  desktop tool TODAY, RustDesk or Apache Guacamole are the     ║
  ║  mature options.                                              ║
  ║                                                               ║
  ║  What does work: the `xenia-wire` seal/open path composes     ║
  ║  correctly with raw-RGBA framing over real tokio TCP sockets. ║
  ║  That's the M0 bar, nothing more.                             ║
  ╚═══════════════════════════════════════════════════════════════╝
```

## Status

| Milestone | Status | Deliverable |
|-----------|--------|-------------|
| **M0** | ✅ shipped | Workspace scaffold + `xenia-server-core` crate + real-TCP loopback integration test exchanging 100 RGBA frames. |
| M1 | 🚧 not started | Linux screen capture (X11 + Wayland via portal) + H.264 hardware encode via `ffmpeg-next` + browser WebCodecs decode. 2–3 weeks. |
| M2 | not started | Input injection + consent-flow wiring (Linux first). 1–2 weeks. |
| M3 | not started | macOS + Windows capture/encode/input. 4–6 weeks. |
| M4 | not started | Native viewer, cursor sync, clipboard, file transfer, audio, unattended-access, audit log, session recording, installer signing. 3–4 months. |
| M5+ | speculative | Attended-support flow, MSP admin console, PSA integrations, mobile viewer. |

Full plan: [`plans/VIEWER_PLAN.md`](https://github.com/Luminous-Dynamics/xenia-wire/blob/main/plans/VIEWER_PLAN.md)
in the `xenia-wire` repo (that's where the Track-A-through-Track-B
planning lives).

## Crate layout

```
xenia-server/
├── Cargo.toml                              # workspace
└── crates/
    └── xenia-server-core/                  # this is M0
        ├── src/
        │   ├── lib.rs                      # re-exports
        │   ├── frame.rs                    # RawFrame / RawInput / PixelFormat
        │   ├── session.rs                  # Session (thin wrapper on xenia_wire::Session)
        │   └── transport.rs                # Transport trait + TcpTransport
        └── tests/
            └── loopback.rs                 # integration tests
```

Planned sub-crates for later milestones:

- `xenia-capture` (M1) — screen capture abstraction + per-platform
  backends (x11rb, lamco-pipewire, screencapturekit, windows-capture).
- `xenia-inject` (M2) — input injection abstraction + per-platform
  backends.
- `xenia-video` (M1) — H.264/VP9 encode/decode via ffmpeg-next.
- `xenia-transport-quic` (M1+) — QUIC transport (iroh or quinn).
- `xenia-transport-ws` (M1+) — WebSocket transport for browser
  viewer compatibility.
- `xenia-server` (M4) — the binary.
- `xenia-viewer-native` (M4) — native Rust viewer (egui → Tauri).

## Design decisions locked in

Per `plans/VIEWER_PLAN.md` §4, these are pre-committed and not
intended to be re-litigated per milestone:

- **Codec**: H.264 primary, VP9 fallback, HEVC bonus. Not AV1.
- **Codec wrapper**: `ffmpeg-next` (heavy dep, but mature and
  hardware-everywhere).
- **Capture**: per-platform crates behind a `xenia-capture` trait.
  Not `scap` as a hard dep.
- **Input injection**: per-platform backends. Not `enigo` as the
  production abstraction.
- **Transports**: Iroh QUIC primary + tokio-tungstenite WebSocket
  fallback. Not custom KCP-style reliable-UDP.
- **Browser viewer**: WebCodecs + WebGL2. Not Canvas 2D past 720p.
- **Native viewer**: egui for MVP → Tauri for product.
- **Handshake**: initial releases ship with caller-supplied 32-byte
  keys (same as `xenia-wire`); full ML-KEM-768 + Ed25519 handshake
  extraction is a separate crate.

## Wayland posture

Wayland screen capture and input injection both require user
consent via `xdg-desktop-portal` — this is non-suppressible by
design. **Our policy**:

- **Attended-support sessions** (technician + end-user present):
  portal prompt is the correct UX and aligns with the
  consent-ceremony philosophy Xenia pitches.
- **Unattended-access sessions** on Wayland: **not supported** on
  GNOME/KDE by design. Fall back to X11 or use `libei` on
  wlroots-based compositors (Sway, Hyprland).
- libei detection at runtime gives a clean path on wlroots when
  present.

## Quick start (developers)

```console
$ git clone https://github.com/Luminous-Dynamics/xenia-server
$ cd xenia-server
$ cargo test
```

Expected: 9 unit + 4 integration tests pass in under 1 second.
Including the M0 exit-criterion test
`hundred_frames_plus_inputs_roundtrip_over_tcp` which spins up real
tokio TCP listeners on localhost and exchanges 100 RGBA frames +
10 input events through the full xenia-wire seal/open path.

There is no binary to run yet — M0 is library-only.

## M0 exit criterion (achieved)

> `cargo test -p xenia-server-core` green, and a `RawServer`/
> `RawViewer` integration test exchanges 100 RGBA frames through
> the sealed wire over [a real network transport] on localhost.

Concretely:

```
running 4 tests
test session_fixture_constructors_work_without_runtime ... ok
test replay_protection_across_real_transport ... ok
test oversize_envelope_is_rejected_before_allocation ... ok
test hundred_frames_plus_inputs_roundtrip_over_tcp ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured
```

Plus replay protection (second delivery of the same envelope bytes
is rejected) and oversize-envelope safety (forged 100 MiB length
prefix doesn't OOM the receiver).

**M0 deviation from the plan**: `plans/VIEWER_PLAN.md` §3 originally
framed M0 as extracting 11 `rdp_*.rs` modules from the Symthaea
research monorepo. On execution, `xenia-wire 0.1.0-alpha.3` already
owns every load-bearing primitive (AEAD, replay, consent, key
lifecycle). The clean M0 is therefore fresh code that depends on
`xenia-wire` and treats Symthaea's rdp_* as reference material
rather than copy-paste targets. Result: a few hundred lines of
clean code instead of thousands of extracted-then-fixed-up lines.
Symthaea's in-tree rdp_* work — HDC tile codec, content
classification, consciousness-gated telemetry — stays in Symthaea
where it belongs.

## License

Dual-licensed under Apache-2.0 OR MIT, matching
[`xenia-wire`](https://github.com/Luminous-Dynamics/xenia-wire).

## Security

See [xenia-wire's SECURITY.md](https://github.com/Luminous-Dynamics/xenia-wire/blob/main/SECURITY.md)
for the disclosure policy — xenia-server inherits the same
posture. Do not report security issues via public GitHub issues.

## Relationship to Track A

[`xenia-wire`](https://github.com/Luminous-Dynamics/xenia-wire) is
the completed Track A: the wire protocol + spec + paper + demo
pages. This repo is Track B: the actual remote-desktop product.
Track A is feature-complete-pending-external-review. Track B is
signal-gated past M0 — the cheap M0 is worth doing before external
signal, but M1 and beyond should follow genuine demand.
