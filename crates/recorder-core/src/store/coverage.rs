//! Coverage analysis: turn the segments overlapping a time range into a
//! human-facing picture of what audio exists and where the gaps are.

use crate::store::index::SegmentRecord;
use crate::timeline::UtcNs;

/// A contiguous span of recorded audio (adjacent segments merged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start_ns: UtcNs,
    pub end_ns: UtcNs,
}

impl Span {
    pub fn duration_ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }
}

/// Coverage of one device over a requested `[range_start, range_end)` window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub covered: Vec<Span>,
    pub gaps: Vec<Span>,
    pub segment_count: usize,
}

impl Coverage {
    pub fn covered_ns(&self) -> u64 {
        self.covered.iter().map(Span::duration_ns).sum()
    }

    pub fn gap_ns(&self) -> u64 {
        self.gaps.iter().map(Span::duration_ns).sum()
    }
}

/// Compute coverage of `segments` (assumed overlapping the range, ordered by
/// start) clipped to `[range_start, range_end)`. Adjacent or overlapping
/// segments with a gap no larger than `merge_tolerance_ns` are merged into
/// one covered span; the remainder of the window is reported as gaps.
pub fn coverage(
    segments: &[SegmentRecord],
    range_start: UtcNs,
    range_end: UtcNs,
    merge_tolerance_ns: u64,
) -> Coverage {
    let mut covered: Vec<Span> = Vec::new();
    for seg in segments {
        let start = seg.utc_start_ns.max(range_start);
        let end = seg.utc_end_ns.min(range_end);
        if end <= start {
            continue;
        }
        match covered.last_mut() {
            Some(last) if start <= last.end_ns.saturating_add(merge_tolerance_ns) => {
                last.end_ns = last.end_ns.max(end);
            }
            _ => covered.push(Span {
                start_ns: start,
                end_ns: end,
            }),
        }
    }

    let mut gaps = Vec::new();
    let mut cursor = range_start;
    for span in &covered {
        if span.start_ns > cursor {
            gaps.push(Span {
                start_ns: cursor,
                end_ns: span.start_ns,
            });
        }
        cursor = cursor.max(span.end_ns);
    }
    if cursor < range_end {
        gaps.push(Span {
            start_ns: cursor,
            end_ns: range_end,
        });
    }

    Coverage {
        covered,
        gaps,
        segment_count: segments.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::StreamKind;

    const SEC: u64 = 1_000_000_000;

    fn seg(start_s: u64, end_s: u64) -> SegmentRecord {
        SegmentRecord {
            slug: "mic".into(),
            kind: StreamKind::Mic,
            session_id: "s".into(),
            utc_start_ns: start_s * SEC,
            utc_end_ns: end_s * SEC,
            sample_rate: 48_000,
            channels: 1,
            n_frames: (end_s - start_s) * 48_000,
            rel_path: format!("{start_s}.opus"),
            clean_close: true,
        }
    }

    #[test]
    fn contiguous_segments_merge() {
        let segs = [seg(0, 60), seg(60, 120), seg(120, 180)];
        let cov = coverage(&segs, 0, 180 * SEC, SEC);
        assert_eq!(cov.covered.len(), 1);
        assert_eq!(
            cov.covered[0],
            Span {
                start_ns: 0,
                end_ns: 180 * SEC
            }
        );
        assert_eq!(cov.gaps.len(), 0);
        assert_eq!(cov.covered_ns(), 180 * SEC);
    }

    #[test]
    fn gap_between_segments_is_reported() {
        // 60s of audio, 120s gap, 60s of audio.
        let segs = [seg(0, 60), seg(180, 240)];
        let cov = coverage(&segs, 0, 240 * SEC, SEC);
        assert_eq!(cov.covered.len(), 2);
        assert_eq!(cov.gaps.len(), 1);
        assert_eq!(
            cov.gaps[0],
            Span {
                start_ns: 60 * SEC,
                end_ns: 180 * SEC
            }
        );
        assert_eq!(cov.gap_ns(), 120 * SEC);
    }

    #[test]
    fn range_clips_segments_and_edges() {
        // Segment spans 0..300 but the window is 60..120.
        let cov = coverage(&[seg(0, 300)], 60 * SEC, 120 * SEC, SEC);
        assert_eq!(
            cov.covered[0],
            Span {
                start_ns: 60 * SEC,
                end_ns: 120 * SEC
            }
        );
        assert_eq!(cov.gaps.len(), 0);
    }

    #[test]
    fn leading_and_trailing_gaps() {
        // Window 0..300, audio only 60..120.
        let cov = coverage(&[seg(60, 120)], 0, 300 * SEC, SEC);
        assert_eq!(cov.gaps.len(), 2);
        assert_eq!(
            cov.gaps[0],
            Span {
                start_ns: 0,
                end_ns: 60 * SEC
            }
        );
        assert_eq!(
            cov.gaps[1],
            Span {
                start_ns: 120 * SEC,
                end_ns: 300 * SEC
            }
        );
    }

    #[test]
    fn empty_range_is_all_gap() {
        let cov = coverage(&[], 0, 100 * SEC, SEC);
        assert_eq!(cov.covered.len(), 0);
        assert_eq!(cov.gaps.len(), 1);
        assert_eq!(cov.gap_ns(), 100 * SEC);
    }

    #[test]
    fn small_gaps_within_tolerance_merge() {
        // A 500ms gap (dropped frames) with 1s tolerance is bridged.
        let a = seg(0, 60);
        let mut b = seg(60, 120);
        b.utc_start_ns = 60 * SEC + 500_000_000;
        let cov = coverage(&[a, b], 0, 120 * SEC + 500_000_000, SEC);
        assert_eq!(cov.covered.len(), 1);
    }

    #[test]
    fn overlapping_segments_merge() {
        let cov = coverage(&[seg(0, 90), seg(60, 120)], 0, 120 * SEC, SEC);
        assert_eq!(cov.covered.len(), 1);
        assert_eq!(cov.covered[0].end_ns, 120 * SEC);
    }
}
