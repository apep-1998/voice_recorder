//! Export planning: turn a requested time window plus the segments that
//! overlap it into a precise, wall-clock-aligned track plan. Pure and fully
//! unit-testable; the ffmpeg runner (see `export::ffmpeg`) executes it.
//!
//! A plan for one device is a list of [`TrackPart`]s laid end to end, whose
//! total duration equals the requested window. Gaps between segments become
//! explicit silence parts so that t=0 of the output is exactly the window
//! start and every device's track stays aligned to the same wall clock —
//! which is what makes mixing mic + monitor correct.

use crate::store::index::SegmentRecord;
use crate::timeline::UtcNs;

/// One piece of a device's output track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackPart {
    /// Take `[seg_offset_ns, seg_offset_ns + duration_ns)` from this segment
    /// file (offset measured from the segment's own start).
    Segment {
        rel_path: String,
        seg_offset_ns: u64,
        duration_ns: u64,
    },
    /// Insert this much silence (a recording gap, or lead/trail padding).
    Silence { duration_ns: u64 },
}

impl TrackPart {
    pub fn duration_ns(&self) -> u64 {
        match self {
            TrackPart::Segment { duration_ns, .. } | TrackPart::Silence { duration_ns } => {
                *duration_ns
            }
        }
    }
}

/// A full plan for one device over the requested window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPlan {
    pub slug: String,
    pub channels: u8,
    pub sample_rate: u32,
    pub parts: Vec<TrackPart>,
}

impl TrackPlan {
    pub fn total_ns(&self) -> u64 {
        self.parts.iter().map(TrackPart::duration_ns).sum()
    }

    /// True if the plan contains any real audio (not pure silence).
    pub fn has_audio(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, TrackPart::Segment { .. }))
    }
}

/// Gaps shorter than this are rounding noise from converting 48 kHz granule
/// counts to nanoseconds between otherwise-contiguous segments, not real
/// recording gaps. Absorbing them avoids emitting zero-length silence parts.
pub const GAP_EPSILON_NS: u64 = 1_000_000; // 1 ms

