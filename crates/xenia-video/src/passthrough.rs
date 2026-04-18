// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Passthrough "codec" — no compression, no transform.
//!
//! Packs the raw RGBA bytes into an [`EncodedPacket`] unchanged, with
//! a minimal header declaring width / height / pixel-format so the
//! decoder can reconstruct [`DecodedFrame`] on the other side.
//!
//! **Purpose.** Gives M0/M1 a working end-to-end codec path without
//! pulling in `ffmpeg-next` + libav dev headers. The compressed-
//! bandwidth profile is obviously terrible (raw pixels over the
//! wire), but the wire integration, pipeline shape, seal/open path,
//! and decoder-emits-frames plumbing are all identical to the H.264
//! path that lands at M1.2b.
//!
//! Wire layout of `EncodedPacket.bytes` in passthrough mode:
//!
//! ```text
//! +--------+--------+--------+--------+--------+--------+--------+--------+
//! | magic  | ver    | fmt    |  rsv   |       width (u32, LE)             |
//! |  0x58  |  0x01  | 0=RGBA |  0x00  |                                   |
//! |        |        | 1=BGRA |        |                                   |
//! +--------+--------+--------+--------+--------+--------+--------+--------+
//! |       height (u32, LE)            |     pixels (width*height*bpp)     |
//! |                                   |     ...                           |
//! +--------+--------+--------+--------+-----------------------------------+
//! ```
//!
//! 12-byte header + raw pixel bytes. The header is NOT an H.264 NAL
//! and must not be confused with one; swap the encoder/decoder pair
//! together.

use crate::{CodecError, DecodedFrame, Decoder, EncodeParams, EncodedPacket, Encoder, PixelFormat};

const MAGIC: u8 = 0x58; // 'X'
const VERSION: u8 = 0x01;
const HEADER_LEN: usize = 12;

fn fmt_byte(fmt: PixelFormat) -> u8 {
    match fmt {
        PixelFormat::Rgba => 0,
        PixelFormat::Bgra => 1,
    }
}

fn fmt_from_byte(b: u8) -> Result<PixelFormat, CodecError> {
    match b {
        0 => Ok(PixelFormat::Rgba),
        1 => Ok(PixelFormat::Bgra),
        other => Err(CodecError::DecodeFailed(format!(
            "unknown passthrough pixel-format byte {other:#x}"
        ))),
    }
}

/// Passthrough encoder. Always available, no external deps.
pub struct PassthroughEncoder {
    params: EncodeParams,
}

impl PassthroughEncoder {
    /// Construct a passthrough encoder with the given params. Never
    /// fails — passthrough imposes no hardware or library
    /// prerequisites.
    pub fn new(params: EncodeParams) -> Self {
        Self { params }
    }
}

impl Encoder for PassthroughEncoder {
    fn encode(&mut self, raw: &[u8], pts_ms: u64) -> Result<Vec<EncodedPacket>, CodecError> {
        let expected = self.params.frame_size();
        if raw.len() != expected {
            return Err(CodecError::InputMismatch(format!(
                "passthrough: expected {} bytes for {}x{} {:?}, got {}",
                expected,
                self.params.width,
                self.params.height,
                self.params.pixel_format,
                raw.len()
            )));
        }
        let mut bytes = Vec::with_capacity(HEADER_LEN + raw.len());
        bytes.push(MAGIC);
        bytes.push(VERSION);
        bytes.push(fmt_byte(self.params.pixel_format));
        bytes.push(0x00); // reserved
        bytes.extend_from_slice(&self.params.width.to_le_bytes());
        bytes.extend_from_slice(&self.params.height.to_le_bytes());
        bytes.extend_from_slice(raw);
        Ok(vec![EncodedPacket {
            bytes,
            pts_ms,
            // Every passthrough frame is a standalone keyframe.
            is_keyframe: true,
        }])
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>, CodecError> {
        Ok(Vec::new())
    }

    fn params(&self) -> EncodeParams {
        self.params
    }
}

/// Passthrough decoder.
pub struct PassthroughDecoder {
    /// Output format reported by `output_format()`. Set to match the
    /// first packet's header and stable afterward.
    output_fmt: PixelFormat,
}

impl Default for PassthroughDecoder {
    fn default() -> Self {
        // Default to RGBA; overridden on first decode.
        Self {
            output_fmt: PixelFormat::Rgba,
        }
    }
}

impl PassthroughDecoder {
    /// New passthrough decoder.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Decoder for PassthroughDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<DecodedFrame>, CodecError> {
        let b = &packet.bytes;
        if b.len() < HEADER_LEN {
            return Err(CodecError::DecodeFailed(format!(
                "passthrough: packet too short ({} bytes, need at least {})",
                b.len(),
                HEADER_LEN
            )));
        }
        if b[0] != MAGIC {
            return Err(CodecError::DecodeFailed(format!(
                "passthrough: bad magic byte {:#x} (expected {:#x})",
                b[0], MAGIC
            )));
        }
        if b[1] != VERSION {
            return Err(CodecError::DecodeFailed(format!(
                "passthrough: unsupported version {:#x}",
                b[1]
            )));
        }
        let fmt = fmt_from_byte(b[2])?;
        let width = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let height = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
        let bpp = fmt.bpp();
        let expected_body = (width as usize) * (height as usize) * bpp;
        let body = &b[HEADER_LEN..];
        if body.len() != expected_body {
            return Err(CodecError::DecodeFailed(format!(
                "passthrough: body {} bytes, expected {} for {}x{} {:?}",
                body.len(),
                expected_body,
                width,
                height,
                fmt
            )));
        }
        self.output_fmt = fmt;
        Ok(vec![DecodedFrame {
            width,
            height,
            pixel_format: fmt,
            pixels: body.to_vec(),
            pts_ms: packet.pts_ms,
        }])
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>, CodecError> {
        Ok(Vec::new())
    }

