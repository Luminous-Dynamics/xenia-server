// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// Ported from `symthaea/src/swarm/rdp_codec.rs`. Relicensed to
// Apache-2.0 OR MIT for this crate by the copyright holder (same
// author); see ADR-002 for the library-vs-binary licensing split.
// Faithful port of the 64x64 grayscale-tile HDC-delta codec, minus
// the Symthaea-specific types and the consciousness-coupled framing.
// `ContinuousHV` is inlined below as a minimal ~40-line struct —
// the only HDC surface the codec touches is `from_values`,
// `.similarity(&Self)`, and `.values` element access, so there's no
// need to drag in `symthaea-core` as a dependency.

//! HDC hybrid tile-delta codec.
//!
//! Research-grade compression for desktop content. Not competitive
//! with H.264 on video frames; decisively better than H.264 on
//! low-motion text / UI / code editors thanks to sparse-tile
//! transmission + HDC-based change detection.
//!
//! ## Pipeline
//!
//! ```text
//!     Capture (RGBA)
//!         ↓
//!     64×64 tile grid
//!         ↓  (per tile)
//!     HDC encoding → cosine sim vs prev frame's tile HV
//!         ↓
//!     if sim > threshold (0.92 default) → skip
//!         ↓ else
//!     Classify (Text / Photo / Video / Static) via pixel stats
//!         ↓
//!     Grayscale-quantize tile (i8 values, TILE_SIZE² bytes)
//!         ↓
//!     Emit tile-delta packet: (keyframe flag, frame_id, changed
//!     tiles [(index, grayscale_bytes)…])
//! ```
//!
//! On the decoder side the previous full frame is held in a buffer;
//! each new packet patches in the changed tiles. First packet of a
//! stream is a **keyframe** covering every tile.
//!
//! Output is currently **grayscale only**. The underlying Symthaea
//! codec made this trade-off deliberately for sovereign-RDP
//! bandwidth; extending to RGB (or RGB-for-photo-tiles + grayscale-
//! for-text-tiles) is a follow-up.
//!
//! ## Wire format
//!
//! Each [`EncodedPacket`] body is a bincode-v1 serialization of
//! [`HdcPacket`]. The packet type (keyframe vs delta) is encoded
//! into the `tag` byte; every keyframe carries all
//! `tile_cols * tile_rows` tiles and is a valid self-contained
//! start-of-stream.

use crate::{
    CodecError, DecodedFrame, Decoder, EncodeParams, EncodedPacket, Encoder,
    PixelFormat as XvPixelFormat,
};
use serde::{Deserialize, Serialize};

// ═══════════════════════════════════════════════════════════════════
// Minimal ContinuousHV
// ═══════════════════════════════════════════════════════════════════

/// 16,384-dimensional continuous-valued hyperdimensional vector.
///
/// Vendored minimal surface from `symthaea_core::hdc::unified_hv::ContinuousHV`.
/// Only the operations the tile codec needs are implemented here —
/// enough for change detection + content classification. Not a
/// substitute for Symthaea's full HDC library.
#[derive(Clone, Debug)]
pub struct ContinuousHV {
    /// Dense continuous values, length == [`TILE_HDC_DIM`].
    pub values: Vec<f32>,
}

impl ContinuousHV {
    /// Construct from a pre-computed value vector.
    pub fn from_values(values: Vec<f32>) -> Self {
        Self { values }
    }

