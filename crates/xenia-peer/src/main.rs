// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `xenia-peer` — headless daemon that hosts a Xenia session.
//!
//! **M1 scaffold.** Full capture → encode → seal → send pipeline
//! now wired end-to-end using:
//!
//! - [`xenia_capture::TestCapture`] producing synthetic RGBA frames
//!   (M1.2b swaps in real Wayland capture behind a feature flag).
//! - [`xenia_video::passthrough`] as the codec (M1.2b adds real
//!   H.264 via `ffmpeg-next`).
//! - `xenia-peer-core::Session::seal_frame` over a TCP transport.
//!
//! The daemon remains a single-viewer, single-session stub with a
//! fixture AEAD key. Multi-viewer fan-out and the ML-KEM handshake
//! land in later milestones.
//!
//! Roadmap:
//!
//! | Milestone | What lands here |
//! |---|---|
//! | **M1** (now) | TestCapture + passthrough codec + TCP send loop. |
//! | M1.2b | `xenia-video::h264` backend (ffmpeg-next libx264). |
//! | M1.2c | Wayland capture backends (wlr-screencopy + xdg-portal). |
//! | M2 | Consent ceremony flow (UI prompt on the host). |
//! | M3 | Iroh QUIC transport; WebSocket fallback. |
//! | M4 | Systemd unit, sandboxing, log rotation. |

use clap::{Parser, ValueEnum};
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{info, warn};
use xenia_capture::{ScreenCapture, TestCapture};
use xenia_peer_core::frame::PixelFormat as FramePixelFormat;
use xenia_peer_core::transport::{TcpTransport, Transport};
use xenia_peer_core::{frame::RawFrame, Session, SessionRole};
use xenia_video::passthrough::PassthroughEncoder;
use xenia_video::{EncodeParams, Encoder, PixelFormat as CodecPixelFormat};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum CodecChoice {
    /// No compression. Always available. ~RGBA*width*height bytes per frame.
    Passthrough,
    /// H.264 via libx264 (ffmpeg-next). Requires the `h264` Cargo feature
    /// at build time AND libav dev headers. In `nix develop` shells both
    /// are provided; outside Nix install ffmpeg + llvm dev packages.
    H264,
}

fn make_encoder(
    choice: CodecChoice,
    params: EncodeParams,
) -> Result<Box<dyn Encoder>, Box<dyn std::error::Error>> {
    match choice {
        CodecChoice::Passthrough => Ok(Box::new(PassthroughEncoder::new(params))),
        CodecChoice::H264 => build_h264_encoder(params),
    }
}

#[cfg(feature = "h264")]
fn build_h264_encoder(
    params: EncodeParams,
) -> Result<Box<dyn Encoder>, Box<dyn std::error::Error>> {
    let enc = xenia_video::h264::H264Encoder::new(params)?;
    Ok(Box::new(enc))
}

#[cfg(not(feature = "h264"))]
fn build_h264_encoder(
    _params: EncodeParams,
) -> Result<Box<dyn Encoder>, Box<dyn std::error::Error>> {
    Err("xenia-peer was built without the `h264` feature; rebuild inside `nix develop` with `cargo build -p xenia-peer --features h264`, or use --codec passthrough".into())
}

fn codec_to_frame_format(choice: CodecChoice) -> FramePixelFormat {
    match choice {
        CodecChoice::Passthrough => FramePixelFormat::Passthrough,
        CodecChoice::H264 => FramePixelFormat::H264,
    }
}

/// Dev fixture key. M2 replaces with handshake-derived session key.
const FIXTURE_KEY: [u8; 32] = *b"xenia-peer-m0-stub-fixture-key!!";

#[derive(Parser, Debug)]
#[command(name = "xenia-peer", version, about = "Host daemon for Xenia sessions")]
struct Args {
    /// Address to listen on for incoming viewer connections.
    #[arg(long, default_value = "127.0.0.1:4747")]
    listen: String,

    /// Fixed source_id for the session (hex; exactly 16 chars). Dev
    /// fixture; production sessions randomize per-session.
    #[arg(long, default_value = "7878656e69617068")]
    source_id_hex: String,

