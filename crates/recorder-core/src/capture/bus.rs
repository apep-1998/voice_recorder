//! The frame bus: capture publishes, everyone else subscribes.
//!
//! This is the listener-fan-out foundation: the Opus encoder, the debug WAV
//! dumper, and the future Unix-socket fan-out server are all just
//! subscribers of the same `tokio::sync::broadcast` channel. A slow
//! subscriber lags and loses frames (it is told how many); it can never
//! block capture.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::device::StreamKind;
use crate::timeline::UtcNs;

/// Default bus capacity in events. At typical PipeWire quantum sizes
/// (~1024 frames ≈ 21 ms at 48 kHz) this buffers roughly 20 s per device.
pub const DEFAULT_BUS_CAPACITY: usize = 1024;

/// A chunk of captured PCM audio.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Storage slug of the device this came from.
    pub slug: Arc<str>,
    pub kind: StreamKind,
    /// UTC timestamp of the first sample in this frame.
    pub utc_ns: UtcNs,
    /// Per-device monotonically increasing frame counter.
    pub seq: u64,
    /// Sample rate in Hz.
    pub rate: u32,
    /// Interleaved channel count.
    pub channels: u8,
    /// Interleaved f32 samples; length is a multiple of `channels`.
    pub samples: Arc<[f32]>,
}

impl AudioFrame {
    /// Number of frames (samples per channel).
    pub fn frame_count(&self) -> u64 {
        self.samples.len() as u64 / u64::from(self.channels)
    }
}

/// Everything that flows over the bus.
#[derive(Debug, Clone)]
pub enum BusEvent {
    /// Captured audio.
    Frame(AudioFrame),
    /// The capture clock jumped (suspend, clock step, stall). Subscribers
    /// writing segments must finalize and re-anchor; `frame.utc_ns` of the
    /// next `Frame` event carries the new anchor.
    Discontinuity {
        slug: Arc<str>,
        kind: StreamKind,
        drift_ns: u64,
    },
    /// Capture for this device stopped (device removed or engine shutdown).
    StreamClosed { slug: Arc<str> },
}

impl BusEvent {
    pub fn slug(&self) -> &Arc<str> {
        match self {
            BusEvent::Frame(frame) => &frame.slug,
            BusEvent::Discontinuity { slug, .. } | BusEvent::StreamClosed { slug } => slug,
        }
    }
}

/// Cloneable handle for publishing to and subscribing on the bus.
#[derive(Debug, Clone)]
pub struct FrameBus {
    sender: broadcast::Sender<BusEvent>,
}

impl FrameBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an event. Returns the number of current subscribers (0 is
    /// fine — recording may start before any subscriber attaches).
    pub fn publish(&self, event: BusEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.sender.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for FrameBus {
    fn default() -> Self {
        Self::new(DEFAULT_BUS_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    fn frame(seq: u64) -> BusEvent {
        BusEvent::Frame(AudioFrame {
            slug: Arc::from("test-device"),
            kind: StreamKind::Mic,
            utc_ns: 1_000 + seq,
            seq,
            rate: 48_000,
            channels: 1,
            samples: Arc::from(vec![0.0_f32; 480]),
        })
    }

    #[test]
    fn subscribers_receive_published_frames() {
        let bus = FrameBus::new(16);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        assert_eq!(bus.publish(frame(0)), 2);
        assert_eq!(bus.publish(frame(1)), 2);

        for receiver in [&mut a, &mut b] {
            for expected_seq in 0..2 {
                match receiver.try_recv().expect("event available") {
                    BusEvent::Frame(f) => assert_eq!(f.seq, expected_seq),
                    other => panic!("unexpected event {other:?}"),
                }
            }
        }
    }

    #[test]
    fn publish_without_subscribers_is_fine() {
        let bus = FrameBus::new(16);
        assert_eq!(bus.publish(frame(0)), 0);
    }

    #[test]
    fn slow_subscriber_lags_instead_of_blocking() {
        let bus = FrameBus::new(4);
        let mut rx = bus.subscribe();
        for seq in 0..10 {
            bus.publish(frame(seq));
        }
        // The receiver missed the oldest events and is told how many.
        match rx.try_recv() {
            Err(TryRecvError::Lagged(n)) => assert!(n > 0),
            other => panic!("expected lag, got {other:?}"),
        }
        // After the lag notice it resumes from the oldest retained event.
        assert!(matches!(rx.try_recv(), Ok(BusEvent::Frame(_))));
    }

    #[test]
    fn blocking_recv_works_without_a_runtime() {
        let bus = FrameBus::new(16);
        let mut rx = bus.subscribe();
        bus.publish(BusEvent::StreamClosed {
            slug: Arc::from("gone"),
        });
        match rx.blocking_recv() {
            Ok(BusEvent::StreamClosed { slug }) => assert_eq!(&*slug, "gone"),
            other => panic!("unexpected {other:?}"),
        }
        drop(bus);
        assert!(matches!(rx.blocking_recv(), Err(RecvError::Closed)));
    }

    #[test]
    fn frame_count_accounts_for_channels() {
        let f = AudioFrame {
            slug: Arc::from("x"),
            kind: StreamKind::Monitor,
            utc_ns: 0,
            seq: 0,
            rate: 48_000,
            channels: 2,
            samples: Arc::from(vec![0.0_f32; 960]),
        };
        assert_eq!(f.frame_count(), 480);
    }
}
