// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! # xenia-video
//!
//! Video codec abstraction for the Xenia remote-session stack.
//!
//! The shared [`Encoder`] / [`Decoder`] traits let the daemon + viewer
//! negotiate over a codec-agnostic surface, with concrete backends
//! (currently H.264 via `ffmpeg-next`; VP9 / HEVC planned per
//! VIEWER_PLAN §4.1) behind feature flags.
//!
//! ## Quick start
//!
//! ```no_run
//! use xenia_video::{passthrough::PassthroughEncoder, Encoder, EncodeParams, PixelFormat};
//!
//! let mut enc = PassthroughEncoder::new(EncodeParams {
//!     width: 1920,
//!     height: 1080,
//!     pixel_format: PixelFormat::Rgba,
//!     target_fps: 30,
//!     bitrate_kbps: 6_000,
//! });
//!
//! let rgba_frame: Vec<u8> = vec![0x80; 1920 * 1080 * 4];
//! let packets = enc.encode(&rgba_frame, /* pts_ms */ 0).unwrap();
//! for packet in packets {
//!     // packet.bytes is wire-shipped; the viewer side decodes via
//!     // `passthrough::PassthroughDecoder`.
//! }
//! ```
//!
//! Swap `passthrough::*` for `h264::*` once the H.264 backend lands
//! at M1.2b. The `Encoder` / `Decoder` traits are identical.
//!
//! ## Thread safety
//!
//! Encoders and decoders are `!Send` by default because `ffmpeg-next`
//! wraps libav state that is not thread-safe. The intended pattern is
//! one encoder per capture thread, one decoder per render thread, with
//! the sealed wire envelopes as the cross-thread boundary.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

use thiserror::Error;

pub mod passthrough;

#[cfg(feature = "h264")]
pub mod h264;

#[cfg(feature = "hdc")]
pub mod hdc;

/// Errors surfaced by any codec backend.
#[derive(Debug, Error)]
pub enum CodecError {
    /// Backend-specific failure. Wrapped for logs; callers should
    /// drop the frame and continue.
    #[error("codec backend: {0}")]
    Backend(String),

    /// Input frame dimensions or pixel format didn't match the
    /// encoder's configured parameters.
    #[error("codec input mismatch: {0}")]
    InputMismatch(String),

    /// The configured codec is unavailable (missing feature flag,
    /// missing system library, etc.).
    #[error("codec unavailable: {0}")]
    Unavailable(String),

    /// Decoder received a malformed or truncated packet.
    #[error("codec decode: {0}")]
    DecodeFailed(String),
}

/// Pixel format for raw frames entering the encoder / leaving the
/// decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit RGBA, 4 bytes per pixel. Row-major, top-left origin.
    Rgba,
    /// 8-bit BGRA, 4 bytes per pixel. Native format on most
    /// Windows / macOS capture backends; re-swizzled before encode.
    Bgra,
}

impl PixelFormat {
    /// Bytes per pixel.
    pub fn bpp(self) -> usize {
        match self {
            PixelFormat::Rgba | PixelFormat::Bgra => 4,
        }
    }
}

/// Parameters for constructing an encoder.
#[derive(Debug, Clone, Copy)]
pub struct EncodeParams {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Raw-input pixel format.
    pub pixel_format: PixelFormat,
    /// Target frame rate in frames per second. Used for GOP /
    /// keyframe cadence heuristics.
    pub target_fps: u32,
    /// Target bitrate in kilobits per second.
    pub bitrate_kbps: u32,
}

impl EncodeParams {
    /// Expected stride (bytes per row) for a tightly-packed frame.
    pub fn stride(&self) -> usize {
        self.width as usize * self.pixel_format.bpp()
    }

    /// Expected total size of a raw frame in bytes.
    pub fn frame_size(&self) -> usize {
        self.stride() * self.height as usize
    }
}

/// An encoded video packet emitted by [`Encoder::encode`].
///
/// For H.264 this carries one or more concatenated NAL units in the
/// Annex-B format (00 00 00 01 start codes). Downstream code should
/// treat the bytes as opaque and feed them to the matching decoder.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// Encoded payload bytes. For H.264 Annex-B, contains one or more
    /// NAL units separated by 0x00 0x00 0x00 0x01 start codes.
    pub bytes: Vec<u8>,
    /// Presentation timestamp in milliseconds, forwarded from the
    /// caller's `encode(frame, pts_ms)` invocation.
    pub pts_ms: u64,
    /// `true` if this packet contains an IDR (instantaneous decoder
    /// refresh) frame — a keyframe from which the decoder can start
    /// fresh without prior-frame state.
    pub is_keyframe: bool,
}

/// A decoded raw frame emitted by [`Decoder::decode`].
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Pixel format of `pixels`.
    pub pixel_format: PixelFormat,
    /// Raw pixel bytes. Length == `width * height * pixel_format.bpp()`.
    pub pixels: Vec<u8>,
    /// Presentation timestamp in milliseconds, forwarded from the
    /// encoder.
    pub pts_ms: u64,
}

/// Video encoder.
///
/// One encoder instance per capture stream. Feeding frames of
/// different dimensions or pixel formats than the configured
/// `EncodeParams` returns [`CodecError::InputMismatch`].
pub trait Encoder {
    /// Encode a single raw frame. Returns zero or more packets — some
    /// backends emit nothing for the first frames while they fill
    /// their look-ahead window, then emit packets in bursts.
    fn encode(&mut self, raw: &[u8], pts_ms: u64) -> Result<Vec<EncodedPacket>, CodecError>;

    /// Flush any internal buffers, emitting any pending packets. Call
    /// this before dropping the encoder to avoid losing tail frames.
    fn flush(&mut self) -> Result<Vec<EncodedPacket>, CodecError>;

    /// Encoder parameters as configured at construction.
    fn params(&self) -> EncodeParams;
}

/// Video decoder.
///
/// One decoder instance per stream. The decoder consumes packets in
/// PTS order and emits [`DecodedFrame`]s in decode order. Backends
/// that buffer B-frames may emit nothing for some input packets and
/// then multiple frames for others.
pub trait Decoder {
    /// Decode one or more packets. Returns zero or more frames.
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<DecodedFrame>, CodecError>;

    /// Flush any internal buffers, emitting any pending frames. Call
    /// this at end-of-stream.
    fn flush(&mut self) -> Result<Vec<DecodedFrame>, CodecError>;

    /// Output pixel format. Decoder-internal colorspace conversion
    /// guarantees output matches this format.
    fn output_format(&self) -> PixelFormat;
}