    /// Fixed epoch byte.
    #[arg(long, default_value_t = 0x01)]
    epoch: u8,

    /// Capture width in pixels.
    #[arg(long, default_value_t = 320)]
    width: u32,

    /// Capture height in pixels.
    #[arg(long, default_value_t = 200)]
    height: u32,

    /// Target capture frame rate (frames per second).
    #[arg(long, default_value_t = 10)]
    fps: u32,

    /// Number of frames to send before exiting. 0 = run
    /// indefinitely (until viewer disconnects).
    #[arg(long, default_value_t = 30)]
    frames: u64,

    /// Codec to use. `passthrough` is always available; `h264`
    /// requires `--features h264` at build time + libav dev
    /// headers (enable with `nix develop`).
    #[arg(long, value_enum, default_value_t = CodecChoice::Passthrough)]
    codec: CodecChoice,

    /// Target H.264 bitrate in kilobits per second. Ignored for
    /// the passthrough codec.
    #[arg(long, default_value_t = 3000)]
    bitrate_kbps: u32,
}

fn parse_source_id(hex: &str) -> Result<[u8; 8], String> {
    if hex.len() != 16 {
        return Err(format!("source_id must be 16 hex chars, got {}", hex.len()));
    }
    let mut out = [0u8; 8];
    for i in 0..8 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("source_id hex[{i}]: {e}"))?;
    }
    Ok(out)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let source_id = parse_source_id(&args.source_id_hex)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let listener = TcpListener::bind(&args.listen).await?;
    let local = listener.local_addr()?;
    info!(addr = %local, width = args.width, height = args.height, fps = args.fps, codec = ?args.codec, "xenia-peer daemon listening");
    warn!("M1 scaffold: fixture key, TestCapture synthetic frames. See ADR-001.");

    let (stream, peer) = listener.accept().await?;
    stream.set_nodelay(true).ok();
    info!(peer = %peer, "viewer connected");
    let mut transport = TcpTransport::new(stream);

    let mut session = Session::with_fixture(SessionRole::Host, source_id, args.epoch);
    session.install_key(FIXTURE_KEY);

    let mut capture = TestCapture::new(args.width, args.height);
    let encode_params = EncodeParams {
        width: args.width,
        height: args.height,
        pixel_format: CodecPixelFormat::Rgba,
        target_fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
    };
    let mut encoder = make_encoder(args.codec, encode_params)?;
    let frame_fmt = codec_to_frame_format(args.codec);
    info!(codec = ?args.codec, "encoder ready");

    let frame_interval = if args.fps > 0 {
        Duration::from_micros(1_000_000 / args.fps as u64)
    } else {
        Duration::from_millis(0)
    };
    let mut ticker = tokio::time::interval(frame_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut sent: u64 = 0;
    loop {
        if args.frames != 0 && sent >= args.frames {
            info!(sent, "reached --frames, exiting");
            break;
        }
        ticker.tick().await;

        let Ok(Some(captured)) = capture.capture() else {
            warn!("capture produced no frame; exiting");
            break;
        };
        let pts = now_ms();
        let packets = match encoder.encode(&captured.pixels, pts) {
            Ok(p) => p,
            Err(err) => {
                warn!(error = %err, "encode failed");
                continue;
            }
        };
        for packet in packets {
            let raw_frame = RawFrame::encoded(
                sent,
                pts,
                captured.width,
                captured.height,
                frame_fmt,
                packet.bytes,
            );
            let envelope = match session.seal_frame(&raw_frame) {
                Ok(e) => e,
                Err(err) => {
                    warn!(error = %err, "seal_frame failed");
                    continue;
                }
            };
            if let Err(err) = transport.send_envelope(&envelope).await {
                info!(error = %err, sent, "transport closed, exiting");
                return Ok(());
            }
            sent += 1;
            if sent % 10 == 0 || sent == 1 {
                info!(sent, bytes = raw_frame.pixels.len(), "frame sent");
            }
        }
    }

    info!(sent, "daemon exiting");
    Ok(())
}
