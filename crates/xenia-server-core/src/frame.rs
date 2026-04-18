// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Framing types for M0.
//!
//! Raw RGBA only — no encoding, no delta compression, no patch format.
//! Real codecs land in `xenia-video` (M1).

use serde::{Deserialize, Serialize};
use xenia_wire::{Sealable, WireError};

/// Pixel layout of a [`RawFrame`]. M0 supports RGBA8 only; other
/// variants are reserved so future formats (BGRA, YUV420, encoded
/// H.264 NALs) can be added without breaking wire compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum PixelFormat {
    /// 8 bits per channel, red-green-blue-alpha order, 4 bytes per pixel.
    Rgba8 = 0,
    /// 8 bits per channel, blue-green-red-alpha order, 4 bytes per pixel.
    /// Reserved — not yet produced by any capture backend.
    Bgra8 = 1,
    /// Encoded H.264 access unit (SPS/PPS + slice). Reserved for M1.
    H264 = 16,
    /// Encoded VP9 frame. Reserved for M1.
    Vp9 = 17,
}

/// A single captured-screen frame on the forward path.
///
/// The `pixels` field carries raw bytes whose layout is determined by
/// `pixel_format`. For `Rgba8`, `pixels.len()` MUST equal
/// `width * height * 4`. The receiver is responsible for validating;
/// a malformed frame surfaces as a `RawFrame::validate` failure.
///
/// This struct is `Sealable`, which means it flows through the Xenia
/// wire as payload type `0x10` (FRAME) or `0x12` (FRAME_LZ4). The
/// server-side capture loop produces these; the viewer opens them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawFrame {
    /// Monotonic frame identifier. Resets on rekey (SPEC §6).
    pub frame_id: u64,
    /// Server-local milliseconds-since-Unix-epoch at capture time.
    /// Viewer uses this for latency measurement and frame-drop detection.
    pub timestamp_ms: u64,
    /// Frame width in pixels (or logical units for encoded formats).
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Pixel layout / codec identifier.
    pub pixel_format: PixelFormat,
    /// Raw pixel bytes. For `Rgba8`, length MUST be `width * height * 4`.
    pub pixels: Vec<u8>,
}

impl RawFrame {
    /// Construct an `Rgba8` frame. Panics in debug builds if the pixel
    /// buffer's length doesn't match the declared dimensions.
    pub fn rgba8(
        frame_id: u64,
        timestamp_ms: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Self {
        debug_assert_eq!(
            pixels.len() as u64,
            u64::from(width) * u64::from(height) * 4,
            "RawFrame::rgba8: pixel buffer length mismatch",
        );
        Self {
            frame_id,
            timestamp_ms,
            width,
            height,
            pixel_format: PixelFormat::Rgba8,
            pixels,
        }
    }

    /// Runtime check that the pixel buffer matches the declared layout.
    /// Called by the receive path before rendering; returns `false`
    /// when the frame should be dropped.
    pub fn validate(&self) -> bool {
        match self.pixel_format {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => {
                self.pixels.len() as u64 == u64::from(self.width) * u64::from(self.height) * 4
            }
            PixelFormat::H264 | PixelFormat::Vp9 => {
                // Encoded formats are opaque — any non-empty byte
                // sequence is structurally valid here; decoder has
                // the actual say.
                !self.pixels.is_empty()
            }
        }
    }
}

impl Sealable for RawFrame {
    fn to_bin(&self) -> Result<Vec<u8>, WireError> {
        bincode::serialize(self).map_err(WireError::encode)
    }
    fn from_bin(bytes: &[u8]) -> Result<Self, WireError> {
        bincode::deserialize(bytes).map_err(WireError::decode)
    }
}

/// A viewer-side input event traveling on the reverse path.
///
/// The `payload` is opaque bytes with caller-defined semantics. For
/// M0 the convention is a UTF-8 JSON object, matching what
/// `xenia-viewer-web`'s viewer MVP emits. Future milestones may
/// switch to a typed enum; the opaque-bytes shape is deliberate so
/// the core crate doesn't have to ship a stable `InputEvent`
/// taxonomy before the viewer ecosystem agrees on one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawInput {
    /// Monotonic per-session input sequence. Independent of
    /// `xenia_wire`'s nonce sequence.
    pub sequence: u64,
    /// Viewer-local milliseconds-since-Unix-epoch at capture time.
    pub timestamp_ms: u64,
    /// Event payload. M0 convention: UTF-8 JSON.
    pub payload: Vec<u8>,
}

impl Sealable for RawInput {
    fn to_bin(&self) -> Result<Vec<u8>, WireError> {
        bincode::serialize(self).map_err(WireError::encode)
    }
    fn from_bin(bytes: &[u8]) -> Result<Self, WireError> {
        bincode::deserialize(bytes).map_err(WireError::decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_red(w: u32, h: u32) -> Vec<u8> {
        (0..(w * h)).flat_map(|_| [255u8, 0, 0, 255]).collect()
    }

    #[test]
    fn rgba8_roundtrip_preserves_bytes() {
        let frame = RawFrame::rgba8(1, 1_700_000_000_000, 4, 2, solid_red(4, 2));
        let bytes = frame.to_bin().unwrap();
        let decoded = RawFrame::from_bin(&bytes).unwrap();
        assert_eq!(decoded, frame);
        assert!(decoded.validate());
    }

    #[test]
    fn validate_rejects_mismatched_buffer() {
        let frame = RawFrame {
            frame_id: 1,
            timestamp_ms: 0,
            width: 10,
            height: 10,
            pixel_format: PixelFormat::Rgba8,
            pixels: vec![0u8; 5],
        };
        assert!(!frame.validate());
    }

    #[test]
    fn encoded_frame_accepts_opaque_bytes() {
        let frame = RawFrame {
            frame_id: 7,
            timestamp_ms: 0,
            width: 1920,
            height: 1080,
            pixel_format: PixelFormat::H264,
            pixels: vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42],
        };
        assert!(frame.validate());
    }

    #[test]
    fn input_roundtrip() {
        let input = RawInput {
            sequence: 42,
            timestamp_ms: 1_700_000_000_050,
            payload: br#"{"type":"mousemove","x":0.5,"y":0.5}"#.to_vec(),
        };
        let bytes = input.to_bin().unwrap();
        let decoded = RawInput::from_bin(&bytes).unwrap();
        assert_eq!(decoded, input);
    }
}
