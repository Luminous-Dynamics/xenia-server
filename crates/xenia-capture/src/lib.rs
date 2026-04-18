// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// Portions derived from `symthaea/src/swarm/rdp_capture.rs`.
// Relicensed to Apache-2.0 OR MIT for this crate by the copyright
// holder (same author); see ADR-002 for the library-vs-binary
// licensing rationale. Per VIEWER_PLAN §0.1, `rdp_capture.rs` was
// explicitly listed as a "carry wholesale" artifact for this crate;
// the pure-Rust trait + TestCapture + BlankCapture pieces below are
// extracted directly. The X11 implementation present in the
// upstream was dropped per ADR-001 Decision 2 (Wayland-only).

//! # xenia-capture
//!
//! Screen-capture abstraction for the Xenia remote-session stack.
//!
//! Implements a platform-agnostic [`ScreenCapture`] trait with four
//! implementations:
//!
//! - [`TestCapture`] — deterministic synthetic gradient frames with a
//!   per-frame-varying "active region" that simulates cursor motion.
//!   Always available. Used by unit + integration tests.
//! - [`BlankCapture`] — solid-color frames. Always available. Useful
//!   for bandwidth smoke tests where content is irrelevant.
//! - `WlrootsCapture` — wlr-screencopy-unstable-v1 on wlroots
//!   compositors (Sway, Hyprland, labwc). Feature-gated on
//!   `wayland-wlroots`. Scaffold only as of `0.0.0-m1`; the
//!   `Screencopy` WAYLAND negotiation lands in M1.2b.
//! - `PortalCapture` — xdg-desktop-portal ScreenCast on GNOME/KDE.
//!   Feature-gated on `wayland-portal`. Scaffold only as of
//!   `0.0.0-m1`; portal DBus negotiation lands in M1.2c.
//!
//! X11 is explicitly out of scope — see the repo-level
//! `docs/ADR-001-m0-architecture.md` for the reasoning. The X11
//! server's shared-buffer design permits any client to read any
//! other client's input and framebuffer, which fundamentally undoes
//! Xenia's end-to-end threat model.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

use thiserror::Error;

/// Errors surfaced by a capture backend.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// Backend-specific failure. Wrapped for logs; callers should
    /// drop the frame and retry on the next tick.
    #[error("capture backend: {0}")]
    Backend(String),

    /// The requested capture backend is not built into this binary
    /// (missing Cargo feature) or not available on the current
    /// system (compositor doesn't speak the required protocol).
    #[error("capture unavailable: {0}")]
    Unavailable(String),

    /// The compositor's user-consent prompt was denied, cancelled,
    /// or timed out. Distinct from [`CaptureError::Backend`]
    /// because callers may want to surface a UX message rather
    /// than retry.
    #[error("capture consent denied")]
    ConsentDenied,
}

/// A captured screen frame.
#[derive(Clone, Debug)]
pub struct CapturedFrame {
    /// RGBA pixel data, row-major, top-left origin, 4 bytes per
    /// pixel. Length MUST equal `width * height * 4`.
    pub pixels: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// Monitor information for multi-monitor enumeration.
#[derive(Debug, Clone)]
pub struct MonitorDescriptor {
    /// Monitor index (0-based).
    pub index: u8,
    /// Display name (e.g., "eDP-1", "HDMI-A-1", "DP-2").
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether this is the primary monitor.
    pub is_primary: bool,
    /// X offset in the virtual desktop (for multi-monitor layouts).
    pub x_offset: i32,
    /// Y offset in the virtual desktop.
    pub y_offset: i32,
}

/// Platform-agnostic screen-capture interface.
///
/// Implementations are `Send` so a capture loop can run on a
/// dedicated tokio task. They are NOT `Sync` because most underlying
/// Wayland bindings hold non-Sync state (wayland-client
/// `EventQueue`, portal DBus connections).
pub trait ScreenCapture: Send {
    /// Capture the current screen contents.
    ///
    /// Returns `Ok(Some(frame))` on success, `Ok(None)` if the
    /// backend has no new frame to report this tick (e.g., the
    /// compositor hasn't submitted one yet), and `Err(_)` on fatal
    /// failures the caller should surface to the user.
    fn capture(&mut self) -> Result<Option<CapturedFrame>, CaptureError>;

    /// Screen width in pixels.
    fn width(&self) -> u32;

    /// Screen height in pixels.
    fn height(&self) -> u32;

