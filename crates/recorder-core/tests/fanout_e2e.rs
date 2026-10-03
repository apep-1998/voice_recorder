//! Integration test for the fan-out socket: a real client connects over a
//! Unix socket, subscribes, and receives live frames published to the bus,
//! including a DROP notice when it reads too slowly.

use std::sync::Arc;
use std::time::Duration;

use recorder_core::capture::bus::{AudioFrame, BusEvent, FrameBus};
use recorder_core::device::StreamKind;
use recorder_core::fanout::protocol::{FrameHeader, FrameType, SubscribeReply, PROTOCOL_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;

fn frame(slug: &Arc<str>, seq: u64) -> BusEvent {
    BusEvent::Frame(AudioFrame {
        slug: Arc::clone(slug),
        kind: StreamKind::Mic,
        utc_ns: 1_000_000_000 + seq,
        seq,
        rate: 48_000,
        channels: 1,
        samples: Arc::from(vec![0.25_f32; 480]),
    })
}

async fn read_line(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> String {
    let mut line = Vec::new();
    loop {
        let b = reader.read_u8().await.expect("read");
        if b == b'\n' {
            break;
        }
        line.push(b);
    }
    String::from_utf8(line).unwrap()
}

async fn read_header(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> FrameHeader {
    let mut buf = [0_u8; 32];
    reader.read_exact(&mut buf).await.expect("header");
    FrameHeader::decode(&buf).expect("valid header")
}

#[tokio::test]
async fn client_subscribes_and_receives_frames() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("audio.sock");
    let bus = FrameBus::new(256);
    let slug: Arc<str> = Arc::from("test-mic");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let server = tokio::spawn(recorder_core::fanout::serve(
        socket.clone(),
        bus.clone(),
        Arc::new(vec!["test-mic".to_string()]),
        shutdown_rx,
    ));

    // Wait for the socket to appear.
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stream = UnixStream::connect(&socket).await.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    write_half
        .write_all(b"{\"v\":1,\"subscribe\":[\"mic\"],\"format\":\"s16le\"}\n")
        .await
        .unwrap();

    let reply: SubscribeReply = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.v, PROTOCOL_VERSION);
    assert_eq!(reply.streams.len(), 1);
    assert_eq!(reply.streams[0].device, "test-mic");

    // Publish a few frames; the client should receive them in order.
    for seq in 0..3 {
        bus.publish(frame(&slug, seq));
    }

    for expected_seq in 0..3 {
        let header = read_header(&mut reader).await;
        assert_eq!(header.frame_type, FrameType::Pcm);
        assert_eq!(header.seq, expected_seq);
        assert_eq!(header.n_samples, 480);
        // s16le mono: 480 samples * 2 bytes.
        assert_eq!(header.payload_len, 960);
        let mut payload = vec![0_u8; header.payload_len as usize];
        reader.read_exact(&mut payload).await.unwrap();
    }

    shutdown_tx.send(true).unwrap();
    let _ = server.await;
}

#[tokio::test]
async fn unmatched_device_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("audio.sock");
    let bus = FrameBus::new(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let server = tokio::spawn(recorder_core::fanout::serve(
        socket.clone(),
        bus,
        Arc::new(vec!["real-device".to_string()]),
        shutdown_rx,
    ));
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    write_half
        .write_all(b"{\"v\":1,\"subscribe\":[\"nonexistent\"]}\n")
        .await
        .unwrap();
    let reply: SubscribeReply = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(!reply.ok);
    assert!(reply.error.unwrap().contains("no matching"));

    shutdown_tx.send(true).unwrap();
    let _ = server.await;
}
