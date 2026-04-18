// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Portions derived from `symthaea/crates/symthaea-phone-embodiment/src/scrcpy/decoder.rs`
// (same author, same AGPL terms) — specifically the ffmpeg-next
// threading config, swscale-cached-context pattern, and the
// send/receive drain loop for decode. See VIEWER_PLAN §0.1
// "carry wholesale" guidance.

//! H.264 encode / decode via `ffmpeg-next` (libx264 + libavcodec).
//!
//! **Requires the `h264` feature**, which in turn requires libav dev
//! headers + libclang at build time. On NixOS this is
//! `nix develop /srv/luminous-dynamics` (or Symthaea's flake);
//! outside Nix install `ffmpeg` + `llvm` dev packages through
//! distro channels.
//!
//! Pipeline:
//!
//! ```text
//! RGBA8 raw ─swscale─▶ YUV420P ─libx264─▶ H.264 NAL packet
//!                                                │
//!                                                ▼
//! RGBA8 raw ◀─swscale─ YUV420P ◀libavcodec─ (remote side)
//! ```
//!
//! Threading: libx264 is internally multi-threaded via x264's own
//! slice threading. The decode side uses `Type::Slice, count=0`
//! (ffmpeg picks `nproc`) — this matches the Symthaea scrcpy
//! decoder's discovery that `Type::Frame` needs too much input
//! buffered before emitting frames, capping us at single-frame
//! latency.

use std::sync::Once;

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;

use crate::{
    CodecError, DecodedFrame, Decoder, EncodeParams, EncodedPacket, Encoder,
    PixelFormat as XvPixelFormat,
};

static FFMPEG_INIT: Once = Once::new();

fn ensure_ffmpeg_initialized() {
    FFMPEG_INIT.call_once(|| {
        // ffmpeg-next 7 handles registration implicitly; init() is kept
        // idempotent via this Once guard so we can call it freely.
        let _ = ffmpeg::init();
    });
}

fn map_xv_pix_to_ff(f: XvPixelFormat) -> Pixel {
    match f {
        XvPixelFormat::Rgba => Pixel::RGBA,
        XvPixelFormat::Bgra => Pixel::BGRA,
    }
}

fn ff_err(e: ffmpeg::Error, context: &str) -> CodecError {
    CodecError::Backend(format!("{context}: {e}"))
}

// ═══════════════════════════════════════════════════════════════════
// Encoder
// ═══════════════════════════════════════════════════════════════════

/// H.264 encoder wrapping `libx264` via `ffmpeg-next`.
///
/// Input: raw RGBA (or BGRA) frames. Output: H.264 NAL packets in
/// Annex-B format (start-code-prefixed; what a standard H.264
/// decoder or WebCodecs expects).
///
/// Sender-side lifecycle: `new(params)` → `encode(frame, pts)` per
/// captured frame → `flush()` at end-of-stream. Drop releases the
/// libav encoder context.
pub struct H264Encoder {
    params: EncodeParams,
    encoder: ffmpeg::encoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    // Reusable YUV420P frame so we don't reallocate per encode.
    yuv: ffmpeg::frame::Video,
    // Monotonically increasing PTS in the encoder's time_base units.
    // time_base is 1/target_fps so each successive frame bumps by 1.
    next_pts: i64,
}