    /// Enumerate available monitors. Default: single monitor at
    /// `0, 0` with `(width(), height())`.
    fn enumerate_monitors(&self) -> Vec<MonitorDescriptor> {
        vec![MonitorDescriptor {
            index: 0,
            name: "default".to_string(),
            width: self.width(),
            height: self.height(),
            is_primary: true,
            x_offset: 0,
            y_offset: 0,
        }]
    }

    /// Select a specific monitor for capture. Returns `true` if the
    /// backend accepted the selection.
    fn select_monitor(&mut self, _index: u8) -> bool {
        true
    }

    /// Identifier string for the active backend. Used for
    /// observability; stable for a given crate version.
    fn backend_name(&self) -> &str;
}

// ───────────────────────── TestCapture ─────────────────────────────

/// Deterministic test capture: generates synthetic gradient frames.
///
/// Each frame has a unique pattern based on a frame counter, which
/// makes it useful for reproducible delta-detection, codec, and
/// end-to-end pipeline testing.
pub struct TestCapture {
    width: u32,
    height: u32,
    frame_counter: u64,
    /// `(x, y, size)` region that changes each frame (simulates
    /// cursor or selection-rectangle motion).
    active_region: (u32, u32, u32),
}

impl TestCapture {
    /// Create a test capture with given resolution. The active
    /// region defaults to a 64×64 square at `(100, 100)`.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame_counter: 0,
            active_region: (100, 100, 64),
        }
    }

    /// Override the active-region rectangle.
    pub fn set_active_region(&mut self, x: u32, y: u32, size: u32) {
        self.active_region = (x, y, size);
    }

    /// Current frame counter. Useful for per-test assertions.
    pub fn frame_counter(&self) -> u64 {
        self.frame_counter
    }
}