    /// Cosine similarity in `[-1.0, 1.0]`. Undefined for zero-norm
    /// vectors; returns `0.0` in that degenerate case (matches the
    /// Symthaea-core behavior the caller expects).
    pub fn similarity(&self, other: &Self) -> f32 {
        if self.values.len() != other.values.len() {
            return 0.0;
        }
        let mut dot = 0.0f32;
        let mut n_a = 0.0f32;
        let mut n_b = 0.0f32;
        for (a, b) in self.values.iter().zip(other.values.iter()) {
            dot += a * b;
            n_a += a * a;
            n_b += b * b;
        }
        let denom = (n_a * n_b).sqrt();
        if denom <= f32::EPSILON {
            0.0
        } else {
            dot / denom
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Codec constants + types
// ═══════════════════════════════════════════════════════════════════

/// Tile edge in pixels. 64×64 is Symthaea's canonical trade-off
/// between granularity and per-tile overhead.
pub const TILE_SIZE: usize = 64;

/// HDC vector dimension. Symthaea's default; smaller means faster
/// similarity compute but coarser change detection.
pub const TILE_HDC_DIM: usize = 16_384;

/// Default cosine-similarity threshold above which a tile is
/// considered unchanged. Tuned on screen recordings; lower = more
/// aggressive change detection (more bandwidth), higher = more
/// static-skipping.
pub const DEFAULT_CHANGE_THRESHOLD: f32 = 0.92;

/// Max delta patches per packet. Mirrors Symthaea's
/// `rdp_protocol::MAX_DELTA_PATCHES`; keeps a single sealed
/// envelope under the replay-window-friendly size limit.
pub const MAX_DELTA_PATCHES: usize = 512;

/// Content type detected by HDC classification. Used for future
/// adaptive encoding (grayscale for text, JPEG for photos, etc.);
/// currently all non-skipped tiles emit as grayscale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileContentType {
    /// Static UI / icons / backgrounds — skippable.
    Static,
    /// Text / code. Needs sharp edges; near-lossless encoding.
    Text,
    /// Natural image / photo. JPEG-quality is fine.
    Photo,
    /// Video / animation. High-motion region.
    Video,
}

/// Per-tile change patch carried in a delta packet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TilePatch {
    /// Linear tile index = `row * tile_cols + col`.
    pub index: u16,
    /// Cosine-similarity surprise score `1.0 - similarity`. Higher
    /// means more changed. Receivers can prioritize by this value.
    pub surprise: f32,
    /// Grayscale pixel bytes, `TILE_SIZE * TILE_SIZE` of them for
    /// edge-aligned tiles (shorter at the right/bottom image edges
    /// where the tile is clipped).
    pub values: Vec<u8>,
    /// Detected content type for adaptive future encoding.
    pub content_type: TileContentType,
    /// Logical width of this tile (in pixels; equals
    /// `TILE_SIZE` except at image edges where the tile is clipped).
    pub tile_w: u16,
    /// Logical height (see `tile_w`).
    pub tile_h: u16,
}

/// A complete encoded HDC frame payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HdcPacket {
    /// `0x01` = keyframe (all tiles), `0x02` = delta (changed tiles).
    pub tag: u8,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Number of tile columns.
    pub tile_cols: u16,
    /// Number of tile rows.
    pub tile_rows: u16,
    /// Serial frame id; monotonically increases per encoded frame.
    pub frame_id: u64,
    /// Source-time presentation timestamp in milliseconds.
    pub pts_ms: u64,
    /// Changed tile patches. For a keyframe this covers every tile
    /// in row-major order.
    pub patches: Vec<TilePatch>,
}

// ═══════════════════════════════════════════════════════════════════
// Per-tile state tracked across frames
// ═══════════════════════════════════════════════════════════════════

#[derive(Clone)]
struct TileState {
    hv: ContinuousHV,
    content_type: TileContentType,
    static_count: u32,
}

// ═══════════════════════════════════════════════════════════════════
// Encoder
// ═══════════════════════════════════════════════════════════════════

/// HDC hybrid-tile encoder. One instance per session.
pub struct HdcEncoder {
    params: EncodeParams,
    tile_cols: u16,
    tile_rows: u16,
    prev_tiles: Vec<TileState>,
    position_hvs: Vec<ContinuousHV>,
    frame_count: u64,
    change_threshold: f32,
    static_threshold: u32,
}

impl HdcEncoder {
    /// Construct a new HDC encoder sized to the given frame params.
    /// Pixel format must be RGBA or BGRA (both produce the same
    /// grayscale output since luminance is computed from all three
    /// channels).
    pub fn new(params: EncodeParams) -> Self {
        // Tile grid dimensions (ceil-divide at image edges).
        let tile_cols = params.width.div_ceil(TILE_SIZE as u32) as u16;
        let tile_rows = params.height.div_ceil(TILE_SIZE as u32) as u16;
        let n_tiles = tile_cols as usize * tile_rows as usize;

        // Deterministic position-basis HVs, seeded per-tile so each
        // tile index gets a unique "where am I" vector.
        let seed = (params.width as u64) * 0x100000000 + (params.height as u64);
        let position_hvs: Vec<ContinuousHV> = (0..n_tiles)
            .map(|i| generate_position_hv(i, seed))
            .collect();

        let prev_tiles = vec![
            TileState {
                hv: ContinuousHV::from_values(vec![0.0; TILE_HDC_DIM]),
                content_type: TileContentType::Static,
                static_count: 0,
            };
            n_tiles
        ];

        Self {
            params,
            tile_cols,
            tile_rows,
            prev_tiles,
            position_hvs,
            frame_count: 0,
            change_threshold: DEFAULT_CHANGE_THRESHOLD,
            static_threshold: params.target_fps.max(1), // ~1s of stillness = Static
        }
    }