    fn output_format(&self) -> PixelFormat {
        self.output_fmt
    }
}

// ───────────────────────── Tests ───────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> EncodeParams {
        EncodeParams {
            width: 16,
            height: 8,
            pixel_format: PixelFormat::Rgba,
            target_fps: 30,
            bitrate_kbps: 1000,
        }
    }

    fn sample_frame(p: EncodeParams) -> Vec<u8> {
        (0..p.frame_size()).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn encode_decode_roundtrip_preserves_pixels() {
        let p = params();
        let raw = sample_frame(p);
        let mut enc = PassthroughEncoder::new(p);
        let mut dec = PassthroughDecoder::new();
        let packets = enc.encode(&raw, 1234).unwrap();
        assert_eq!(packets.len(), 1);
        assert!(packets[0].is_keyframe);
        assert_eq!(packets[0].pts_ms, 1234);
        let frames = dec.decode(&packets[0]).unwrap();
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert_eq!(f.width, p.width);
        assert_eq!(f.height, p.height);
        assert_eq!(f.pixel_format, p.pixel_format);
        assert_eq!(f.pixels, raw);
        assert_eq!(f.pts_ms, 1234);
    }

    #[test]
    fn encode_rejects_wrong_size() {
        let p = params();
        let mut enc = PassthroughEncoder::new(p);
        let err = enc.encode(&[0u8; 5], 0).unwrap_err();
        assert!(matches!(err, CodecError::InputMismatch(_)));
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut dec = PassthroughDecoder::new();
        let pkt = EncodedPacket {
            bytes: vec![0x00; HEADER_LEN + 4],
            pts_ms: 0,
            is_keyframe: true,
        };
        let err = dec.decode(&pkt).unwrap_err();
        assert!(matches!(err, CodecError::DecodeFailed(_)));
    }

    #[test]
    fn decode_rejects_truncated_body() {
        let p = params();
        let mut enc = PassthroughEncoder::new(p);
        let raw = sample_frame(p);
        let mut pkt = enc.encode(&raw, 0).unwrap().remove(0);
        pkt.bytes.truncate(HEADER_LEN + 4); // body too short
        let mut dec = PassthroughDecoder::new();
        let err = dec.decode(&pkt).unwrap_err();
        assert!(matches!(err, CodecError::DecodeFailed(_)));
    }

    #[test]
    fn bgra_roundtrip() {
        let mut p = params();
        p.pixel_format = PixelFormat::Bgra;
        let raw = sample_frame(p);
        let mut enc = PassthroughEncoder::new(p);
        let mut dec = PassthroughDecoder::new();
        let packets = enc.encode(&raw, 0).unwrap();
        let frames = dec.decode(&packets[0]).unwrap();
        assert_eq!(frames[0].pixel_format, PixelFormat::Bgra);
        assert_eq!(frames[0].pixels, raw);
        assert_eq!(dec.output_format(), PixelFormat::Bgra);
    }
}
