//! The SQLite segment index.
//!
//! The index makes time-range queries fast; it is a **cache**, rebuildable
//! from the segment filenames plus their Ogg granule positions (`voicerec
//! reindex`). WAL mode keeps it crash-safe. All timestamps are nanoseconds
//! since the Unix epoch stored as `INTEGER` (fits until the year 2262).

use std::path::Path;

use rusqlite::{params, Connection};

use crate::device::StreamKind;
use crate::error::IndexError;
use crate::timeline::UtcNs;

/// One indexed segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRecord {
    pub slug: String,
    pub kind: StreamKind,
    pub session_id: String,
    pub utc_start_ns: UtcNs,
    pub utc_end_ns: UtcNs,
    pub sample_rate: u32,
    pub channels: u8,
    pub n_frames: u64,
    /// Path relative to the storage root.
    pub rel_path: String,
    /// Whether the segment was finalized cleanly (false: salvaged).
    pub clean_close: bool,
}

/// A recording session: one continuous run of a capture stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub started_utc_ns: UtcNs,
    /// Why the session started: "startup", "resume", "device-reappeared",
    /// "clock-discontinuity", ...
    pub reason: String,
}

pub struct Index {
    conn: Connection,
}

impl Index {
    /// Open (creating if needed) the index database at `path`.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| IndexError::Io {
                path: path.to_owned(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// In-memory index for tests.
    pub fn open_in_memory() -> Result<Self, IndexError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn record_session(&self, session: &SessionRecord) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (id, started_utc_ns, reason) VALUES (?1, ?2, ?3)",
            params![
                session.id,
                ns_to_i64(session.started_utc_ns),
                session.reason
            ],
        )?;
        Ok(())
    }

    pub fn insert_segment(&self, segment: &SegmentRecord) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO segments
               (slug, kind, session_id, utc_start_ns, utc_end_ns,
                sample_rate, channels, n_frames, rel_path, clean_close)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                segment.slug,
                segment.kind.as_str(),
                segment.session_id,
                ns_to_i64(segment.utc_start_ns),
                ns_to_i64(segment.utc_end_ns),
                segment.sample_rate,
                segment.channels,
                i64::try_from(segment.n_frames).unwrap_or(i64::MAX),
                segment.rel_path,
                segment.clean_close,
            ],
        )?;
        Ok(())
    }

    /// Segments of `slug` overlapping `[start_ns, end_ns)`, ordered by start.
    pub fn overlapping(
        &self,
        slug: &str,
        start_ns: UtcNs,
        end_ns: UtcNs,
    ) -> Result<Vec<SegmentRecord>, IndexError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT slug, kind, session_id, utc_start_ns, utc_end_ns,
                    sample_rate, channels, n_frames, rel_path, clean_close
             FROM segments
             WHERE slug = ?1 AND utc_start_ns < ?3 AND utc_end_ns > ?2
             ORDER BY utc_start_ns",
        )?;
        let rows = stmt.query_map(
            params![slug, ns_to_i64(start_ns), ns_to_i64(end_ns)],
            row_to_segment,
        )?;
        collect_rows(rows)
    }

    /// All distinct device slugs present in the index.
    pub fn slugs(&self) -> Result<Vec<String>, IndexError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT DISTINCT slug FROM segments ORDER BY slug")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        collect_rows(rows)
    }

    /// Delete index rows for segments that END before `cutoff_ns`, returning
    /// their relative paths so the caller can remove the files.
    pub fn delete_ending_before(&self, cutoff_ns: UtcNs) -> Result<Vec<String>, IndexError> {
        let cutoff = ns_to_i64(cutoff_ns);
        let mut stmt = self
            .conn
            .prepare_cached("SELECT rel_path FROM segments WHERE utc_end_ns < ?1")?;
        let paths: Vec<String> = collect_rows(stmt.query_map(params![cutoff], |row| row.get(0))?)?;
        self.conn.execute(
            "DELETE FROM segments WHERE utc_end_ns < ?1",
            params![cutoff],
        )?;
        Ok(paths)
    }

    /// Remove one segment row by relative path.
    pub fn delete_by_rel_path(&self, rel_path: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "DELETE FROM segments WHERE rel_path = ?1",
            params![rel_path],
        )?;
        Ok(())
    }

    /// Total bytes of indexed audio is unknown (files compress); this counts
    /// rows for quick stats.
    pub fn segment_count(&self) -> Result<u64, IndexError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM segments", [], |row| row.get(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Oldest segment start and newest segment end, if any segments exist.
    pub fn time_span(&self) -> Result<Option<(UtcNs, UtcNs)>, IndexError> {
        let span: Option<(i64, i64)> = self.conn.query_row(
            "SELECT MIN(utc_start_ns), MAX(utc_end_ns) FROM segments",
            [],
            |row| {
                let min: Option<i64> = row.get(0)?;
                let max: Option<i64> = row.get(1)?;
                Ok(min.zip(max))
            },
        )?;
        Ok(span.map(|(min, max)| (i64_to_ns(min), i64_to_ns(max))))
    }

    /// Delete every segment row (used by reindex before a rebuild).
    pub fn clear_segments(&self) -> Result<(), IndexError> {
        self.conn.execute("DELETE FROM segments", [])?;
        Ok(())
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id             TEXT PRIMARY KEY,
    started_utc_ns INTEGER NOT NULL,
    reason         TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS segments (
    id            INTEGER PRIMARY KEY,
    slug          TEXT    NOT NULL,
    kind          TEXT    NOT NULL,
    session_id    TEXT    NOT NULL,
    utc_start_ns  INTEGER NOT NULL,
    utc_end_ns    INTEGER NOT NULL,
    sample_rate   INTEGER NOT NULL,
    channels      INTEGER NOT NULL,
    n_frames      INTEGER NOT NULL,
    rel_path      TEXT    NOT NULL UNIQUE,
    clean_close   INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_segments_slug_time
    ON segments (slug, utc_start_ns, utc_end_ns);
CREATE INDEX IF NOT EXISTS idx_segments_end
    ON segments (utc_end_ns);
";

fn ns_to_i64(ns: UtcNs) -> i64 {
    i64::try_from(ns).unwrap_or(i64::MAX)
}

fn i64_to_ns(value: i64) -> UtcNs {
    u64::try_from(value).unwrap_or(0)
}

fn row_to_segment(row: &rusqlite::Row<'_>) -> rusqlite::Result<SegmentRecord> {
    let kind: String = row.get(1)?;
    Ok(SegmentRecord {
        slug: row.get(0)?,
        kind: kind.parse().unwrap_or(StreamKind::Mic),
        session_id: row.get(2)?,
        utc_start_ns: i64_to_ns(row.get(3)?),
        utc_end_ns: i64_to_ns(row.get(4)?),
        sample_rate: row.get(5)?,
        channels: row.get(6)?,
        n_frames: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        rel_path: row.get(8)?,
        clean_close: row.get(9)?,
    })
}

fn collect_rows<T>(rows: impl Iterator<Item = rusqlite::Result<T>>) -> Result<Vec<T>, IndexError> {
    rows.collect::<rusqlite::Result<Vec<T>>>()
        .map_err(IndexError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    fn segment(slug: &str, start_s: u64, end_s: u64) -> SegmentRecord {
        SegmentRecord {
            slug: slug.to_owned(),
            kind: StreamKind::Mic,
            session_id: "sess0001".to_owned(),
            utc_start_ns: start_s * SEC,
            utc_end_ns: end_s * SEC,
            sample_rate: 48_000,
            channels: 1,
            n_frames: (end_s - start_s) * 48_000,
            rel_path: format!("segments/{slug}/{start_s}.opus"),
            clean_close: true,
        }
    }

    #[test]
    fn insert_and_query_overlap() {
        let index = Index::open_in_memory().unwrap();
        // Three adjacent minutes: [60,120), [120,180), [180,240).
        for (s, e) in [(60, 120), (120, 180), (180, 240)] {
            index.insert_segment(&segment("mic", s, e)).unwrap();
        }

        // Range fully inside one segment.
        let hits = index.overlapping("mic", 130 * SEC, 140 * SEC).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].utc_start_ns, 120 * SEC);

        // Range spanning two segments.
        let hits = index.overlapping("mic", 110 * SEC, 130 * SEC).unwrap();
        assert_eq!(hits.len(), 2);

        // Touching boundaries is NOT overlap (half-open ranges).
        let hits = index.overlapping("mic", 240 * SEC, 300 * SEC).unwrap();
        assert_eq!(hits.len(), 0);
        let hits = index.overlapping("mic", 0, 60 * SEC).unwrap();
        assert_eq!(hits.len(), 0);

        // Other slugs don't match.
        let hits = index.overlapping("other", 130 * SEC, 140 * SEC).unwrap();
        assert_eq!(hits.len(), 0);
    }

    #[test]
    fn records_round_trip_exactly() {
        let index = Index::open_in_memory().unwrap();
        let mut original = segment("monitor-slug", 100, 160);
        original.kind = StreamKind::Monitor;
        original.clean_close = false;
        original.channels = 2;
        index.insert_segment(&original).unwrap();
        let fetched = index
            .overlapping("monitor-slug", 0, 1_000 * SEC)
            .unwrap()
            .remove(0);
        assert_eq!(fetched, original);
    }

    #[test]
    fn delete_ending_before_returns_paths() {
        let index = Index::open_in_memory().unwrap();
        for (s, e) in [(0, 60), (60, 120), (120, 180)] {
            index.insert_segment(&segment("mic", s, e)).unwrap();
        }
        // Strictly "ends before": a segment ending exactly at the cutoff
        // survives, so cutoff 180 removes [0,60) and [60,120) only.
        let removed = index.delete_ending_before(180 * SEC).unwrap();
        assert_eq!(
            removed,
            vec![
                "segments/mic/0.opus".to_owned(),
                "segments/mic/60.opus".to_owned()
            ]
        );
        assert_eq!(index.segment_count().unwrap(), 1);
        // The survivor is the newest segment.
        let left = index.overlapping("mic", 0, 1_000 * SEC).unwrap();
        assert_eq!(left[0].utc_start_ns, 120 * SEC);
    }

    #[test]
    fn slugs_and_span() {
        let index = Index::open_in_memory().unwrap();
        assert!(index.time_span().unwrap().is_none());
        index.insert_segment(&segment("b-mic", 60, 120)).unwrap();
        index.insert_segment(&segment("a-mon", 30, 90)).unwrap();
        assert_eq!(index.slugs().unwrap(), vec!["a-mon", "b-mic"]);
        assert_eq!(index.time_span().unwrap(), Some((30 * SEC, 120 * SEC)));
    }

    #[test]
    fn reinserting_same_path_replaces() {
        let index = Index::open_in_memory().unwrap();
        index.insert_segment(&segment("mic", 60, 120)).unwrap();
        let mut updated = segment("mic", 60, 120);
        updated.n_frames = 1;
        index.insert_segment(&updated).unwrap();
        assert_eq!(index.segment_count().unwrap(), 1);
        let got = index.overlapping("mic", 0, 1_000 * SEC).unwrap();
        assert_eq!(got[0].n_frames, 1);
    }

    #[test]
    fn sessions_are_recorded() {
        let index = Index::open_in_memory().unwrap();
        index
            .record_session(&SessionRecord {
                id: "sess0001".into(),
                started_utc_ns: 42 * SEC,
                reason: "startup".into(),
            })
            .unwrap();
        // Re-recording the same id is fine (replace).
        index
            .record_session(&SessionRecord {
                id: "sess0001".into(),
                started_utc_ns: 42 * SEC,
                reason: "startup".into(),
            })
            .unwrap();
    }

    #[test]
    fn persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.sqlite3");
        {
            let index = Index::open(&path).unwrap();
            index.insert_segment(&segment("mic", 60, 120)).unwrap();
        }
        let reopened = Index::open(&path).unwrap();
        assert_eq!(reopened.segment_count().unwrap(), 1);
    }
}