    /// Adjust the change-detection threshold at runtime. Valid
    /// range `[0.5, 0.999]`.
    pub fn set_change_threshold(&mut self, t: f32) {
        self.change_threshold = t.clamp(0.5, 0.999);
    }
}

impl Encoder for HdcEncoder {
    fn encode(&mut self, raw: &[u8], pts_ms: u64) -> Result<Vec<EncodedPacket>, CodecError> {
        let expected = self.params.frame_size();
        if raw.len() != expected {
            return Err(CodecError::InputMismatch(format!(
                "hdc: expected {} bytes for {}x{} {:?}, got {}",
                expected,
                self.params.width,
                self.params.height,
                self.params.pixel_format,
                raw.len()
            )));
        }

        let is_keyframe = self.frame_count == 0;
        let width = self.params.width;
        let height = self.params.height;
        let mut patches: Vec<TilePatch> = Vec::new();

        for row in 0..self.tile_rows as usize {
            for col in 0..self.tile_cols as usize {
                let idx = row * self.tile_cols as usize + col;
                let tile_x = col * TILE_SIZE;
                let tile_y = row * TILE_SIZE;

                // HDC-encode the current tile.
                let tile_hv = encode_tile_hdc(
                    raw,
                    width as usize,
                    height as usize,
                    tile_x,
                    tile_y,
                    TILE_SIZE,
                    &self.position_hvs[idx],
                );

                let sim = self.prev_tiles[idx].hv.similarity(&tile_hv);
                let sim = if sim.is_finite() { sim } else { 0.0 };
                let changed = sim <= self.change_threshold;

                if changed {
                    self.prev_tiles[idx].static_count = 0;
                    let content = classify_tile_content(
                        raw,
                        width as usize,
                        height as usize,
                        tile_x,
                        tile_y,
                        TILE_SIZE,
                    );
                    self.prev_tiles[idx].content_type = content;
                } else {
                    self.prev_tiles[idx].static_count += 1;
                    if self.prev_tiles[idx].static_count >= self.static_threshold {
                        self.prev_tiles[idx].content_type = TileContentType::Static;
                    }
                }
                self.prev_tiles[idx].hv = tile_hv;

                // Emit the patch if it's a keyframe OR the tile
                // changed. Keyframes cover everything regardless.
                if is_keyframe || changed {
                    let (values, tile_w, tile_h) = extract_tile_grayscale(
                        raw,
                        width as usize,
                        height as usize,
                        tile_x,
                        tile_y,
                        TILE_SIZE,
                    );
                    patches.push(TilePatch {
                        index: idx as u16,
                        surprise: 1.0 - sim,
                        values,
                        content_type: self.prev_tiles[idx].content_type,
                        tile_w,
                        tile_h,
                    });

                    // Delta packets cap patches to keep each sealed
                    // envelope reasonably sized. A full keyframe is
                    // exempt — it always carries every tile even if
                    // that means a larger first packet.
                    if !is_keyframe && patches.len() >= MAX_DELTA_PATCHES {
                        break;
                    }
                }
            }
            if !is_keyframe && patches.len() >= MAX_DELTA_PATCHES {
                break;
            }
        }

        let frame_id = self.frame_count;
        self.frame_count += 1;

        let packet = HdcPacket {
            tag: if is_keyframe { 0x01 } else { 0x02 },
            width,
            height,
            tile_cols: self.tile_cols,
            tile_rows: self.tile_rows,
            frame_id,
            pts_ms,
            patches,
        };

        let bytes = bincode::serialize(&packet)
            .map_err(|e| CodecError::Backend(format!("hdc encode bincode: {e}")))?;

        Ok(vec![EncodedPacket {
            bytes,
            pts_ms,
            is_keyframe,
        }])
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>, CodecError> {
        Ok(Vec::new())
    }

    fn params(&self) -> EncodeParams {
        self.params
    }
}

// ═══════════════════════════════════════════════════════════════════
// Decoder
// ═══════════════════════════════════════════════════════════════════

/// HDC decoder. Holds a full-frame canvas and patches incoming
/// deltas into it.
pub struct HdcDecoder {
    canvas: Vec<u8>,
    width: u32,
    height: u32,
    tile_cols: u16,
    tile_rows: u16,
    // Have we seen a keyframe yet? Deltas before the first
    // keyframe are rejected.
    primed: bool,
}

impl HdcDecoder {
    /// Construct a fresh decoder with no canvas. The first keyframe
    /// allocates the canvas and subsequent deltas patch into it.
    pub fn new() -> Self {
        Self {
            canvas: Vec::new(),
            width: 0,
            height: 0,
            tile_cols: 0,
            tile_rows: 0,
            primed: false,
        }
    }
}

impl Default for HdcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for HdcDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<DecodedFrame>, CodecError> {
        let pkt: HdcPacket = bincode::deserialize(&packet.bytes)
            .map_err(|e| CodecError::DecodeFailed(format!("hdc decode bincode: {e}")))?;

