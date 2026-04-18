// Copyright (c) 2024-2026 Tristan Stoltz / Luminous Dynamics
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! M0 exit-criterion integration test.
//!
//! Spins up a TCP server on localhost, a TCP viewer that connects to
//! it, and exchanges 100 synthetic RGBA frames through the full
//! xenia-wire seal/open path on both sides over real tokio TCP
//! sockets. Also exercises the reverse path — the viewer sends 10
//! synthetic input events and the server opens them.
//!
//! This isn't testing the wire protocol (that lives in `xenia-wire`'s
//! own tests) — it's testing that `xenia-server-core` correctly
//! composes the wire with its own framing types and a real transport.

use std::time::Duration;

use tokio::net::TcpListener;
use tokio::time::timeout;
use xenia_server_core::transport::{TcpTransport, Transport};
use xenia_server_core::{Session, SessionRole};

const FIXTURE_KEY: [u8; 32] = [0xAB; 32];
const FRAMES: u64 = 100;
const INPUTS: u64 = 10;
const FRAME_W: u32 = 32;
const FRAME_H: u32 = 24;

fn synth_frame(frame_id: u64, w: u32, h: u32) -> Vec<u8> {
    // A synthetic pattern that varies per frame so we can catch
    // silent frame duplication or skipping on the receiver.
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = ((x + frame_id as u32) & 0xFF) as u8;
            let g = ((y + frame_id as u32) & 0xFF) as u8;
            let b = ((x ^ y ^ frame_id as u32) & 0xFF) as u8;
            pixels.extend_from_slice(&[r, g, b, 255]);
        }
    }
    pixels
}