impl H264Encoder {
    /// Build an H.264 encoder configured for the given frame
    /// dimensions, pixel format, frame rate, and bitrate.
    pub fn new(params: EncodeParams) -> Result<Self, CodecError> {
        ensure_ffmpeg_initialized();

        let codec = ffmpeg::encoder::find_by_name("libx264").ok_or_else(|| {
            CodecError::Unavailable(
                "libx264 not available in this ffmpeg build; install ffmpeg with --enable-libx264"
                    .into(),
            )
        })?;

        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
        ctx.set_threading(ffmpeg::codec::threading::Config {
            kind: ffmpeg::codec::threading::Type::Slice,
            count: 0,
        });

        let mut enc = ctx
            .encoder()
            .video()
            .map_err(|e| ff_err(e, "encoder() -> video"))?;

        enc.set_width(params.width);
        enc.set_height(params.height);
        enc.set_format(Pixel::YUV420P);
        let fps = params.target_fps.max(1);
        enc.set_time_base(ffmpeg::Rational::new(1, fps as i32));
        enc.set_frame_rate(Some(ffmpeg::Rational::new(fps as i32, 1)));
        enc.set_bit_rate(params.bitrate_kbps as usize * 1_000);
        // One keyframe every 2 seconds. Matches typical remote-desktop
        // cadence — long enough that inter-frame compression pays off,
        // short enough that a fresh viewer joining mid-stream waits at
        // most 2s for a decodable frame.
        enc.set_gop((fps * 2).max(2));
        // Zero-latency tuning — no B-frames, no lookahead. Trade ~20%
        // quality for real-time pipeline.
        enc.set_max_b_frames(0);

        let mut opts = ffmpeg::Dictionary::new();
        opts.set("preset", "ultrafast");
        opts.set("tune", "zerolatency");

        let opened = enc
            .open_with(opts)
            .map_err(|e| ff_err(e, "encoder.open_with"))?;
        let encoder = opened;

        let src_pix = map_xv_pix_to_ff(params.pixel_format);
        let scaler = ffmpeg::software::scaling::Context::get(
            src_pix,
            params.width,
            params.height,
            Pixel::YUV420P,
            params.width,
            params.height,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .map_err(|e| ff_err(e, "swscale RGBA->YUV420P"))?;

        let yuv = ffmpeg::frame::Video::new(Pixel::YUV420P, params.width, params.height);

        Ok(Self {
            params,
            encoder,
            scaler,
            yuv,
            next_pts: 0,
        })
    }

    fn drain(&mut self) -> Result<Vec<EncodedPacket>, CodecError> {
        let mut out = Vec::new();
        let mut pkt = ffmpeg::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut pkt) {
                Ok(()) => {
                    let data = pkt
                        .data()
                        .ok_or_else(|| CodecError::Backend("encoder packet had no data".into()))?;
                    // Reconstruct pts_ms from the encoder's time_base
                    // (1 / target_fps) — pts_frames * 1000 / fps.
                    let pts_frames = pkt.pts().unwrap_or(0).max(0) as u64;
                    let fps = self.params.target_fps.max(1) as u64;
                    let pts_ms = pts_frames * 1000 / fps;
                    out.push(EncodedPacket {
                        bytes: data.to_vec(),
                        pts_ms,
                        is_keyframe: pkt.is_key(),
                    });
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    break;
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(ff_err(e, "encoder.receive_packet")),
            }
        }
        Ok(out)
    }
}

impl Encoder for H264Encoder {
    fn encode(&mut self, raw: &[u8], _pts_ms: u64) -> Result<Vec<EncodedPacket>, CodecError> {
        let expected = self.params.frame_size();
        if raw.len() != expected {
            return Err(CodecError::InputMismatch(format!(
                "h264: expected {} bytes for {}x{} {:?}, got {}",
                expected,
                self.params.width,
                self.params.height,
                self.params.pixel_format,
                raw.len()
            )));
        }

        // Stage the raw RGBA into an ffmpeg frame so swscale can
        // read it. `frame::Video` expects planar/packed data laid
        // out in `data(0)` with the declared stride. For packed
        // RGBA we populate plane 0 tightly.
        let src_pix = map_xv_pix_to_ff(self.params.pixel_format);
        let mut rgba_frame =
            ffmpeg::frame::Video::new(src_pix, self.params.width, self.params.height);
        let stride = rgba_frame.stride(0);
        let row_bytes = self.params.stride();
        {
            let dst = rgba_frame.data_mut(0);
            for row in 0..self.params.height as usize {
                let src_off = row * row_bytes;
                let dst_off = row * stride;
                dst[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&raw[src_off..src_off + row_bytes]);
            }
        }

        self.scaler
            .run(&rgba_frame, &mut self.yuv)
            .map_err(|e| ff_err(e, "swscale.run"))?;
        self.yuv.set_pts(Some(self.next_pts));
        self.next_pts += 1;

        self.encoder
            .send_frame(&self.yuv)
            .map_err(|e| ff_err(e, "encoder.send_frame"))?;

        self.drain()
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>, CodecError> {
        self.encoder
            .send_eof()
            .map_err(|e| ff_err(e, "encoder.send_eof"))?;
        self.drain()
    }

    fn params(&self) -> EncodeParams {
        self.params
    }
}

// ═══════════════════════════════════════════════════════════════════
// Decoder
// ═══════════════════════════════════════════════════════════════════

/// H.264 decoder producing RGBA frames.
pub struct H264Decoder {
    decoder: ffmpeg::decoder::Video,
    // Scaler is lazy because we don't know the stream dimensions
    // until we see the first decoded frame.
    scaler: Option<CachedScaler>,
}

struct CachedScaler {
    in_format: Pixel,
    in_width: u32,
    in_height: u32,
    ctx: ffmpeg::software::scaling::Context,
}

impl H264Decoder {
    /// Build an H.264 decoder. Dimensions are learned from the
    /// bitstream SPS; no need to pass them upfront.
    pub fn new() -> Result<Self, CodecError> {
        ensure_ffmpeg_initialized();
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
            .ok_or_else(|| CodecError::Unavailable("H.264 decoder not found".into()))?;
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
        ctx.set_threading(ffmpeg::codec::threading::Config {
            kind: ffmpeg::codec::threading::Type::Slice,
            count: 0,
        });
        let decoder = ctx
            .decoder()
            .video()
            .map_err(|e| ff_err(e, "decoder() -> video"))?;
        Ok(Self {
            decoder,
            scaler: None,
        })
    }

