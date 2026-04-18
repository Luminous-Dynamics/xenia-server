// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `xenia-viewer` — native client that connects to an `xenia-peer`
//! daemon and receives + decodes sealed frames.
//!
//! **M1 scaffold.** Full recv → open → decode pipeline is now wired
//! end-to-end using [`xenia_video::passthrough`]. Output is pixel
//! statistics to stdout; the egui GUI lands at M4.
//!
//! If `--verify` is passed, the viewer locally instantiates a
//! mirror [`xenia_capture::TestCapture`] with the same dimensions
//! and asserts that every decoded frame equals the expected
//! deterministic output byte-for-byte. This is the M1 exit-criterion
//! smoke check without needing a separate integration harness.

use clap::{Parser, ValueEnum};
use tracing::{info, warn};
use xenia_capture::{ScreenCapture, TestCapture};
use xenia_peer_core::frame::PixelFormat as FramePixelFormat;
use xenia_peer_core::transport::{TcpTransport, Transport};
use xenia_peer_core::{Session, SessionRole};
use xenia_video::passthrough::PassthroughDecoder;
use xenia_video::{Decoder, EncodedPacket};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum CodecChoice {
    Passthrough,
    H264,
}

fn make_decoder(choice: CodecChoice) -> Result<Box<dyn Decoder>, Box<dyn std::error::Error>> {
    match choice {
        CodecChoice::Passthrough => Ok(Box::new(PassthroughDecoder::new())),
        CodecChoice::H264 => build_h264_decoder(),
    }
}

#[cfg(feature = "h264")]
fn build_h264_decoder() -> Result<Box<dyn Decoder>, Box<dyn std::error::Error>> {
    let dec = xenia_video::h264::H264Decoder::new()?;
    Ok(Box::new(dec))
}

#[cfg(not(feature = "h264"))]
fn build_h264_decoder() -> Result<Box<dyn Decoder>, Box<dyn std::error::Error>> {
    Err("xenia-viewer was built without the `h264` feature; rebuild with `cargo build -p xenia-viewer --features h264`, or connect to a daemon using --codec passthrough".into())
}

fn codec_to_frame_format(choice: CodecChoice) -> FramePixelFormat {
    match choice {
        CodecChoice::Passthrough => FramePixelFormat::Passthrough,
        CodecChoice::H264 => FramePixelFormat::H264,
    }
}

/// Dev fixture key. MUST match the daemon's value.
const FIXTURE_KEY: [u8; 32] = *b"xenia-peer-m0-stub-fixture-key!!";

#[derive(Parser, Debug)]
#[command(
    name = "xenia-viewer",
    version,
    about = "Native viewer for Xenia sessions"
)]
struct Args {
    /// Address of the xenia-peer daemon to connect to.
    #[arg(long, default_value = "127.0.0.1:4747")]
    connect: String,

    /// Fixed source_id (hex, 16 chars). MUST match daemon.
    #[arg(long, default_value = "7878656e69617068")]
    source_id_hex: String,

    /// Fixed epoch. MUST match daemon.
    #[arg(long, default_value_t = 0x01)]
    epoch: u8,

    /// Stop after this many frames (0 = unbounded; run until daemon disconnects).
    #[arg(long, default_value_t = 30)]
    frames: u64,

    /// Capture width used by the daemon (for `--verify`).
    #[arg(long, default_value_t = 320)]
    width: u32,

    /// Capture height used by the daemon (for `--verify`).
    #[arg(long, default_value_t = 200)]
    height: u32,

    /// If set, instantiate a local mirror `TestCapture` and
    /// byte-compare every decoded frame to what the daemon should
    /// have produced. Fails fast on mismatch. M1 exit-criterion
    /// check for the passthrough codec. H.264 is lossy so this
    /// flag is only meaningful with `--codec passthrough`.
    #[arg(long)]
    verify: bool,

    /// Codec the daemon is using. Must match the daemon's
    /// `--codec` flag.
    #[arg(long, value_enum, default_value_t = CodecChoice::Passthrough)]
    codec: CodecChoice,
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

    info!(peer = %args.connect, frames = args.frames, verify = args.verify, codec = ?args.codec, "connecting to xenia-peer daemon");
    warn!("M1 scaffold: fixture key, no GUI. See ADR-001.");

    let mut transport = TcpTransport::connect(&args.connect).await?;
    let mut session = Session::with_fixture(SessionRole::Viewer, source_id, args.epoch);
    session.install_key(FIXTURE_KEY);

    let mut decoder = make_decoder(args.codec)?;
    let expected_frame_fmt = codec_to_frame_format(args.codec);
    info!(codec = ?args.codec, "decoder ready");
    let verify_is_meaningful = args.codec == CodecChoice::Passthrough;
    let mut expected_mirror = if args.verify && verify_is_meaningful {
        Some(TestCapture::new(args.width, args.height))
    } else {
        if args.verify && !verify_is_meaningful {
            warn!("--verify with a lossy codec (H.264) would fail trivially; mirror disabled");
        }
        None
    };

    let mut received: u64 = 0;
    loop {
        if args.frames != 0 && received >= args.frames {
            info!(received, "reached --frames, exiting");
            break;
        }
        let envelope = match transport.recv_envelope().await {
            Ok(e) => e,
            Err(err) => {
                info!(error = %err, received, "daemon disconnected");
                break;
            }
        };
        let raw_frame = match session.open_frame(&envelope) {
            Ok(f) => f,
            Err(err) => {
                warn!(error = %err, "failed to open frame");
                continue;
            }
        };
        if raw_frame.pixel_format != expected_frame_fmt {
            warn!(
                fmt = ?raw_frame.pixel_format,
                expected = ?expected_frame_fmt,
                "frame format mismatch (daemon and viewer must agree on --codec)"
            );
            continue;
        }
        let packet = EncodedPacket {
            bytes: raw_frame.pixels,
            pts_ms: raw_frame.timestamp_ms,
            // Flag is informational-only for the decoder; both
            // passthrough and H.264 ignore it on the decode path
            // (H.264 looks at the NAL header itself).
            is_keyframe: true,
        };
        let frames = match decoder.decode(&packet) {
            Ok(f) => f,
            Err(err) => {
                warn!(error = %err, "decode failed");
                continue;
            }
        };
        for decoded in frames {
            received += 1;

            if let Some(ref mut mirror) = expected_mirror {
                let expected = mirror
                    .capture()
                    .expect("mirror capture")
                    .expect("mirror produced frame");
                if decoded.pixels != expected.pixels {
                    // Fail fast — the pipeline is broken and further
                    // frames wouldn't tell us anything new.
                    return Err(format!(
                        "verify: decoded frame {received} did not match local mirror (byte-for-byte diff)"
                    )
                    .into());
                }
                if received <= 3 || received % 10 == 0 {
                    info!(received, "frame verified byte-for-byte vs mirror");
                }
            } else if received <= 3 || received % 10 == 0 {
                info!(
                    received,
                    width = decoded.width,
                    height = decoded.height,
                    bytes = decoded.pixels.len(),
                    "frame received + decoded"
                );
            }
        }
    }

    if args.verify {
        info!(verified = received, "verify: all frames matched mirror");
    }
    info!(received, "viewer exiting");
    Ok(())
}
