# xenia-peer

Application-layer crates for Xenia: a peer-to-peer, consciousness-first
remote-session stack built on top of
[`xenia-wire`](https://github.com/Luminous-Dynamics/xenia-wire)
(the PQC-sealed protocol, separate repo).

```text
  ╔═══════════════════════════════════════════════════════════════╗
  ║  PRE-ALPHA, M0 — NOT USABLE AS A PRODUCT YET                  ║
  ║                                                               ║
  ║  This milestone is the workspace scaffold: a headless daemon  ║
  ║  binary, a CLI viewer binary, and the shared library they     ║
  ║  sit on. End-to-end loopback works (sealed RawInput round-    ║
  ║  trips over real tokio TCP). What does NOT work yet: screen   ║
  ║  capture, video encode/decode, consent UI, input injection,   ║
  ║  and anything resembling a graphical interface.               ║
  ║                                                               ║
  ║  For a working remote-desktop tool today: RustDesk,           ║
  ║  MeshCentral, or Apache Guacamole.                            ║
  ╚═══════════════════════════════════════════════════════════════╝
```

## Architecture at a glance

Two peers. No relay, no central server, no trust authority. One peer
**hosts** its screen; the other peer **views** it. Both speak
`xenia-wire` underneath; both are ordinary Linux processes.

```
┌─────────────────────────────┐      xenia-wire       ┌─────────────────────────────┐
│  xenia-peer (daemon)        │ ◀───(sealed)────────▶ │  xenia-viewer (CLI/GUI)     │
│  - hosts screen             │                        │  - decodes frames           │
│  - accepts incoming viewers │                        │  - sends input events       │
│  - drives consent ceremony  │                        │  - renders (M4+)            │
│  SessionRole::Host          │                        │  SessionRole::Viewer        │
└─────────────────────────────┘                        └─────────────────────────────┘
            │                                                          │
            └─────────── shared library ──────────────────────────────┘
                              xenia-peer-core
         (Session wrapper, transport trait, raw frame/input types)
```

No "server" in the MSP / client-server sense — the repo was renamed
from `xenia-server` to `xenia-peer` in commit `a861501` precisely
because that legacy naming was actively misleading about the
decentralized trust model. See
[`docs/ADR-001-m0-architecture.md`](docs/ADR-001-m0-architecture.md)
for the full architectural decisions.

## Crate layout

Single Cargo workspace. Three crates.

| Crate | Kind | License | Purpose |
|---|---|---|---|
| [`xenia-peer-core`](crates/xenia-peer-core/) | library | **Apache-2.0 OR MIT** | Shared library: `Session`, `Transport` trait, `TcpTransport`, `RawFrame` / `RawInput`. Reusable by third-party clients. |
| [`xenia-peer`](crates/xenia-peer/) | binary (daemon) | **AGPL-3.0-or-later** | The machine sharing its screen. Listens for viewer connections, hosts the session, drives the consent UI (M2+). |
| [`xenia-viewer`](crates/xenia-viewer/) | binary (CLI → GUI at M4) | **AGPL-3.0-or-later** | The machine watching. Connects to a daemon, decodes frames, renders. Today a CLI probe tool; egui GUI lands at M4. |

**Why split the licenses?** The library stays permissive so any tool
(browser client, VS Code extension, TUI, etc.) can link against it.
The binaries are the value-add application layer — AGPL forces a
commercial-user or modify-and-distribute organization to either open
source their stack or negotiate a commercial license. The `xenia-wire`
protocol itself remains Apache-2.0 OR MIT to maximize adoption of the
wire format.

Full reasoning: [ADR-001 §Decision 3](docs/ADR-001-m0-architecture.md).

## Status

The single source of truth for what's done, blocked, and queued is
[`ROADMAP.md`](ROADMAP.md). Short version:

**Live today** — 3 codecs (passthrough / H.264 / HDC), 2
transports (TCP / WebSocket), 3 viewers (CLI / egui GUI / browser),
end-to-end testable between any two desktops and desktop→phone
browser.

**Hard blockers before real deployment** — PQC handshake (still
on fixture key), real Wayland capture (stub today), consent-
ceremony UI. See ROADMAP §"Hard blockers for real deployment".

**Still-to-carry from Symthaea** — input injection, Iroh QUIC,
clipboard, file transfer, audio, recording. See ROADMAP §"From
Symthaea: carry-wholesale backlog".

Full VIEWER_PLAN (the parent design doc) is in the sibling repo:
[`plans/VIEWER_PLAN.md`](https://github.com/Luminous-Dynamics/xenia-wire/blob/main/plans/VIEWER_PLAN.md).

## Platform policy — Wayland only

`xenia-peer` supports **Wayland-native capture and input** only.
X11 is explicitly out of scope. The X11 server's core design permits
any client to read any other client's keystrokes and screen content —
that's fundamentally incompatible with Xenia's end-to-end-encrypted
consent-gated threat model.

Supported Wayland paths:

- **wlroots compositors** (Sway, Hyprland, labwc): stable
  `wlr-screencopy-unstable-v1` + `libei` for capture + input.
- **GNOME / KDE**: `xdg-desktop-portal`'s `ScreenCast` + `RemoteDesktop`
  interfaces.

Both require an explicit consent prompt from the compositor, which
aligns with Xenia's consent-ceremony UX rather than fighting it.

X11-only systems (older Ubuntu LTS, unmaintained distros) are
unsupported targets. A community X11 backend as a separate
`xenia-peer-x11` fork would be acceptable but is not upstream.

Full reasoning: [ADR-001 §Decision 2](docs/ADR-001-m0-architecture.md).

## Quick start (developers)

### Clone + test

```console
$ git clone https://github.com/Luminous-Dynamics/xenia-peer
$ cd xenia-peer
$ cargo test --workspace
```

Expected: 23 tests pass (library unit tests + 4 real-TCP integration
tests including the 100-frame + 10-input seal/open loopback). All
under 2 seconds on a cold build.

### Run end-to-end (passthrough codec — always available)

Two terminals:

```console
# terminal 1 — host daemon, sends 30 synthetic frames
$ cargo run --release -p xenia-peer -- --listen 127.0.0.1:4747 --frames 30 --codec passthrough

# terminal 2 — viewer, verifies every frame byte-for-byte
$ cargo run --release -p xenia-viewer -- --connect 127.0.0.1:4747 --frames 30 --codec passthrough --verify
```

The viewer locally regenerates each expected frame via a mirror
`TestCapture` and asserts byte-exact equality against what it
decoded. Mismatch = pipeline broken.

### Run end-to-end (H.264 codec)

H.264 requires libav + libclang at build time. Easiest path is
the bundled Nix flake:

```console
$ nix develop                                 # inside xenia-peer/
$ cargo build --release --workspace --features "xenia-peer/h264 xenia-viewer/h264"
$ ./target/release/xenia-peer --listen 127.0.0.1:4747 --frames 30 --codec h264 --bitrate-kbps 2000 &
$ ./target/release/xenia-viewer --connect 127.0.0.1:4747 --frames 30 --codec h264
```

Outside Nix, install ffmpeg dev + llvm dev packages through your
distro (`libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
libswresample-dev libclang-dev` on Debian/Ubuntu) and rebuild with
the same `--features` flags.

On the synthetic 320×240 gradient at 30 fps the H.264 stream comes
in around 11 KB for the keyframe + ~3 KB per P-frame — ~100× smaller
than passthrough's 256 KB/frame. Real desktop content compresses
further.

## M0 exit criterion (achieved)

> `cargo test -p xenia-peer-core` green; real-TCP loopback test
> exchanges 100 RGBA frames + 10 input events through the full
> `xenia-wire` seal/open path; both binaries (`xenia-peer` and
> `xenia-viewer`) compile and complete a loopback probe exchange.

```
running 4 tests
test session_fixture_constructors_work_without_runtime ... ok
test replay_protection_across_real_transport ... ok
test oversize_envelope_is_rejected_before_allocation ... ok
test hundred_frames_plus_inputs_roundtrip_over_tcp ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured
```

Plus replay protection (duplicate envelope rejected) and oversize-
envelope safety (forged 100 MiB length prefix doesn't OOM the receiver).

## Licensing

| Layer | License |
|---|---|
| [`xenia-wire`](https://github.com/Luminous-Dynamics/xenia-wire) (protocol) | Apache-2.0 OR MIT |
| `xenia-peer-core` (library) | Apache-2.0 OR MIT |
| `xenia-peer` (daemon binary) | AGPL-3.0-or-later |
| `xenia-viewer` (viewer binary) | AGPL-3.0-or-later |

Full license texts in [`LICENSE-APACHE`](LICENSE-APACHE),
[`LICENSE-MIT`](LICENSE-MIT), and [`LICENSE-AGPL-3.0`](LICENSE-AGPL-3.0).

Dual commercial licensing for the AGPL binaries is available on
request for organizations whose policies preclude AGPL adoption —
the repository author is the sole copyright holder and can grant
exceptions on a case-by-case basis.

## Security

See
[xenia-wire's SECURITY.md](https://github.com/Luminous-Dynamics/xenia-wire/blob/main/SECURITY.md)
for the disclosure policy — `xenia-peer` inherits the same posture.
Do not report security issues via public GitHub issues.

## Relationship to Track A

[`xenia-wire`](https://github.com/Luminous-Dynamics/xenia-wire) is
the completed Track A: the wire protocol + SPEC draft-03 + paper +
demo pages, stable at `0.2.0-alpha.3`. This repo is Track B: the
actual application layer. Track A is feature-complete-pending-
external-review; Track B is signal-gated past M0.