        // Reshape canvas to the packet's declared dimensions. A
        // stream can carry dimensions changes across keyframes.
        let canvas_len = (pkt.width as usize) * (pkt.height as usize) * 4;
        if pkt.tag == 0x01 {
            // Keyframe: (re)allocate canvas fresh.
            if self.canvas.len() != canvas_len {
                self.canvas = vec![0u8; canvas_len];
            } else {
                self.canvas.fill(0);
            }
            self.width = pkt.width;
            self.height = pkt.height;
            self.tile_cols = pkt.tile_cols;
            self.tile_rows = pkt.tile_rows;
            self.primed = true;
        } else if pkt.tag == 0x02 {
            if !self.primed {
                return Err(CodecError::DecodeFailed(
                    "hdc: delta received before first keyframe".into(),
                ));
            }
            if pkt.width != self.width || pkt.height != self.height {
                return Err(CodecError::DecodeFailed(
                    "hdc: delta declared different dimensions than current canvas".into(),
                ));
            }
        } else {
            return Err(CodecError::DecodeFailed(format!(
                "hdc: unknown packet tag {:#x}",
                pkt.tag
            )));
        }

        // Patch each tile into the canvas.
        for patch in &pkt.patches {
            let idx = patch.index as usize;
            if idx >= (self.tile_cols as usize) * (self.tile_rows as usize) {
                return Err(CodecError::DecodeFailed(format!(
                    "hdc: tile index {} out of range",
                    idx
                )));
            }
            let row = idx / self.tile_cols as usize;
            let col = idx % self.tile_cols as usize;
            let tile_x = col * TILE_SIZE;
            let tile_y = row * TILE_SIZE;
            let tw = patch.tile_w as usize;
            let th = patch.tile_h as usize;
            if patch.values.len() != tw * th {
                return Err(CodecError::DecodeFailed(format!(
                    "hdc: tile {} has {} bytes, declared {}×{}",
                    idx,
                    patch.values.len(),
                    tw,
                    th
                )));
            }
            for dy in 0..th {
                for dx in 0..tw {
                    let src = patch.values[dy * tw + dx];
                    let dst_off = ((tile_y + dy) * self.width as usize + (tile_x + dx)) * 4;
                    if dst_off + 3 < self.canvas.len() {
                        // Expand grayscale to RGBA (R=G=B=src, A=255).
                        self.canvas[dst_off] = src;
                        self.canvas[dst_off + 1] = src;
                        self.canvas[dst_off + 2] = src;
                        self.canvas[dst_off + 3] = 255;
                    }
                }
            }
        }

        Ok(vec![DecodedFrame {
            width: self.width,
            height: self.height,
            pixel_format: XvPixelFormat::Rgba,
            pixels: self.canvas.clone(),
            pts_ms: pkt.pts_ms,
        }])
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>, CodecError> {
        Ok(Vec::new())
    }

