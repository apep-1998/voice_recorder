//! Per-device segment sink: subscribes to the frame bus and writes rotating
//! Ogg/Opus segments for one device, maintaining the index as it goes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, watch};

use crate::capture::bus::{AudioFrame, BusEvent};
use crate::device::StreamKind;
use crate::encode::SegmentWriter;
use crate::error::RecorderError;
use crate::store::index::{Index, SegmentRecord, SessionRecord};
use crate::store::layout::{segment_rel_path, StorageLayout};
use crate::timeline::UtcNs;

/// Encoding parameters for one sink.
#[derive(Debug, Clone, Copy)]
pub struct SinkSettings {
    pub bitrate: u32,
    pub segment_duration: Duration,
}

/// Why the current segment was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseReason {
    Rotation,
    Discontinuity,
    StreamClosed,
    Shutdown,
    Flush,
}

/// Runs until the bus closes, the stream closes, or `shutdown` flips to true.
/// Returns the number of segments finalized.
pub async fn run_sink(
    mut rx: broadcast::Receiver<BusEvent>,
    slug: Arc<str>,
    kind: StreamKind,
    layout: StorageLayout,
    index: Arc<Mutex<Index>>,
    settings: SinkSettings,
    mut shutdown: watch::Receiver<bool>,
) -> Result<u64, RecorderError> {
    let mut sink = Sink {
        slug,
        kind,
        layout,
        index,
        settings,
        current: None,
        session_id: new_session_id(),
        session_recorded: false,
        finalized: 0,
    };

    loop {
        let event = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    sink.close_current(CloseReason::Shutdown)?;
                    return Ok(sink.finalized);
                }
                continue;
            }
            event = rx.recv() => event,
        };
        match event {
            Ok(BusEvent::Frame(frame)) => {
                if frame.slug == sink.slug {
                    sink.on_frame(&frame)?;
                }
            }
            Ok(BusEvent::Discontinuity { slug, .. }) => {
                if slug == sink.slug {
                    sink.close_current(CloseReason::Discontinuity)?;
                    sink.session_id = new_session_id();
                    sink.session_recorded = false;
                }
            }
            Ok(BusEvent::StreamClosed { slug }) => {
                if slug == sink.slug {
                    sink.close_current(CloseReason::StreamClosed)?;
                }
            }
            Ok(BusEvent::FlushAll) => {
                // Finalize before suspend; the session continues (a real gap,
                // if any, is caught by the drift detector on resume).
                sink.close_current(CloseReason::Flush)?;
            }
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(slug = %sink.slug, missed, "sink lagged; audio dropped");
                // The dropped frames leave a hole; close the segment so the
                // gap is represented as absence rather than time-shifted audio.
                sink.close_current(CloseReason::Discontinuity)?;
            }
            Err(broadcast::error::RecvError::Closed) => {
                sink.close_current(CloseReason::Shutdown)?;
                return Ok(sink.finalized);
            }
        }
    }
}

struct OpenSegment {
    writer: SegmentWriter,
    utc_start_ns: UtcNs,
    rel_path: String,
    rate: u32,
    channels: u8,
    target_frames: u64,
}

struct Sink {
    slug: Arc<str>,
    kind: StreamKind,
    layout: StorageLayout,
    index: Arc<Mutex<Index>>,
    settings: SinkSettings,
    current: Option<OpenSegment>,
    session_id: String,
    session_recorded: bool,
    finalized: u64,
}

impl Sink {
    fn on_frame(&mut self, frame: &AudioFrame) -> Result<(), RecorderError> {
        if self.current.is_none() {
            self.open_segment(frame)?;
        }
        let segment = self.current.as_mut().expect("segment just opened");

        // A format change mid-stream needs a fresh segment.
        if segment.rate != frame.rate || segment.channels != frame.channels {
            self.close_current(CloseReason::Discontinuity)?;
            self.open_segment(frame)?;
        }
        let segment = self.current.as_mut().expect("segment open");
        segment.writer.write_samples(&frame.samples)?;

        if segment.writer.n_frames() >= segment.target_frames {
            self.close_current(CloseReason::Rotation)?;
        }
        Ok(())
    }

