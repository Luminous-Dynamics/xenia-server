// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Server-side session wrapper.
//!
//! A thin layer over [`xenia_wire::Session`] that tracks
//! server-specific framing state: a monotonic frame counter on the
//! forward (server → viewer) path, a monotonic input counter on the
//! reverse path, and which role (`Server` / `Viewer`) this instance
//! plays. All AEAD + replay + consent machinery lives in
//! `xenia-wire`; this struct is thin.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use xenia_wire::{open_frame, open_input, Session as WireSession, WireError};

use crate::frame::{RawFrame, RawInput};

/// Role played by a session instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    /// Captures frames, accepts input. The side running on the
    /// machine being controlled.
    Server,
    /// Renders frames, sends input. The side running on the
    /// technician's device.
    Viewer,
}

/// Session errors surfaced from the server-side wrapper.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SessionError {
    /// Underlying `xenia-wire` operation failed.
    #[error("xenia-wire: {0}")]
    Wire(#[from] WireError),
}

/// A server or viewer session.
///
/// Wraps `xenia_wire::Session` with framing-side bookkeeping.
/// Caller installs a 32-byte AEAD key via [`Session::install_key`],
/// optionally drives the consent ceremony via
/// [`Session::observe_consent`], and then seals/opens application
/// payloads through the dedicated helpers here.
pub struct Session {
    role: SessionRole,
    wire: WireSession,
    /// Frame ID counter for outbound `RawFrame`s (server side only).
    next_frame_id: u64,
    /// Input sequence counter for outbound `RawInput`s (viewer side only).
    next_input_seq: u64,
}

impl Session {
    /// Construct a server-side session. Generates a random `source_id`
    /// and `epoch`; caller must install a key before sealing.
    pub fn server() -> Self {
        Self::with_role(SessionRole::Server)
    }

    /// Construct a viewer-side session.
    pub fn viewer() -> Self {
        Self::with_role(SessionRole::Viewer)
    }

    fn with_role(role: SessionRole) -> Self {
        Self {
            role,
            wire: WireSession::new(),
            next_frame_id: 0,
            next_input_seq: 0,
        }
    }

    /// Construct with deterministic `source_id` + `epoch`. Useful for
    /// test fixtures — MUST NOT be used in production because it
    /// breaks the per-session randomness assumption the wire relies on.
    pub fn with_fixture(role: SessionRole, source_id: [u8; 8], epoch: u8) -> Self {
        Self {
            role,
            wire: WireSession::with_source_id(source_id, epoch),
            next_frame_id: 0,
            next_input_seq: 0,
        }
    }

    /// Install a 32-byte session key. Must be called before any
    /// `seal_*` or `open_*` call.
    pub fn install_key(&mut self, key: [u8; 32]) {
        self.wire.install_key(key);
    }

    /// Forward consent events to the underlying wire session. See
    /// [`xenia_wire::Session::observe_consent`].
    pub fn observe_consent(
        &mut self,
        event: xenia_wire::consent::ConsentEvent,
    ) -> xenia_wire::consent::ConsentState {
        self.wire.observe_consent(event)
    }

    /// Current consent state from the underlying wire session.
    pub fn consent_state(&self) -> xenia_wire::consent::ConsentState {
        self.wire.consent_state()
    }

    /// Borrow the underlying wire session for advanced callers who
    /// need the raw `seal` / `open` surface (custom payload types,
    /// rekey, tick, etc.).
    pub fn wire(&mut self) -> &mut WireSession {
        &mut self.wire
    }

    /// Return this session's role.
    pub fn role(&self) -> SessionRole {
        self.role
    }

    /// Allocate the next outbound frame ID. Called internally by
    /// [`Session::seal_captured_rgba`]; exposed for callers that batch
    /// frames or need the ID before sealing.
    pub fn next_frame_id(&mut self) -> u64 {
        let id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        id
    }

    /// Allocate the next outbound input sequence.
    pub fn next_input_seq(&mut self) -> u64 {
        let seq = self.next_input_seq;
        self.next_input_seq = self.next_input_seq.wrapping_add(1);
        seq
    }