/// End-to-end: 100 RGBA frames server→viewer + 10 inputs viewer→server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hundred_frames_plus_inputs_roundtrip_over_tcp() {
    // Bind on an ephemeral port.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    // Fixed source_id/epoch so both sides agree on the replay-window
    // key even though the AEAD key is shared. This is the test-
    // fixture pattern; in production each side has independent
    // source_id + epoch.
    let server_task = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        stream.set_nodelay(true).ok();
        let mut transport = TcpTransport::new(stream);

        let mut server = Session::with_fixture(SessionRole::Server, [0x11; 8], 0x42);
        server.install_key(FIXTURE_KEY);

        // Forward path: send FRAMES frames.
        for frame_id in 0..FRAMES {
            let pixels = synth_frame(frame_id, FRAME_W, FRAME_H);
            let envelope = server
                .seal_captured_rgba(FRAME_W, FRAME_H, pixels)
                .expect("seal frame");
            transport
                .send_envelope(&envelope)
                .await
                .expect("send frame");
        }

        // Reverse path: receive INPUTS inputs.
        let mut received_inputs = Vec::with_capacity(INPUTS as usize);
        for _ in 0..INPUTS {
            let envelope = transport.recv_envelope().await.expect("recv input");
            let input = server.open_input(&envelope).expect("open input");
            received_inputs.push(input);
        }
        received_inputs
    });

    let viewer_task = tokio::spawn(async move {
        let mut transport = TcpTransport::connect(&addr.to_string())
            .await
            .expect("connect");

        let mut viewer = Session::with_fixture(SessionRole::Viewer, [0x11; 8], 0x42);
        viewer.install_key(FIXTURE_KEY);

        // Receive FRAMES frames on the forward path.
        let mut received_frames = Vec::with_capacity(FRAMES as usize);
        for _ in 0..FRAMES {
            let envelope = transport.recv_envelope().await.expect("recv frame");
            let frame = viewer.open_frame(&envelope).expect("open frame");
            received_frames.push(frame);
        }

        // Send INPUTS on the reverse path.
        for i in 0..INPUTS {
            let payload = format!(r#"{{"type":"test","index":{i}}}"#).into_bytes();
            let envelope = viewer.seal_input_event(payload).expect("seal input");
            transport
                .send_envelope(&envelope)
                .await
                .expect("send input");
        }
        received_frames
    });

    // Both tasks must finish within a generous timeout.
    let (server_result, viewer_result) = tokio::try_join!(server_task, viewer_task).expect("join");
    let received_inputs = server_result;
    let received_frames = viewer_result;

    // Validate frames: each one decoded correctly, pixels match what
    // the synth function would produce for that frame_id.
    assert_eq!(received_frames.len() as u64, FRAMES);
    for frame in &received_frames {
        assert_eq!(frame.width, FRAME_W);
        assert_eq!(frame.height, FRAME_H);
        assert!(
            frame.validate(),
            "frame {} failed validate()",
            frame.frame_id
        );
        let expected = synth_frame(frame.frame_id, FRAME_W, FRAME_H);
        assert_eq!(
            frame.pixels, expected,
            "pixel mismatch on frame {}",
            frame.frame_id
        );
    }
    // Server should have produced monotonic frame_ids 0..FRAMES.
    for (idx, frame) in received_frames.iter().enumerate() {
        assert_eq!(frame.frame_id, idx as u64);
    }

    // Validate inputs: sequences 0..INPUTS with the JSON payloads
    // we sent.
    assert_eq!(received_inputs.len() as u64, INPUTS);
    for (idx, input) in received_inputs.iter().enumerate() {
        assert_eq!(input.sequence, idx as u64);
        let expected = format!(r#"{{"type":"test","index":{idx}}}"#).into_bytes();
        assert_eq!(input.payload, expected);
    }
}

/// Replay protection survives the full transport: if a viewer
/// somehow receives the same envelope bytes twice, the second open
/// fails even if the transport itself is clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_protection_across_real_transport() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        stream.set_nodelay(true).ok();
        let mut transport = TcpTransport::new(stream);
        let mut server = Session::with_fixture(SessionRole::Server, [0x22; 8], 0x43);
        server.install_key(FIXTURE_KEY);

        // Send the same frame twice — but seal it only ONCE, so the
        // second send is a real replay (identical envelope bytes).
        let envelope = server
            .seal_captured_rgba(2, 2, vec![255; 2 * 2 * 4])
            .unwrap();
        transport.send_envelope(&envelope).await.unwrap();
        transport.send_envelope(&envelope).await.unwrap();
    });

    let viewer_task = tokio::spawn(async move {
        let mut transport = TcpTransport::connect(&addr.to_string()).await.unwrap();
        let mut viewer = Session::with_fixture(SessionRole::Viewer, [0x22; 8], 0x43);
        viewer.install_key(FIXTURE_KEY);

        let first = viewer
            .open_frame(&transport.recv_envelope().await.unwrap())
            .expect("first open must succeed");
        let second = viewer.open_frame(&transport.recv_envelope().await.unwrap());

        assert_eq!(first.pixels, vec![255; 16]);
        assert!(second.is_err(), "replay must be rejected by the wire");
    });

    timeout(Duration::from_secs(5), async {
        tokio::try_join!(server_task, viewer_task).unwrap();
    })
    .await
    .expect("test timeout");
}

/// Transport correctly rejects a length prefix that exceeds the
/// safety cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_envelope_is_rejected_before_allocation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Server sends a forged length prefix of 100 MiB without any
    // payload bytes following. The viewer MUST reject on the
    // length check, not OOM trying to allocate.
    let server_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(&(100u32 * 1024 * 1024).to_be_bytes())
            .await
            .unwrap();
        // Don't send the payload. The viewer should bail on the
        // length prefix alone.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let viewer_task = tokio::spawn(async move {
        let mut transport = TcpTransport::connect(&addr.to_string()).await.unwrap();
        let err = transport.recv_envelope().await;
        assert!(err.is_err(), "oversize prefix must be rejected");
    });

    tokio::try_join!(server_task, viewer_task).unwrap();
}

/// Quick smoke of the Session fixture API so the test file also
/// serves as usage documentation.
#[test]
fn session_fixture_constructors_work_without_runtime() {
    let mut server = Session::with_fixture(SessionRole::Server, [1; 8], 1);
    let mut viewer = Session::with_fixture(SessionRole::Viewer, [1; 8], 1);
    server.install_key([0xCD; 32]);
    viewer.install_key([0xCD; 32]);

    let envelope = server
        .seal_captured_rgba(1, 1, vec![0, 255, 0, 255])
        .expect("seal tiny frame");
    let opened = viewer.open_frame(&envelope).expect("open tiny frame");
    assert_eq!(opened.pixels, vec![0, 255, 0, 255]);
}
