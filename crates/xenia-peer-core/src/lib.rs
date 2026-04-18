// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! # xenia-peer-core
//!
//! Core session + transport primitives for `xenia-peer`. **M0 — not
//! usable as a real product yet**: no screen capture, no video encoding,
//! no input injection. What this crate gives you today:
//!
//! - [`RawFrame`] + [`RawInput`] — framing types that carry raw RGBA
//!   pixel data and opaque input bytes.
//! - [`Session`] — a thin wrapper around `xenia_wire::Session` that
//!   tracks framing state (frame ID counter, input sequence counter)
//!   alongside the wire's crypto state.
//! - [`transport::TcpTransport`] — the simplest possible network
//!   transport: length-prefixed sealed envelopes over a
//!   `tokio::net::TcpStream`. Not production — QUIC/Iroh is the
//!   primary transport for M1+.
//!
//! ## What this crate deliberately does NOT do yet
//!
//! - **No screen capture.** A real capture backend (X11 / Wayland /
//!   ScreenCaptureKit / Windows Graphics Capture) lives in
//!   `xenia-capture`, planned for M1.
//! - **No video encoding.** Raw RGBA frames only. Hardware H.264 /
//!   VP9 via `ffmpeg-next` is `xenia-video`, planned for M1.
//! - **No input injection.** Viewer-captured events are shipped but
//!   never actually applied. `xenia-inject` is M2.
//! - **No handshake.** Callers install 32-byte session keys directly
//!   via `Session::install_key`, same as `xenia_wire`. A real
//!   ML-KEM-768 handshake is deferred to a companion crate.
//!
//! ## Relationship to Symthaea's in-tree RDP stack
//!
//! The
//! [Track A viewer plan](https://github.com/Luminous-Dynamics/xenia-wire/blob/main/plans/VIEWER_PLAN.md)
//! originally framed M0 as *extracting* 11 `rdp_*.rs` modules from the
//! Symthaea research monorepo into this crate. That framing treated
//! Symthaea as the source of production-ready code. In practice,
//! `xenia-wire 0.1.0-alpha.3` already owns the load-bearing primitives
//! — AEAD seal/open, replay window, session key lifecycle, consent
//! state machine — so the *actual* cleanest M0 is to write fresh code
//! that depends on `xenia-wire` and treats Symthaea's `rdp_*` modules
//! as reference material rather than copy-paste targets.
//!
//! The result: this crate is small (a few hundred lines) instead of
//! thousands, with a much tighter dependency surface. Symthaea's
//! in-tree work on content classification, HDC tile codecs, and
//! consciousness-gated telemetry remains in Symthaea — this crate
//! stays focused on the MSP product shape.

#![warn(missing_docs)]

pub mod frame;
mod session;
pub mod transport;

pub use frame::{PixelFormat, RawFrame, RawInput};
pub use session::{Session, SessionError, SessionRole};

/// Semantic-version string for the xenia-wire crate this server
/// binds against. Exposed so the transport layer can log it on
/// connect and detect mismatches.
pub const XENIA_WIRE_VERSION: &str = "0.1.0-alpha.3";