impl ScreenCapture for TestCapture {
    fn capture(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        let w = self.width as usize;
        let h = self.height as usize;
        let mut pixels = vec![0u8; w * h * 4];

        // Static background: gradient based on position (deterministic).
        for y in 0..h {
            for x in 0..w {
                let offset = (y * w + x) * 4;
                pixels[offset] = (x * 255 / w) as u8; // R: horizontal
                pixels[offset + 1] = (y * 255 / h) as u8; // G: vertical
                pixels[offset + 2] = 128; // B: constant
                pixels[offset + 3] = 255; // A: opaque
            }
        }

        // Active region: changes each frame (simulates user activity).
        let (rx, ry, rs) = self.active_region;
        let phase = (self.frame_counter % 256) as u8;
        for dy in 0..rs as usize {
            let y = ry as usize + dy;
            if y >= h {
                break;
            }
            for dx in 0..rs as usize {
                let x = rx as usize + dx;
                if x >= w {
                    break;
                }
                let offset = (y * w + x) * 4;
                pixels[offset] = phase;
                pixels[offset + 1] = phase.wrapping_add(64);
                pixels[offset + 2] = phase.wrapping_add(128);
            }
        }

        self.frame_counter += 1;

        Ok(Some(CapturedFrame {
            pixels,
            width: self.width,
            height: self.height,
        }))
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn backend_name(&self) -> &str {
        "test"
    }
}

// ───────────────────────── BlankCapture ────────────────────────────

/// Solid-color capture. Returns the same bytes every tick.
///
/// Useful for bandwidth smoke tests where content doesn't matter —
/// the encoded stream compresses to essentially nothing, so you
/// measure pure framing and transport overhead.
pub struct BlankCapture {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl BlankCapture {
    /// Create a blank capture at `width × height` filled with
    /// `(r, g, b, 255)`.
    pub fn new(width: u32, height: u32, r: u8, g: u8, b: u8) -> Self {
        let n = width as usize * height as usize;
        let mut pixels = vec![0u8; n * 4];
        for i in 0..n {
            pixels[i * 4] = r;
            pixels[i * 4 + 1] = g;
            pixels[i * 4 + 2] = b;
            pixels[i * 4 + 3] = 255;
        }
        Self {
            width,
            height,
            pixels,
        }
    }
}

impl ScreenCapture for BlankCapture {
    fn capture(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        Ok(Some(CapturedFrame {
            pixels: self.pixels.clone(),
            width: self.width,
            height: self.height,
        }))
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn backend_name(&self) -> &str {
        "blank"
    }
}

// ───────────────────────── Wayland backends (scaffold) ─────────────

/// wlr-screencopy-unstable-v1 capture for wlroots-based compositors
/// (Sway, Hyprland, labwc).
///
/// **M1 scaffold only.** The wayland-client handshake, protocol
/// negotiation, and DMA-BUF plumbing land in M1.2b. Today the type
/// exists so the `ScreenCapture` dyn-trait path compiles against a
/// future caller, and so downstream code can `#[cfg]`-select on
/// feature availability without breaking.
///
/// Requires the `wayland-wlroots` feature.
#[cfg(feature = "wayland-wlroots")]
pub struct WlrootsCapture {
    width: u32,
    height: u32,
}

#[cfg(feature = "wayland-wlroots")]
impl WlrootsCapture {
    /// Connect to the compositor and negotiate wlr-screencopy.
    /// **Currently unimplemented.**
    pub fn new() -> Result<Self, CaptureError> {
        Err(CaptureError::Unavailable(
            "wlr-screencopy backend lands in M1.2b; scaffold only".into(),
        ))
    }
}

#[cfg(feature = "wayland-wlroots")]
impl ScreenCapture for WlrootsCapture {
    fn capture(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        Err(CaptureError::Unavailable(
            "wlr-screencopy backend lands in M1.2b".into(),
        ))
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn backend_name(&self) -> &str {
        "wlroots-screencopy"
    }
}

/// xdg-desktop-portal ScreenCast capture for GNOME / KDE and other
/// portal-speaking compositors.
///
/// **M1 scaffold only.** The portal DBus handshake + PipeWire
/// stream-read loop land in M1.2c.
///
/// Requires the `wayland-portal` feature.
#[cfg(feature = "wayland-portal")]
pub struct PortalCapture {
    width: u32,
    height: u32,
}

#[cfg(feature = "wayland-portal")]
impl PortalCapture {
    /// Call the portal's `CreateSession` + `SelectSources` +
    /// `Start` interfaces. **Currently unimplemented.**
    pub fn new() -> Result<Self, CaptureError> {
        Err(CaptureError::Unavailable(
            "portal ScreenCast backend lands in M1.2c; scaffold only".into(),
        ))
    }
}

#[cfg(feature = "wayland-portal")]
impl ScreenCapture for PortalCapture {
    fn capture(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        Err(CaptureError::Unavailable(
            "portal ScreenCast backend lands in M1.2c".into(),
        ))
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn backend_name(&self) -> &str {
        "xdg-portal-screencast"
    }
}

// ───────────────────────── Tests ───────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_produces_frame_of_declared_size() {
        let mut cap = TestCapture::new(160, 120);
        let f = cap.capture().unwrap().unwrap();
        assert_eq!(f.width, 160);
        assert_eq!(f.height, 120);
        assert_eq!(f.pixels.len(), 160 * 120 * 4);
    }

    #[test]
    fn test_capture_frames_are_deterministic_but_not_identical() {
        let mut cap = TestCapture::new(64, 64);
        // Default active_region is (100, 100) — out of bounds for
        // 64×64. Place it in-bounds so successive frames differ.
        cap.set_active_region(8, 8, 16);
        let f0 = cap.capture().unwrap().unwrap();
        let f1 = cap.capture().unwrap().unwrap();
        // Different frame counter ⇒ the active region differs.
        assert_ne!(f0.pixels, f1.pixels);
        // But recreate cap and the sequence is identical.
        let mut cap2 = TestCapture::new(64, 64);
        cap2.set_active_region(8, 8, 16);
        let g0 = cap2.capture().unwrap().unwrap();
        assert_eq!(f0.pixels, g0.pixels);
    }

    #[test]
    fn blank_capture_is_idempotent() {
        let mut cap = BlankCapture::new(32, 32, 0x10, 0x20, 0x30);
        let f0 = cap.capture().unwrap().unwrap();
        let f1 = cap.capture().unwrap().unwrap();
        assert_eq!(f0.pixels, f1.pixels);
        assert_eq!(f0.pixels[0], 0x10);
        assert_eq!(f0.pixels[1], 0x20);
        assert_eq!(f0.pixels[2], 0x30);
        assert_eq!(f0.pixels[3], 255);
    }

    #[test]
    fn default_enumerate_returns_single_monitor() {
        let cap = TestCapture::new(800, 600);
        let mons = cap.enumerate_monitors();
        assert_eq!(mons.len(), 1);
        assert_eq!(mons[0].width, 800);
        assert_eq!(mons[0].height, 600);
        assert!(mons[0].is_primary);
    }
}