/// Build a device track for `[start_ns, end_ns)` from its overlapping
/// segments (ordered by start). When `compact` is true, recording gaps are
/// dropped instead of filled with silence (so wall-clock alignment is lost
/// but the output has no silent stretches).
///
/// `channels`/`sample_rate` default the track's format when the window has no
/// audio at all.
pub fn plan_track(
    slug: &str,
    segments: &[SegmentRecord],
    start_ns: UtcNs,
    end_ns: UtcNs,
    compact: bool,
) -> TrackPlan {
    let mut parts = Vec::new();
    let mut cursor = start_ns;
    let mut channels = 0_u8;
    let mut sample_rate = 0_u32;

    for seg in segments {
        let seg_start = seg.utc_start_ns.max(start_ns);
        let seg_end = seg.utc_end_ns.min(end_ns);
        if seg_end <= seg_start {
            continue;
        }
        if channels == 0 {
            channels = seg.channels;
            sample_rate = seg.sample_rate;
        }
        if !compact && seg_start > cursor + GAP_EPSILON_NS {
            parts.push(TrackPart::Silence {
                duration_ns: seg_start - cursor,
            });
        }
        parts.push(TrackPart::Segment {
            rel_path: seg.rel_path.clone(),
            seg_offset_ns: seg_start - seg.utc_start_ns,
            duration_ns: seg_end - seg_start,
        });
        cursor = cursor.max(seg_end);
    }

    if !compact && end_ns > cursor + GAP_EPSILON_NS {
        parts.push(TrackPart::Silence {
            duration_ns: end_ns - cursor,
        });
    }

    // Window with no audio at all: one silence part (non-compact), or empty.
    if parts.is_empty() && !compact {
        parts.push(TrackPart::Silence {
            duration_ns: end_ns.saturating_sub(start_ns),
        });
    }

    TrackPlan {
        slug: slug.to_owned(),
        channels: if channels == 0 { 1 } else { channels },
        sample_rate: if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        },
        parts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::StreamKind;

    const SEC: u64 = 1_000_000_000;

    fn seg(start_s: u64, end_s: u64, path: &str) -> SegmentRecord {
        SegmentRecord {
            slug: "mic".into(),
            kind: StreamKind::Mic,
            session_id: "s".into(),
            utc_start_ns: start_s * SEC,
            utc_end_ns: end_s * SEC,
            sample_rate: 48_000,
            channels: 1,
            n_frames: (end_s - start_s) * 48_000,
            rel_path: path.into(),
            clean_close: true,
        }
    }

    #[test]
    fn contiguous_segments_need_no_silence() {
        let segs = [seg(60, 120, "a.opus"), seg(120, 180, "b.opus")];
        let plan = plan_track("mic", &segs, 60 * SEC, 180 * SEC, false);
        assert_eq!(plan.parts.len(), 2);
        assert_eq!(plan.total_ns(), 120 * SEC);
        assert!(
            matches!(&plan.parts[0], TrackPart::Segment { rel_path, .. } if rel_path == "a.opus")
        );
    }

    #[test]
    fn interior_gap_becomes_silence() {
        let segs = [seg(0, 60, "a.opus"), seg(120, 180, "b.opus")];
        let plan = plan_track("mic", &segs, 0, 180 * SEC, false);
        assert_eq!(plan.parts.len(), 3);
        assert_eq!(
            plan.parts[1],
            TrackPart::Silence {
                duration_ns: 60 * SEC
            }
        );
        assert_eq!(plan.total_ns(), 180 * SEC);
    }

    #[test]
    fn edges_are_trimmed_to_the_window() {
        // Segment 0..300, window 60..120 -> take offset 60s, duration 60s.
        let plan = plan_track("mic", &[seg(0, 300, "a.opus")], 60 * SEC, 120 * SEC, false);
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(
            plan.parts[0],
            TrackPart::Segment {
                rel_path: "a.opus".into(),
                seg_offset_ns: 60 * SEC,
                duration_ns: 60 * SEC,
            }
        );
    }

    #[test]
    fn leading_and_trailing_silence() {
        // Window 0..300, audio only 60..120.
        let plan = plan_track("mic", &[seg(60, 120, "a.opus")], 0, 300 * SEC, false);
        assert_eq!(plan.parts.len(), 3);
        assert_eq!(
            plan.parts[0],
            TrackPart::Silence {
                duration_ns: 60 * SEC
            }
        );
        assert!(matches!(plan.parts[1], TrackPart::Segment { .. }));
        assert_eq!(
            plan.parts[2],
            TrackPart::Silence {
                duration_ns: 180 * SEC
            }
        );
        assert_eq!(plan.total_ns(), 300 * SEC);
    }

    #[test]
    fn window_entirely_in_a_gap_is_all_silence() {
        let plan = plan_track("mic", &[], 0, 100 * SEC, false);
        assert_eq!(
            plan.parts,
            vec![TrackPart::Silence {
                duration_ns: 100 * SEC
            }]
        );
        assert!(!plan.has_audio());
        assert_eq!(plan.total_ns(), 100 * SEC);
    }

    #[test]
    fn compact_mode_drops_gaps() {
        let segs = [seg(0, 60, "a.opus"), seg(120, 180, "b.opus")];
        let plan = plan_track("mic", &segs, 0, 180 * SEC, true);
        // No silence parts; just the two audio pieces.
        assert_eq!(plan.parts.len(), 2);
        assert!(plan
            .parts
            .iter()
            .all(|p| matches!(p, TrackPart::Segment { .. })));
        assert_eq!(plan.total_ns(), 120 * SEC);
    }

    #[test]
    fn compact_empty_window_is_empty() {
        let plan = plan_track("mic", &[], 0, 100 * SEC, true);
        assert_eq!(plan.parts.len(), 0);
        assert!(!plan.has_audio());
    }

    #[test]
    fn sub_millisecond_gap_is_absorbed() {
        // Two "contiguous" segments whose boundary is off by 667µs due to
        // 48kHz granule rounding must NOT produce a silence part (which would
        // round to a zero-duration ffmpeg input and hang the concat).
        let a = seg(0, 60, "a.opus");
        let mut b = seg(60, 120, "b.opus");
        b.utc_start_ns = 60 * SEC + 666_667; // 667µs later than a's end
        let plan = plan_track("mic", &[a, b], 0, 120 * SEC + 666_667, false);
        assert!(
            plan.parts
                .iter()
                .all(|p| matches!(p, TrackPart::Segment { .. })),
            "sub-ms gap must not create a silence part: {:?}",
            plan.parts
        );
        assert_eq!(plan.parts.len(), 2);
    }

    #[test]
    fn real_gap_above_epsilon_is_kept() {
        let a = seg(0, 60, "a.opus");
        let b = seg(61, 120, "b.opus"); // a 1s real gap between a and b
        let plan = plan_track("mic", &[a, b], 0, 120 * SEC, false);
        assert_eq!(
            plan.parts
                .iter()
                .filter(|p| matches!(p, TrackPart::Silence { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn preserves_segment_format() {
        let mut s = seg(0, 60, "a.opus");
        s.channels = 2;
        s.sample_rate = 48_000;
        let plan = plan_track("mon", &[s], 0, 60 * SEC, false);
        assert_eq!(plan.channels, 2);
    }
}
