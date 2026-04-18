// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: AGPL-3.0-or-later

//! H.264 encode / decode via `ffmpeg-next` (libx264 + libavcodec).
//!
//! **M1 scaffold only.** Real implementation lands in M1.2b; this
//! module currently exposes the type name so downstream code can
//! reference it behind a `#[cfg(feature = "h264")]` without a
//! follow-up rename later.
//!
//! The passthrough codec in [`crate::passthrough`] is the M0/M1
//! working path.

use crate::{CodecError, DecodedFrame, Decoder, EncodeParams, EncodedPacket, Encoder, PixelFormat};

/// H.264 encoder wrapping libx264 via `ffmpeg-next`.
///
/// **Scaffold only.** `new` currently returns
/// [`CodecError::Unavailable`]; the real libav pipeline lands in
/// M1.2b.
pub struct H264Encoder {
    params: EncodeParams,
}

impl H264Encoder {
    /// Construct an H.264 encoder with the given params.
    ///
    /// **Currently unimplemented** — returns
    /// [`CodecError::Unavailable`] pending M1.2b.
    pub fn new(params: EncodeParams) -> Result<Self, CodecError> {
        let _ = params;
        Err(CodecError::Unavailable(
            "xenia-video H.264 backend lands in M1.2b; use passthrough for now".into(),
        ))
    }
}

impl Encoder for H264Encoder {
    fn encode(&mut self, _raw: &[u8], _pts_ms: u64) -> Result<Vec<EncodedPacket>, CodecError> {
        Err(CodecError::Unavailable(
            "xenia-video H.264 backend lands in M1.2b".into(),
        ))
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>, CodecError> {
        Ok(Vec::new())
    }

    fn params(&self) -> EncodeParams {
        self.params
    }
}

/// H.264 decoder. **Scaffold only** — see [`H264Encoder`].
pub struct H264Decoder;

impl H264Decoder {
    /// Construct an H.264 decoder. **Currently unimplemented.**
    pub fn new() -> Result<Self, CodecError> {
        Err(CodecError::Unavailable(
            "xenia-video H.264 backend lands in M1.2b".into(),
        ))
    }
}

impl Decoder for H264Decoder {
    fn decode(&mut self, _packet: &EncodedPacket) -> Result<Vec<DecodedFrame>, CodecError> {
        Err(CodecError::Unavailable(
            "xenia-video H.264 backend lands in M1.2b".into(),
        ))
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>, CodecError> {
        Ok(Vec::new())
    }

    fn output_format(&self) -> PixelFormat {
        PixelFormat::Rgba
    }
}
