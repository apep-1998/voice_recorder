//! Wire protocol for the live audio fan-out socket.
//!
//! External programs (a "hey jarvis" wake-word detector, a transcriber, …)
//! connect to a Unix socket and receive the live PCM stream of one or more
//! devices *while* the recorder keeps writing segments — the listener is just
//! another subscriber of the in-process frame bus, so it can never block or
//! corrupt recording.
//!
//! Handshake: the client sends one line of JSON, the server replies with one
//! line of JSON, then the server streams binary frames until the client
//! disconnects.
//!
//! ```text
//! client → server (one JSON line, newline-terminated):
//!   {"v":1,"subscribe":["default-mic"],"format":"s16le","rate":16000,"channels":1}
//! server → client (one JSON line):
//!   {"v":1,"ok":true,"streams":[{"id":0,"device":"alsa_input...","rate":16000,"channels":1}]}
//! server → client (repeated, binary):
//!   [32-byte FrameHeader][payload: little-endian PCM]
//! ```

use serde::{Deserialize, Serialize};

/// Protocol version. Bumped on any incompatible wire change.
pub const PROTOCOL_VERSION: u8 = 1;

/// Magic bytes at the start of every binary frame header: `b"VRFA"`.
pub const FRAME_MAGIC: [u8; 4] = *b"VRFA";

/// Size of the fixed binary frame header in bytes.
pub const FRAME_HEADER_LEN: usize = 32;

/// PCM sample format a client can request. The server converts from its
/// internal f32 as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SampleFormat {
    /// 16-bit signed little-endian (what most wake-word / STT engines want).
    S16le,
    /// 32-bit float little-endian (lossless passthrough of the bus format).
    F32le,
}

impl SampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::S16le => 2,
            SampleFormat::F32le => 4,
        }
    }
}

/// The subscribe request a client sends as its first line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscribeRequest {
    pub v: u8,
    /// Device slug substrings to subscribe to (empty = all recorded devices).
    #[serde(default)]
    pub subscribe: Vec<String>,
    #[serde(default = "default_format")]
    pub format: SampleFormat,
    /// Desired sample rate; the server resamples. 0 or absent = native 48000.
    #[serde(default)]
    pub rate: u32,
    /// Desired channel count (1 downmixes). 0 or absent = native.
    #[serde(default)]
    pub channels: u8,
}

fn default_format() -> SampleFormat {
    SampleFormat::S16le
}

/// One subscribed stream, as resolved by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub id: u16,
    pub device: String,
    pub rate: u32,
    pub channels: u8,
}

/// The server's handshake reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscribeReply {
    pub v: u8,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub streams: Vec<StreamInfo>,
}

impl SubscribeReply {
    pub fn ok(streams: Vec<StreamInfo>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            error: None,
            streams,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            error: Some(msg.into()),
            streams: Vec::new(),
        }
    }
}

/// Binary frame type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// PCM audio payload follows.
    Pcm,
    /// The server dropped `seq_gap` frames for this slow client; no payload.
    Drop,
}

impl FrameType {
    fn to_u8(self) -> u8 {
        match self {
            FrameType::Pcm => 1,
            FrameType::Drop => 2,
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(FrameType::Pcm),
            2 => Some(FrameType::Drop),
            _ => None,
        }
    }
}