    fn output_format(&self) -> XvPixelFormat {
        XvPixelFormat::Rgba
    }
}

// ═══════════════════════════════════════════════════════════════════
// Internal helpers — ported from Symthaea's rdp_codec.rs
// ═══════════════════════════════════════════════════════════════════

/// Deterministic position-basis HV. Same seed => same HV. Used to
/// domain-separate tiles so two visually-identical tiles at different
/// positions produce distinguishable HVs.
fn generate_position_hv(index: usize, seed: u64) -> ContinuousHV {
    let combined = seed
        .wrapping_add(index as u64)
        .wrapping_mul(0x517cc1b727220a95);
    let mut values = vec![0.0f32; TILE_HDC_DIM];
    let mut state = combined;
    for v in values.iter_mut() {
        let hash = blake3::hash(&state.to_le_bytes());
        let bytes = hash.as_bytes();
        let u =
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / u32::MAX as f32;
        *v = (u - 0.5) * 2.0; // centered in [-1, 1]
        state = state.wrapping_add(1);
    }
    ContinuousHV::from_values(values)
}

/// Feature-extract a tile's pixel content into an HDC vector. 8
/// features (luminance mean, contrast, RGB means, edge density,
/// and two interaction terms) modulate 8 equal-width bands of the
/// position HV.
fn encode_tile_hdc(
    pixels: &[u8],
    img_width: usize,
    img_height: usize,
    tile_x: usize,
    tile_y: usize,
    tile_size: usize,
    position_hv: &ContinuousHV,
) -> ContinuousHV {
    let mut lum_sum = 0.0f32;
    let mut lum_sq_sum = 0.0f32;
    let mut r_sum = 0.0f32;
    let mut g_sum = 0.0f32;
    let mut b_sum = 0.0f32;
    let mut edge_energy = 0.0f32;
    let mut pixel_count = 0u32;

    for dy in 0..tile_size {
        let y = tile_y + dy;
        if y >= img_height {
            break;
        }
        for dx in 0..tile_size {
            let x = tile_x + dx;
            if x >= img_width {
                break;
            }
            let offset = (y * img_width + x) * 4;
            if offset + 3 >= pixels.len() {
                break;
            }
            let r = pixels[offset] as f32 / 255.0;
            let g = pixels[offset + 1] as f32 / 255.0;
            let b = pixels[offset + 2] as f32 / 255.0;
            let lum = 0.299 * r + 0.587 * g + 0.114 * b;

            lum_sum += lum;
            lum_sq_sum += lum * lum;
            r_sum += r;
            g_sum += g;
            b_sum += b;
            pixel_count += 1;

            if dx > 0 {
                let prev_offset = (y * img_width + x - 1) * 4;
                if prev_offset + 3 < pixels.len() {
                    let prev_lum = 0.299 * pixels[prev_offset] as f32 / 255.0
                        + 0.587 * pixels[prev_offset + 1] as f32 / 255.0
                        + 0.114 * pixels[prev_offset + 2] as f32 / 255.0;
                    edge_energy += (lum - prev_lum).abs();
                }
            }
        }
    }

    let n = pixel_count.max(1) as f32;
    let mean_lum = lum_sum / n;
    let variance = (lum_sq_sum / n - mean_lum * mean_lum).max(0.0);
    let contrast = variance.sqrt();
    let mean_r = r_sum / n;
    let mean_g = g_sum / n;
    let mean_b = b_sum / n;
    let edge_density = edge_energy / n;

    let mut values = vec![0.0f32; TILE_HDC_DIM];
    let band_size = TILE_HDC_DIM / 8;
    for (i, (v_out, pos)) in values.iter_mut().zip(position_hv.values.iter()).enumerate() {
        let band = i / band_size;
        let feature_weight = match band {
            0 => mean_lum,
            1 => contrast,
            2 => mean_r,
            3 => mean_g,
            4 => mean_b,
            5 => edge_density,
            6 => mean_lum * contrast,
            _ => (mean_r - mean_b).abs(),
        };
        *v_out = pos * feature_weight;
    }
    ContinuousHV::from_values(values)
}

/// Classify a tile's content type from its pixel statistics.
/// Heuristics: low-variance = Static, high-edge-density = Text,
/// balanced = Photo, high-lum-variance = Video.
fn classify_tile_content(
    pixels: &[u8],
    img_width: usize,
    img_height: usize,
    tile_x: usize,
    tile_y: usize,
    tile_size: usize,
) -> TileContentType {
    let mut lum_sum = 0.0f32;
    let mut lum_sq_sum = 0.0f32;
    let mut edge_count = 0u32;
    let mut n = 0u32;

    for dy in 0..tile_size {
        let y = tile_y + dy;
        if y >= img_height {
            break;
        }
        for dx in 0..tile_size {
            let x = tile_x + dx;
            if x >= img_width {
                break;
            }
            let offset = (y * img_width + x) * 4;
            if offset + 3 >= pixels.len() {
                break;
            }
            let r = pixels[offset] as f32 / 255.0;
            let g = pixels[offset + 1] as f32 / 255.0;
            let b = pixels[offset + 2] as f32 / 255.0;
            let lum = 0.299 * r + 0.587 * g + 0.114 * b;
            lum_sum += lum;
            lum_sq_sum += lum * lum;
            n += 1;

            if dx > 0 {
                let prev_offset = (y * img_width + x - 1) * 4;
                if prev_offset + 3 < pixels.len() {
                    let prev_lum = 0.299 * pixels[prev_offset] as f32 / 255.0
                        + 0.587 * pixels[prev_offset + 1] as f32 / 255.0
                        + 0.114 * pixels[prev_offset + 2] as f32 / 255.0;
                    if (lum - prev_lum).abs() > 0.15 {
                        edge_count += 1;
                    }
                }
            }
        }
    }

    if n < 2 {
        return TileContentType::Static;
    }
    let mean = lum_sum / n as f32;
    let variance = (lum_sq_sum / n as f32 - mean * mean).max(0.0);
    let edge_density = edge_count as f32 / n as f32;

    if variance < 0.005 {
        TileContentType::Static
    } else if edge_density > 0.15 {
        TileContentType::Text
    } else if variance > 0.1 {
        TileContentType::Video
    } else {
        TileContentType::Photo
    }
}

/// Extract a tile's pixels as 8-bit grayscale (row-major). Returns
/// the bytes + the logical (width, height) of the tile, which may be
/// less than `tile_size` at the image's right/bottom edge where the
/// tile is clipped.
fn extract_tile_grayscale(
    pixels: &[u8],
    img_width: usize,
    img_height: usize,
    tile_x: usize,
    tile_y: usize,
    tile_size: usize,
) -> (Vec<u8>, u16, u16) {
    let tw = tile_size.min(img_width.saturating_sub(tile_x));
    let th = tile_size.min(img_height.saturating_sub(tile_y));
    let mut out = Vec::with_capacity(tw * th);
    for dy in 0..th {
        let y = tile_y + dy;
        for dx in 0..tw {
            let x = tile_x + dx;
            let offset = (y * img_width + x) * 4;
            if offset + 3 >= pixels.len() {
                out.push(0);
                continue;
            }
            let r = pixels[offset] as u32;
            let g = pixels[offset + 1] as u32;
            let b = pixels[offset + 2] as u32;
            // BT.601-ish integer luminance.
            let lum = ((299 * r + 587 * g + 114 * b) / 1000).min(255) as u8;
            out.push(lum);
        }
    }
    (out, tw as u16, th as u16)
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
            bitrate_kbps: 1000, // ignored by HDC
        }
    }

    fn constant_frame(w: u32, h: u32, v: u8) -> Vec<u8> {
        let mut p = vec![0u8; (w * h * 4) as usize];
        for i in 0..(w * h) as usize {
            p[i * 4] = v;
            p[i * 4 + 1] = v;
            p[i * 4 + 2] = v;
            p[i * 4 + 3] = 255;
        }
        p
    }

    fn gradient_frame(w: u32, h: u32, seed: u8) -> Vec<u8> {
        let mut p = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 4;
                p[i] = (x as u8).wrapping_add(seed);
                p[i + 1] = (y as u8).wrapping_add(seed);
                p[i + 2] = seed.wrapping_mul(3);
                p[i + 3] = 255;
            }
        }
        p
    }

    #[test]
    fn continuous_hv_similarity_is_sane() {
        let a = ContinuousHV::from_values(vec![1.0; 4]);
        let b = ContinuousHV::from_values(vec![1.0; 4]);
        let c = ContinuousHV::from_values(vec![-1.0; 4]);
        assert!((a.similarity(&b) - 1.0).abs() < 1e-6);
        assert!((a.similarity(&c) + 1.0).abs() < 1e-6);
        let z = ContinuousHV::from_values(vec![0.0; 4]);
        assert_eq!(a.similarity(&z), 0.0);
    }

    #[test]
    fn keyframe_then_delta_roundtrip() {
        // First frame → keyframe → decoder populates canvas.
        // Second identical frame → delta with zero patches.
        // Third different frame → delta patches the changed tiles.
        let w = 128;
        let h = 128;
        let p = params(w, h);
        let mut enc = HdcEncoder::new(p);
        let mut dec = HdcDecoder::new();

        let f0 = gradient_frame(w, h, 0);
        let pkt0 = enc.encode(&f0, 0).unwrap();
        assert_eq!(pkt0.len(), 1);
        assert!(pkt0[0].is_keyframe);
        let dec0 = dec.decode(&pkt0[0]).unwrap();
        assert_eq!(dec0.len(), 1);
        assert_eq!(dec0[0].width, w);
        assert_eq!(dec0[0].height, h);

        // Identical second frame: HDC sees similarity ~1.0 for every
        // tile, so delta has zero patches. Decoded canvas is still
        // the keyframe's canvas.
        let pkt1 = enc.encode(&f0, 33).unwrap();
        assert!(!pkt1[0].is_keyframe);
        let _ = dec.decode(&pkt1[0]).unwrap();

        // Different frame: many patches.
        let f2 = gradient_frame(w, h, 50);
        let pkt2 = enc.encode(&f2, 66).unwrap();
        assert!(!pkt2[0].is_keyframe);
        let dec2 = dec.decode(&pkt2[0]).unwrap();
        assert_eq!(dec2[0].width, w);
        assert_eq!(dec2[0].height, h);
    }

    #[test]
    fn constant_frame_after_keyframe_emits_no_patches() {
        let w = 128;
        let h = 128;
        let p = params(w, h);
        let mut enc = HdcEncoder::new(p);
        let f = constant_frame(w, h, 128);
        let pkt0 = enc.encode(&f, 0).unwrap();
        // Keyframe carries all tiles.
        let body0: HdcPacket = bincode::deserialize(&pkt0[0].bytes).unwrap();
        assert_eq!(
            body0.patches.len() as u16,
            body0.tile_cols * body0.tile_rows
        );
        // Second identical frame: all tiles above the similarity
        // threshold, so zero patches.
        let pkt1 = enc.encode(&f, 33).unwrap();
        let body1: HdcPacket = bincode::deserialize(&pkt1[0].bytes).unwrap();
        assert_eq!(body1.patches.len(), 0);
    }

    #[test]
    fn encode_rejects_wrong_size() {
        let p = params(64, 64);
        let mut enc = HdcEncoder::new(p);
        let err = enc.encode(&[0u8; 32], 0).unwrap_err();
        assert!(matches!(err, CodecError::InputMismatch(_)));
    }

    #[test]
    fn delta_before_keyframe_fails() {
        let w = 64;
        let h = 64;
        let p = params(w, h);
        let mut enc = HdcEncoder::new(p);
        // Consume the keyframe; feed the NEXT (delta) packet to a
        // fresh decoder that hasn't seen it.
        let _ = enc.encode(&gradient_frame(w, h, 0), 0).unwrap();
        let delta = enc.encode(&gradient_frame(w, h, 1), 33).unwrap();
        let mut fresh = HdcDecoder::new();
        let err = fresh.decode(&delta[0]).unwrap_err();
        assert!(matches!(err, CodecError::DecodeFailed(_)));
    }

    #[test]
    fn position_hv_is_deterministic_per_seed() {
        let a = generate_position_hv(0, 42);
        let b = generate_position_hv(0, 42);
        assert!((a.similarity(&b) - 1.0).abs() < 1e-6);
        let c = generate_position_hv(1, 42);
        // Same seed different index => different HV.
        assert!(a.similarity(&c) < 0.99);
    }
}