    fn open_segment(&mut self, frame: &AudioFrame) -> Result<(), RecorderError> {
        if !self.session_recorded {
            let index = self.index.lock().expect("index lock");
            index.record_session(&SessionRecord {
                id: self.session_id.clone(),
                started_utc_ns: frame.utc_ns,
                reason: "capture-start".into(),
            })?;
            drop(index);
            self.session_recorded = true;
        }
        let rel_path = segment_rel_path(&self.slug, frame.utc_ns, &self.session_id);
        let abs_path = self.layout.root().join(&rel_path);
        let writer = SegmentWriter::create(abs_path, frame.channels, self.settings.bitrate)?;
        let target_frames = u64::try_from(
            self.settings.segment_duration.as_nanos() * u128::from(frame.rate) / 1_000_000_000,
        )
        .unwrap_or(u64::MAX);
        self.current = Some(OpenSegment {
            writer,
            utc_start_ns: frame.utc_ns,
            rel_path: rel_path.to_string_lossy().into_owned(),
            rate: frame.rate,
            channels: frame.channels,
            target_frames: target_frames.max(1),
        });
        Ok(())
    }

    fn close_current(&mut self, reason: CloseReason) -> Result<(), RecorderError> {
        let Some(segment) = self.current.take() else {
            return Ok(());
        };
        let rate = segment.rate;
        let finalized = segment.writer.finalize()?;
        let utc_end_ns = segment.utc_start_ns
            + u64::try_from(u128::from(finalized.n_frames) * 1_000_000_000 / u128::from(rate))
                .unwrap_or(0);
        let record = SegmentRecord {
            slug: self.slug.to_string(),
            kind: self.kind,
            session_id: self.session_id.clone(),
            utc_start_ns: segment.utc_start_ns,
            utc_end_ns,
            sample_rate: rate,
            channels: segment.channels,
            n_frames: finalized.n_frames,
            rel_path: segment.rel_path,
            clean_close: true,
        };
        self.index
            .lock()
            .expect("index lock")
            .insert_segment(&record)?;
        self.finalized += 1;
        tracing::debug!(
            slug = %self.slug,
            path = %record.rel_path,
            frames = record.n_frames,
            ?reason,
            "segment finalized"
        );
        Ok(())
    }
}

fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::bus::FrameBus;
    use crate::encode::read_segment_info;

    const RATE: u32 = 48_000;

    fn frame(slug: &Arc<str>, seq: u64, utc_ns: UtcNs, frames: usize) -> BusEvent {
        BusEvent::Frame(AudioFrame {
            slug: Arc::clone(slug),
            kind: StreamKind::Mic,
            utc_ns,
            seq,
            rate: RATE,
            channels: 1,
            samples: Arc::from(vec![0.1_f32; frames]),
        })
    }

    struct Harness {
        bus: FrameBus,
        slug: Arc<str>,
        dir: tempfile::TempDir,
        index: Arc<Mutex<Index>>,
        shutdown_tx: watch::Sender<bool>,
        task: tokio::task::JoinHandle<Result<u64, RecorderError>>,
    }

    fn start_sink(segment_secs: u64) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path().to_owned());
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        let bus = FrameBus::new(256);
        let slug: Arc<str> = Arc::from("test-mic");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_sink(
            bus.subscribe(),
            Arc::clone(&slug),
            StreamKind::Mic,
            layout,
            Arc::clone(&index),
            SinkSettings {
                bitrate: 32_000,
                segment_duration: Duration::from_secs(segment_secs),
            },
            shutdown_rx,
        ));
        Harness {
            bus,
            slug,
            dir,
            index,
            shutdown_tx,
            task,
        }
    }

    impl Harness {
        async fn finish(self) -> (u64, Vec<SegmentRecord>, tempfile::TempDir) {
            self.shutdown_tx.send(true).unwrap();
            let finalized = self.task.await.unwrap().unwrap();
            let rows = self
                .index
                .lock()
                .unwrap()
                .overlapping("test-mic", 0, u64::MAX)
                .unwrap();
            (finalized, rows, self.dir)
        }
    }

    #[tokio::test]
    async fn rotates_segments_at_duration_boundaries() {
        let h = start_sink(1);
        let start_ns = 1_000_000_000_000;
        // 2.5 seconds of audio in 100ms frames -> two full 1s segments
        // rotate, the final 0.5s is finalized on shutdown.
        for i in 0..25_u64 {
            let utc = start_ns + i * 100_000_000;
            h.bus.publish(frame(&h.slug, i, utc, 4_800));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (finalized, rows, dir) = h.finish().await;

        assert_eq!(finalized, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].n_frames, 48_000);
        assert_eq!(rows[1].n_frames, 48_000);
        assert_eq!(rows[2].n_frames, 24_000);
        // Segments tile the timeline: each starts where the previous ended.
        assert_eq!(rows[0].utc_start_ns, start_ns);
        assert_eq!(rows[1].utc_start_ns, rows[0].utc_end_ns);
        assert_eq!(rows[2].utc_start_ns, rows[1].utc_end_ns);
        assert!(rows.iter().all(|r| r.clean_close));
        // The files exist and decode to the indexed length.
        for row in &rows {
            let info = read_segment_info(&dir.path().join(&row.rel_path)).unwrap();
            assert_eq!(info.n_frames, row.n_frames);
            assert!(info.clean_close);
        }
        // No .part leftovers anywhere in the store.
        let mut stack = vec![dir.path().to_owned()];
        while let Some(current) = stack.pop() {
            for entry in std::fs::read_dir(&current).unwrap().filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    assert_ne!(path.extension().and_then(|e| e.to_str()), Some("part"));
                }
            }
        }
    }

    #[tokio::test]
    async fn discontinuity_starts_a_new_session() {
        let h = start_sink(60);
        let start_ns = 2_000_000_000_000;
        h.bus.publish(frame(&h.slug, 0, start_ns, 4_800));
        h.bus.publish(BusEvent::Discontinuity {
            slug: Arc::clone(&h.slug),
            kind: StreamKind::Mic,
            drift_ns: 5_000_000_000,
        });
        // Audio resumes 5s later (e.g. after suspend).
        h.bus
            .publish(frame(&h.slug, 1, start_ns + 5_000_000_000, 4_800));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (finalized, rows, _dir) = h.finish().await;

        assert_eq!(finalized, 2);
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].session_id, rows[1].session_id, "new session");
        // The 5s gap is represented by segment absence.
        assert_eq!(rows[1].utc_start_ns - rows[0].utc_end_ns, 4_900_000_000);
    }

    #[tokio::test]
    async fn ignores_other_devices() {
        let h = start_sink(60);
        let other: Arc<str> = Arc::from("other-device");
        h.bus.publish(frame(&other, 0, 1_000, 4_800));
        h.bus.publish(BusEvent::StreamClosed {
            slug: Arc::clone(&other),
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (finalized, rows, _dir) = h.finish().await;
        assert_eq!(finalized, 0);
        assert_eq!(rows.len(), 0);
    }

    #[tokio::test]
    async fn flush_all_finalizes_without_ending_session() {
        let h = start_sink(60);
        let start = 4_000_000_000_000;
        // Some audio, then a pre-suspend flush, then more audio.
        h.bus.publish(frame(&h.slug, 0, start, 9_600));
        h.bus.publish(BusEvent::FlushAll);
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.bus.publish(frame(&h.slug, 1, start + 200_000_000, 9_600));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (finalized, rows, _dir) = h.finish().await;

        // Two segments (flushed + final), same session (flush doesn't rotate it).
        assert_eq!(finalized, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id, rows[1].session_id);
    }

    #[tokio::test]
    async fn suspend_resume_via_discontinuity_leaves_clean_gap() {
        // Simulates what the capture engine's drift detector emits across a
        // suspend: audio, a Discontinuity, then audio an hour later.
        let h = start_sink(60);
        let start = 5_000_000_000_000;
        h.bus.publish(frame(&h.slug, 0, start, 9_600));
        h.bus.publish(BusEvent::Discontinuity {
            slug: Arc::clone(&h.slug),
            kind: StreamKind::Mic,
            drift_ns: 3_600_000_000_000, // 1h suspend
        });
        h.bus
            .publish(frame(&h.slug, 1, start + 3_600_000_000_000, 9_600));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (_finalized, rows, _dir) = h.finish().await;

        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].session_id, rows[1].session_id);
        // The hour-long suspend is a gap between segments, not corruption.
        let gap = rows[1].utc_start_ns - rows[0].utc_end_ns;
        assert!(gap >= 3_500_000_000_000, "gap was {gap}ns");
    }

    #[tokio::test]
    async fn stream_closed_finalizes_current_segment() {
        let h = start_sink(60);
        h.bus.publish(frame(&h.slug, 0, 3_000_000_000_000, 9_600));
        h.bus.publish(BusEvent::StreamClosed {
            slug: Arc::clone(&h.slug),
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (finalized, rows, _dir) = h.finish().await;
        assert_eq!(finalized, 1);
        assert_eq!(rows[0].n_frames, 9_600);
    }
}