    fn convert_to_rgba(&mut self, yuv: &ffmpeg::frame::Video) -> Result<Vec<u8>, CodecError> {
        let in_format = yuv.format();
        let in_width = yuv.width();
        let in_height = yuv.height();

        let needs_rebuild = match &self.scaler {
            Some(s) => {
                s.in_format != in_format || s.in_width != in_width || s.in_height != in_height
            }
            None => true,
        };
        if needs_rebuild {
            let ctx = ffmpeg::software::scaling::Context::get(
                in_format,
                in_width,
                in_height,
                Pixel::RGBA,
                in_width,
                in_height,
                ffmpeg::software::scaling::Flags::BILINEAR,
            )
            .map_err(|e| ff_err(e, "swscale YUV->RGBA"))?;
            self.scaler = Some(CachedScaler {
                in_format,
                in_width,
                in_height,
                ctx,
            });
        }

        let scaler = self.scaler.as_mut().expect("scaler set above");
        let mut rgba_frame = ffmpeg::frame::Video::empty();
        scaler
            .ctx
            .run(yuv, &mut rgba_frame)
            .map_err(|e| ff_err(e, "scaler.run"))?;

        let stride = rgba_frame.stride(0);
        let row_bytes = (in_width as usize) * 4;
        let plane = rgba_frame.data(0);
        let mut out = Vec::with_capacity(row_bytes * in_height as usize);
        for row in 0..in_height as usize {
            let start = row * stride;
            out.extend_from_slice(&plane[start..start + row_bytes]);
        }
        Ok(out)
    }

    fn drain(&mut self) -> Result<Vec<DecodedFrame>, CodecError> {
        let mut out = Vec::new();
        let mut yuv = ffmpeg::frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut yuv) {
                Ok(()) => {
                    let width = yuv.width();
                    let height = yuv.height();
                    let rgba = self.convert_to_rgba(&yuv)?;
                    let pts_ms = yuv.pts().unwrap_or(0).max(0) as u64;
                    out.push(DecodedFrame {
                        width,
                        height,
                        pixel_format: XvPixelFormat::Rgba,
                        pixels: rgba,
                        pts_ms,
                    });
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    break;
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(ff_err(e, "decoder.receive_frame")),
            }
        }
        Ok(out)
    }
}

impl Decoder for H264Decoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<DecodedFrame>, CodecError> {
        let mut pkt = ffmpeg::Packet::copy(&packet.bytes);
        pkt.set_pts(Some(packet.pts_ms as i64));
        self.decoder
            .send_packet(&pkt)
            .map_err(|e| ff_err(e, "decoder.send_packet"))?;
        self.drain()
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>, CodecError> {
        self.decoder
            .send_eof()
            .map_err(|e| ff_err(e, "decoder.send_eof"))?;
        self.drain()
    }

    fn output_format(&self) -> XvPixelFormat {
        XvPixelFormat::Rgba
    }
}