/// The fixed 32-byte binary header preceding each frame's payload.
///
/// Layout (little-endian):
/// ```text
///  0..4   magic "VRFA"
///  4      version
///  5      frame type (1=PCM, 2=DROP)
///  6..8   stream id (u16)
///  8..16  utc_ns (u64) — UTC of the first sample
/// 16..20  n_samples (u32) — samples per channel in the payload
/// 20..24  payload_len (u32) — payload bytes following the header
/// 24..32  seq (u64) — per-stream sequence (DROP carries the gap count here)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_type: FrameType,
    pub stream_id: u16,
    pub utc_ns: u64,
    pub n_samples: u32,
    pub payload_len: u32,
    pub seq: u64,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut b = [0_u8; FRAME_HEADER_LEN];
        b[0..4].copy_from_slice(&FRAME_MAGIC);
        b[4] = PROTOCOL_VERSION;
        b[5] = self.frame_type.to_u8();
        b[6..8].copy_from_slice(&self.stream_id.to_le_bytes());
        b[8..16].copy_from_slice(&self.utc_ns.to_le_bytes());
        b[16..20].copy_from_slice(&self.n_samples.to_le_bytes());
        b[20..24].copy_from_slice(&self.payload_len.to_le_bytes());
        b[24..32].copy_from_slice(&self.seq.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < FRAME_HEADER_LEN || b[0..4] != FRAME_MAGIC {
            return None;
        }
        Some(Self {
            frame_type: FrameType::from_u8(b[5])?,
            stream_id: u16::from_le_bytes([b[6], b[7]]),
            utc_ns: u64::from_le_bytes(b[8..16].try_into().ok()?),
            n_samples: u32::from_le_bytes(b[16..20].try_into().ok()?),
            payload_len: u32::from_le_bytes(b[20..24].try_into().ok()?),
            seq: u64::from_le_bytes(b[24..32].try_into().ok()?),
        })
    }
}

/// Convert interleaved f32 samples to the requested wire format.
pub fn encode_samples(samples: &[f32], format: SampleFormat) -> Vec<u8> {
    match format {
        SampleFormat::F32le => samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
        SampleFormat::S16le => samples
            .iter()
            .flat_map(|s| {
                // Value is clamped to [-1, 1] then scaled into i16 range, so
                // the cast cannot overflow.
                #[allow(clippy::cast_possible_truncation)]
                let clamped = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                clamped.to_le_bytes()
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_round_trips() {
        let h = FrameHeader {
            frame_type: FrameType::Pcm,
            stream_id: 3,
            utc_ns: 1_791_000_000_000_000_000,
            n_samples: 960,
            payload_len: 1920,
            seq: 42,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), FRAME_HEADER_LEN);
        assert_eq!(&bytes[0..4], b"VRFA");
        assert_eq!(FrameHeader::decode(&bytes), Some(h));
    }

    #[test]
    fn drop_frame_round_trips() {
        let h = FrameHeader {
            frame_type: FrameType::Drop,
            stream_id: 0,
            utc_ns: 0,
            n_samples: 0,
            payload_len: 0,
            seq: 17, // gap count
        };
        assert_eq!(FrameHeader::decode(&h.encode()), Some(h));
    }

    #[test]
    fn decode_rejects_bad_magic_and_short_input() {
        let mut bytes = FrameHeader {
            frame_type: FrameType::Pcm,
            stream_id: 0,
            utc_ns: 0,
            n_samples: 0,
            payload_len: 0,
            seq: 0,
        }
        .encode();
        bytes[0] = b'X';
        assert_eq!(FrameHeader::decode(&bytes), None);
        assert_eq!(FrameHeader::decode(&[0_u8; 4]), None);
    }

    #[test]
    fn subscribe_request_parses_with_defaults() {
        let req: SubscribeRequest = serde_json::from_str(r#"{"v":1,"subscribe":["mic"]}"#).unwrap();
        assert_eq!(req.v, 1);
        assert_eq!(req.subscribe, vec!["mic"]);
        assert_eq!(req.format, SampleFormat::S16le);
        assert_eq!(req.rate, 0);
    }

    #[test]
    fn reply_serializes_ok_and_error() {
        let ok = SubscribeReply::ok(vec![StreamInfo {
            id: 0,
            device: "mic".into(),
            rate: 16000,
            channels: 1,
        }]);
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(!json.contains("error"));

        let err = SubscribeReply::error("no such device");
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("no such device"));
    }

    #[test]
    fn s16le_encoding_scales_and_clamps() {
        let out = encode_samples(&[0.0, 1.0, -1.0, 2.0], SampleFormat::S16le);
        assert_eq!(out.len(), 8);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 0);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([out[4], out[5]]), -i16::MAX);
        // 2.0 clamps to +1.0 full scale.
        assert_eq!(i16::from_le_bytes([out[6], out[7]]), i16::MAX);
    }

    #[test]
    fn f32le_encoding_is_passthrough() {
        let out = encode_samples(&[0.5, -0.25], SampleFormat::F32le);
        assert_eq!(out.len(), 8);
        assert_eq!(f32::from_le_bytes([out[0], out[1], out[2], out[3]]), 0.5);
    }
}
