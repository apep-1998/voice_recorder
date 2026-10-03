//! The fan-out server: a Unix-socket endpoint that streams the live audio of
//! selected devices to external listeners.
//!
//! Each client is handled by its own task that subscribes to the in-process
//! [`FrameBus`]. A slow client lags the broadcast channel and we translate
//! that lag into an explicit `DROP` frame instead of ever blocking capture or
//! the encoder.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};

use crate::capture::bus::{BusEvent, FrameBus};
use crate::error::FanoutError;
use crate::fanout::protocol::{
    encode_samples, FrameHeader, FrameType, StreamInfo, SubscribeReply, SubscribeRequest,
    PROTOCOL_VERSION,
};

/// Run the fan-out server on `socket_path` until `shutdown` flips to true.
/// `known_slugs` resolves subscribe substrings to device slugs.
pub async fn serve(
    socket_path: PathBuf,
    bus: FrameBus,
    known_slugs: Arc<Vec<String>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), FanoutError> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| FanoutError::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    // A stale socket from a previous run would block binding.
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path).map_err(|source| FanoutError::Io {
        path: socket_path.clone(),
        source,
    })?;
    tracing::info!(socket = %socket_path.display(), "fan-out server listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let bus = bus.clone();
                        let slugs = Arc::clone(&known_slugs);
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, bus, &slugs, shutdown).await {
                                tracing::debug!("fan-out client ended: {err}");
                            }
                        });
                    }
                    Err(err) => tracing::warn!("fan-out accept failed: {err}"),
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    bus: FrameBus,
    known_slugs: &[String],
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), FanoutError> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Read one newline-terminated JSON request line.
    let mut line = Vec::new();
    read_line(&mut reader, &mut line).await?;
    let request: SubscribeRequest =
        serde_json::from_slice(&line).map_err(|e| FanoutError::Protocol(e.to_string()))?;
    if request.v != PROTOCOL_VERSION {
        let reply = SubscribeReply::error(format!("unsupported protocol version {}", request.v));
        write_json_line(&mut write_half, &reply).await?;
        return Ok(());
    }

    // Resolve subscribed slugs (empty = all).
    let selected: Vec<String> = if request.subscribe.is_empty() {
        known_slugs.to_vec()
    } else {
        known_slugs
            .iter()
            .filter(|s| request.subscribe.iter().any(|sub| s.contains(sub)))
            .cloned()
            .collect()
    };
    if selected.is_empty() {
        let reply = SubscribeReply::error("no matching devices");
        write_json_line(&mut write_half, &reply).await?;
        return Ok(());
    }

    let streams: Vec<StreamInfo> = selected
        .iter()
        .enumerate()
        .map(|(i, slug)| StreamInfo {
            id: u16::try_from(i).unwrap_or(u16::MAX),
            device: slug.clone(),
            // rate/channels of 0 in the request mean "native"; we report
            // native here and convert only the format (rate/ch conversion is
            // a future refinement).
            rate: if request.rate == 0 {
                48_000
            } else {
                request.rate
            },
            channels: request.channels,
        })
        .collect();
    write_json_line(&mut write_half, &SubscribeReply::ok(streams)).await?;

    let stream_id_of = |slug: &str| -> Option<u16> {
        selected
            .iter()
            .position(|s| s == slug)
            .map(|i| u16::try_from(i).unwrap_or(u16::MAX))
    };

    let mut rx = bus.subscribe();
    let mut seq: Vec<u64> = vec![0; selected.len()];

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            // Detect client disconnect: a readable half that returns 0 bytes.
            read = reader.read_u8() => {
                if read.is_err() {
                    return Ok(()); // client closed
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(BusEvent::Frame(frame)) => {
                        let Some(id) = stream_id_of(&frame.slug) else { continue };
                        let payload = encode_samples(&frame.samples, request.format);
                        let header = FrameHeader {
                            frame_type: FrameType::Pcm,
                            stream_id: id,
                            utc_ns: frame.utc_ns,
                            n_samples: u32::try_from(frame.frame_count()).unwrap_or(0),
                            payload_len: u32::try_from(payload.len()).unwrap_or(0),
                            seq: seq[id as usize],
                        };
                        seq[id as usize] += 1;
                        write_half.write_all(&header.encode()).await
                            .map_err(|source| FanoutError::Io { path: PathBuf::new(), source })?;
                        write_half.write_all(&payload).await
                            .map_err(|source| FanoutError::Io { path: PathBuf::new(), source })?;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        // Tell the client how many frames it missed (stream 0).
                        let header = FrameHeader {
                            frame_type: FrameType::Drop,
                            stream_id: 0,
                            utc_ns: 0,
                            n_samples: 0,
                            payload_len: 0,
                            seq: missed,
                        };
                        write_half.write_all(&header.encode()).await
                            .map_err(|source| FanoutError::Io { path: PathBuf::new(), source })?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

async fn read_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    out: &mut Vec<u8>,
) -> Result<(), FanoutError> {
    loop {
        let byte = reader.read_u8().await.map_err(|source| FanoutError::Io {
            path: PathBuf::new(),
            source,
        })?;
        if byte == b'\n' {
            return Ok(());
        }
        out.push(byte);
        if out.len() > 64 * 1024 {
            return Err(FanoutError::Protocol("request line too long".into()));
        }
    }
}

async fn write_json_line<T: serde::Serialize>(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> Result<(), FanoutError> {
    let mut line = serde_json::to_vec(value).map_err(|e| FanoutError::Protocol(e.to_string()))?;
    line.push(b'\n');
    write_half
        .write_all(&line)
        .await
        .map_err(|source| FanoutError::Io {
            path: PathBuf::new(),
            source,
        })
}