// ═══════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn params(w: u32, h: u32) -> EncodeParams {
        EncodeParams {
            width: w,
            height: h,
            pixel_format: XvPixelFormat::Rgba,
            target_fps: 30,
            bitrate_kbps: 2_000,
        }
    }

    fn gradient_frame(w: u32, h: u32, seed: u8) -> Vec<u8> {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 4;
                pixels[i] = (x as u8).wrapping_add(seed);
                pixels[i + 1] = (y as u8).wrapping_add(seed);
                pixels[i + 2] = seed.wrapping_mul(3);
                pixels[i + 3] = 255;
            }
        }
        pixels
    }

    /// The most basic sanity check: encoder and decoder both
    /// construct without error. Catches missing-codec / missing-
    /// library build failures.
    #[test]
    fn construct_encoder_and_decoder() {
        let _ = H264Encoder::new(params(320, 240)).unwrap();
        let _ = H264Decoder::new().unwrap();
    }

    /// End-to-end: push 10 frames through encode → decode, confirm
    /// we get back ≥ 1 frame (H.264 buffers; some backends emit
    /// nothing until they see a keyframe + some P-frames).
    ///
    /// We don't assert pixel-exact equality because H.264 is lossy;
    /// instead we assert dimensions match and some non-trivial
    /// fraction of the pixels are non-zero.
    #[test]
    fn encode_decode_roundtrip_produces_frames() {
        let p = params(64, 48);
        let mut enc = H264Encoder::new(p).expect("H264Encoder::new");
        let mut dec = H264Decoder::new().expect("H264Decoder::new");

        let mut total_packets = 0;
        let mut total_frames = 0;
        for i in 0..10u8 {
            let raw = gradient_frame(p.width, p.height, i);
            let pkts = enc.encode(&raw, 0).expect("encode");
            total_packets += pkts.len();
            for pkt in pkts {
                let frames = dec.decode(&pkt).expect("decode");
                total_frames += frames.len();
                for f in frames {
                    assert_eq!(f.width, p.width);
                    assert_eq!(f.height, p.height);
                    assert_eq!(f.pixels.len(), (p.width * p.height * 4) as usize);
                    // Alpha channel is fixed at 255 by the scaler for
                    // YUV420P->RGBA; we assert at least some pixels
                    // are non-trivial so we know this isn't a zero
                    // buffer.
                    let nonzero = f
                        .pixels
                        .iter()
                        .take(256)
                        .filter(|&&b| b != 0 && b != 255)
                        .count();
                    assert!(nonzero > 0, "decoded frame had no non-trivial pixel bytes");
                }
            }
        }

        // Flush to drain any remaining frames.
        let tail_pkts = enc.flush().expect("flush encoder");
        total_packets += tail_pkts.len();
        for pkt in tail_pkts {
            let frames = dec.decode(&pkt).expect("decode tail");
            total_frames += frames.len();
        }
        let dec_tail = dec.flush().expect("flush decoder");
        total_frames += dec_tail.len();

        assert!(
            total_packets >= 1,
            "encoder produced {total_packets} packets over 10 frames"
        );
        assert!(
            total_frames >= 1,
            "decoder produced {total_frames} frames over {total_packets} packets"
        );
    }

    /// First packet MUST be a keyframe — libx264 with tune=zerolatency
    /// + ultrafast preset starts with an IDR.
    #[test]
    fn first_packet_is_keyframe() {
        let p = params(64, 48);
        let mut enc = H264Encoder::new(p).expect("H264Encoder::new");
        let raw = gradient_frame(p.width, p.height, 0);
        let pkts = enc.encode(&raw, 0).expect("encode");
        // Some tunings may emit nothing on frame 0 and wait for more
        // input. Drive a few frames if needed.
        let mut all: Vec<EncodedPacket> = pkts;
        for i in 1..5u8 {
            let more = enc
                .encode(&gradient_frame(p.width, p.height, i), 0)
                .expect("encode more");
            all.extend(more);
            if !all.is_empty() {
                break;
            }
        }
        assert!(!all.is_empty(), "no packets emitted in first 5 frames");
        assert!(
            all[0].is_keyframe,
            "first emitted packet should be keyframe"
        );
    }

    /// Encoder rejects frames of the wrong size.
    #[test]
    fn encoder_rejects_wrong_size() {
        let p = params(64, 48);
        let mut enc = H264Encoder::new(p).expect("H264Encoder::new");
        let err = enc.encode(&[0u8; 100], 0).unwrap_err();
        assert!(matches!(err, CodecError::InputMismatch(_)));
    }
}