    /// Seal a captured raw-RGBA frame on the forward path.
    ///
    /// Fills in `frame_id` (from [`Session::next_frame_id`]) and
    /// `timestamp_ms` (system time) automatically.
    pub fn seal_captured_rgba(
        &mut self,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Result<Vec<u8>, SessionError> {
        let frame = RawFrame::rgba8(self.next_frame_id(), now_ms(), width, height, pixels);
        xenia_wire::seal_frame(&frame_as_wire(&frame)?, &mut self.wire).map_err(Into::into)
    }

    /// Seal a fully-constructed `RawFrame` on the forward path.
    /// Use this when you need control over frame_id / timestamp_ms
    /// (e.g. replaying a recording).
    pub fn seal_frame(&mut self, frame: &RawFrame) -> Result<Vec<u8>, SessionError> {
        xenia_wire::seal_frame(&frame_as_wire(frame)?, &mut self.wire).map_err(Into::into)
    }

    /// Open a sealed envelope as a `RawFrame` on the viewer side.
    /// Rejects envelopes whose pixel buffer doesn't match the declared
    /// width × height × format (returns `Codec(...)`).
    pub fn open_frame(&mut self, envelope: &[u8]) -> Result<RawFrame, SessionError> {
        let wire_frame = open_frame(envelope, &mut self.wire)?;
        let frame = frame_from_wire(&wire_frame)?;
        if !frame.validate() {
            return Err(SessionError::Wire(WireError::decode(
                "RawFrame buffer does not match declared dimensions",
            )));
        }
        Ok(frame)
    }

    /// Seal a viewer-captured input event on the reverse path. Fills
    /// in `sequence` (from [`Session::next_input_seq`]) and
    /// `timestamp_ms` automatically.
    pub fn seal_input_event(&mut self, payload: Vec<u8>) -> Result<Vec<u8>, SessionError> {
        let input = RawInput {
            sequence: self.next_input_seq(),
            timestamp_ms: now_ms(),
            payload,
        };
        xenia_wire::seal_input(&input_as_wire(&input)?, &mut self.wire).map_err(Into::into)
    }

    /// Open a sealed input envelope on the server side.
    pub fn open_input(&mut self, envelope: &[u8]) -> Result<RawInput, SessionError> {
        let wire_input = open_input(envelope, &mut self.wire)?;
        input_from_wire(&wire_input).map_err(Into::into)
    }
}

// ─── Bridging RawFrame/RawInput ↔ xenia_wire::Frame/Input ─────────────
//
// xenia-wire's reference `Frame` type is intentionally opaque-bytes
// (per SPEC.md §4 — the wire doesn't know about pixel formats). We
// serialize the richer `RawFrame` into `xenia_wire::Frame.payload`
// via bincode, then seal as a regular FRAME.

fn frame_as_wire(frame: &RawFrame) -> Result<xenia_wire::Frame, WireError> {
    let payload = bincode::serialize(frame).map_err(WireError::encode)?;
    Ok(xenia_wire::Frame {
        frame_id: frame.frame_id,
        timestamp_ms: frame.timestamp_ms,
        payload,
    })
}

fn frame_from_wire(wire: &xenia_wire::Frame) -> Result<RawFrame, WireError> {
    bincode::deserialize(&wire.payload).map_err(WireError::decode)
}

fn input_as_wire(input: &RawInput) -> Result<xenia_wire::Input, WireError> {
    let payload = bincode::serialize(input).map_err(WireError::encode)?;
    Ok(xenia_wire::Input {
        sequence: input.sequence,
        timestamp_ms: input.timestamp_ms,
        payload,
    })
}

fn input_from_wire(wire: &xenia_wire::Input) -> Result<RawInput, WireError> {
    bincode::deserialize(&wire.payload).map_err(WireError::decode)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_key() -> [u8; 32] {
        [0x42; 32]
    }

    fn paired() -> (Session, Session) {
        let mut server = Session::with_fixture(SessionRole::Server, [0x11; 8], 0xAB);
        let mut viewer = Session::with_fixture(SessionRole::Viewer, [0x11; 8], 0xAB);
        server.install_key(fixture_key());
        viewer.install_key(fixture_key());
        (server, viewer)
    }

    fn solid_blue(w: u32, h: u32) -> Vec<u8> {
        (0..(w * h)).flat_map(|_| [0u8, 0, 255, 255]).collect()
    }

    #[test]
    fn frame_seal_open_roundtrip_preserves_pixels() {
        let (mut server, mut viewer) = paired();
        let pixels = solid_blue(8, 4);
        let sealed = server.seal_captured_rgba(8, 4, pixels.clone()).unwrap();
        let opened = viewer.open_frame(&sealed).unwrap();
        assert_eq!(opened.width, 8);
        assert_eq!(opened.height, 4);
        assert_eq!(opened.pixels, pixels);
        assert_eq!(opened.pixel_format, crate::frame::PixelFormat::Rgba8);
        assert_eq!(opened.frame_id, 0);
    }

    #[test]
    fn frame_counter_advances_per_seal() {
        let (mut server, _viewer) = paired();
        for expected_id in 0u64..5 {
            let _ = server.seal_captured_rgba(2, 2, solid_blue(2, 2)).unwrap();
            // next_frame_id advances past what we just used.
            assert_eq!(server.next_frame_id, expected_id + 1);
        }
    }

    #[test]
    fn input_seal_open_roundtrip_preserves_payload() {
        let (mut server, mut viewer) = paired();
        let payload = br#"{"type":"mousemove","x":0.7,"y":0.3}"#.to_vec();
        let sealed = viewer.seal_input_event(payload.clone()).unwrap();
        let opened = server.open_input(&sealed).unwrap();
        assert_eq!(opened.payload, payload);
        assert_eq!(opened.sequence, 0);
    }

    #[test]
    fn open_frame_rejects_dimension_mismatch() {
        // Hand-craft a RawFrame whose declared dims don't match the
        // buffer, seal it, and verify the viewer catches it on open.
        let (mut server, mut viewer) = paired();
        let bad = RawFrame {
            frame_id: 0,
            timestamp_ms: 0,
            width: 100,
            height: 100,
            pixel_format: crate::frame::PixelFormat::Rgba8,
            pixels: vec![0u8; 10],
        };
        let sealed = server.seal_frame(&bad).unwrap();
        let err = viewer.open_frame(&sealed);
        assert!(err.is_err(), "dimension mismatch must be rejected");
    }

    #[test]
    fn role_preserved() {
        let server = Session::server();
        let viewer = Session::viewer();
        assert_eq!(server.role(), SessionRole::Server);
        assert_eq!(viewer.role(), SessionRole::Viewer);
    }
}
