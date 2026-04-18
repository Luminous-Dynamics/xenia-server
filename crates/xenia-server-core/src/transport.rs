// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Transport abstractions for M0.
//!
//! Defines a minimal [`Transport`] trait and one implementation:
//! [`TcpTransport`], which ships length-prefixed sealed envelopes
//! over a single `tokio::net::TcpStream`.
//!
//! **TCP is a deliberately boring choice for M0.** It's not the
//! production transport — that's QUIC (via `iroh` or `quinn`) and
//! WebSocket (for browsers), both planned as separate crates
//! (`xenia-transport-quic`, `xenia-transport-ws`). TCP is here
//! because it's the smallest thing that can exercise the full
//! xenia-wire seal/open path through a real network syscall, which
//! is what M0's exit criterion requires.

use std::io;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Maximum envelope size this transport will accept. Guards against
/// a malicious peer sending a length prefix that would cause the
/// receiver to allocate gigabytes. 16 MiB covers any realistic
/// frame — a 4K RGBA frame is ~33 MiB, so this is actually tight
/// for uncompressed; real deployments using encoded frames (H.264
/// I-frames ~1-2 MiB) are well under.
pub const MAX_ENVELOPE_BYTES: u32 = 16 * 1024 * 1024;

/// Transport-level errors.
#[derive(Debug, Error)]
pub enum TransportError {
    /// Network I/O failure (connection reset, read timeout, etc.).
    #[error("transport I/O: {0}")]
    Io(#[from] io::Error),

    /// Peer sent a length prefix larger than [`MAX_ENVELOPE_BYTES`].
    #[error("transport: envelope too large ({0} bytes > {MAX_ENVELOPE_BYTES} byte limit)")]
    EnvelopeTooLarge(u32),

    /// Peer closed the connection mid-envelope.
    #[error("transport: connection closed mid-envelope")]
    UnexpectedEof,
}

/// A bidirectional, best-effort-reliable channel for sealed envelope
/// bytes. Implementations MUST preserve envelope boundaries — the
/// caller passes a complete `Vec<u8>` to `send_envelope` and expects
/// to receive a complete `Vec<u8>` back from `recv_envelope`.
///
/// The trait is deliberately async and takes `&mut self` so
/// implementations can back-pressure on the send side without
/// interior mutability. For concurrent send + recv, callers split
/// into halves via platform-specific APIs (e.g.
/// `tokio::io::split`).
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// Send a single sealed envelope.
    async fn send_envelope(&mut self, bytes: &[u8]) -> Result<(), TransportError>;

    /// Receive a single sealed envelope. Blocks until one arrives
    /// or the connection closes.
    async fn recv_envelope(&mut self) -> Result<Vec<u8>, TransportError>;
}

/// TCP transport: 4-byte big-endian length prefix + envelope bytes.
///
/// Not framing to any existing protocol. Just the simplest thing
/// that reliably delimits sealed envelopes on a byte stream.
pub struct TcpTransport {
    stream: TcpStream,
}

impl TcpTransport {
    /// Wrap an existing `TcpStream`.
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    /// Convenience constructor: connect to a server address.
    pub async fn connect(addr: &str) -> Result<Self, TransportError> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(Self::new(stream))
    }
}

impl Transport for TcpTransport {
    async fn send_envelope(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        let len =
            u32::try_from(bytes.len()).map_err(|_| TransportError::EnvelopeTooLarge(u32::MAX))?;
        if len > MAX_ENVELOPE_BYTES {
            return Err(TransportError::EnvelopeTooLarge(len));
        }
        self.stream.write_all(&len.to_be_bytes()).await?;
        self.stream.write_all(bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn recv_envelope(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).await.map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                TransportError::UnexpectedEof
            } else {
                TransportError::Io(e)
            }
        })?;
        let len = u32::from_be_bytes(len_buf);
        if len > MAX_ENVELOPE_BYTES {
            return Err(TransportError::EnvelopeTooLarge(len));
        }
        let mut buf = vec![0u8; len as usize];
        self.stream.read_exact(&mut buf).await.map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                TransportError::UnexpectedEof
            } else {
                TransportError::Io(e)
            }
        })?;
        Ok(buf)
    }
}
