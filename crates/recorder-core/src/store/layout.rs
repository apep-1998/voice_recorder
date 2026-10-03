//! On-disk storage layout.
//!
//! ```text
//! <root>/
//!   index.sqlite3
//!   segments/<device_slug>/<YYYY>/<MM>/<DD>/<YYYYMMDD'T'HHMMSS.mmm'Z'>_<session8>.opus
//! ```
//!
//! Filenames are self-describing (UTC start + session id); together with the
//! Ogg granule positions inside the files they are the source of truth — the
//! SQLite index is a rebuildable cache.

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};

use crate::timeline::UtcNs;

const FILENAME_TS_FORMAT: &str = "%Y%m%dT%H%M%S%.3fZ";

/// Resolves paths inside the storage root.
#[derive(Debug, Clone)]
pub struct StorageLayout {
    root: PathBuf,
}

impl StorageLayout {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.sqlite3")
    }

    pub fn segments_dir(&self) -> PathBuf {
        self.root.join("segments")
    }

    pub fn device_dir(&self, slug: &str) -> PathBuf {
        self.segments_dir().join(slug)
    }

    /// Absolute path of a segment starting at `utc_start_ns` for `slug`.
    pub fn segment_path(&self, slug: &str, utc_start_ns: UtcNs, session8: &str) -> PathBuf {
        self.root
            .join(segment_rel_path(slug, utc_start_ns, session8))
    }
}

/// Root-relative path of a segment file.
pub fn segment_rel_path(slug: &str, utc_start_ns: UtcNs, session8: &str) -> PathBuf {
    let dt = utc_from_ns(utc_start_ns);
    PathBuf::from("segments")
        .join(slug)
        .join(dt.format("%Y").to_string())
        .join(dt.format("%m").to_string())
        .join(dt.format("%d").to_string())
        .join(format!("{}_{session8}.opus", dt.format(FILENAME_TS_FORMAT)))
}

/// Parsed identity of a segment file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentName {
    /// UTC start, millisecond precision (as encoded in the filename).
    pub utc_start_ns: UtcNs,
    pub session_id: String,
}

/// Parse `20261003T154501.123Z_ab12cd34.opus` (optionally with a `.part`
/// suffix) back into its identity.
pub fn parse_segment_filename(file_name: &str) -> Option<SegmentName> {
    let stem = file_name
        .strip_suffix(".part")
        .unwrap_or(file_name)
        .strip_suffix(".opus")?;
    let (ts, session) = stem.split_once('_')?;
    if session.is_empty() {
        return None;
    }
    let dt = chrono::NaiveDateTime::parse_from_str(ts, FILENAME_TS_FORMAT).ok()?;
    let utc = Utc.from_utc_datetime(&dt);
    Some(SegmentName {
        utc_start_ns: u64::try_from(utc.timestamp_nanos_opt()?).ok()?,
        session_id: session.to_owned(),
    })
}

pub fn utc_from_ns(utc_ns: UtcNs) -> DateTime<Utc> {
    let secs = i64::try_from(utc_ns / 1_000_000_000).unwrap_or(i64::MAX);
    let nanos = u32::try_from(utc_ns % 1_000_000_000).unwrap_or(0);
    Utc.timestamp_opt(secs, nanos)
        .single()
        .unwrap_or(DateTime::UNIX_EPOCH)
}

/// Human-readable UTC timestamp for logs and listings.
pub fn format_utc(utc_ns: UtcNs) -> String {
    utc_from_ns(utc_ns).to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: UtcNs = 1_791_647_101_123_000_000; // 2026-10-10T15:45:01.123Z

    #[test]
    fn segment_paths_are_date_sharded() {
        let rel = segment_rel_path("my-mic", TS, "ab12cd34");
        assert_eq!(
            rel,
            PathBuf::from("segments/my-mic/2026/10/10/20261010T154501.123Z_ab12cd34.opus")
        );
    }

    #[test]
    fn filename_round_trips() {
        let rel = segment_rel_path("mic", TS, "ab12cd34");
        let name = rel.file_name().unwrap().to_str().unwrap();
        let parsed = parse_segment_filename(name).expect("parses");
        // Filenames carry millisecond precision.
        assert_eq!(parsed.utc_start_ns, TS / 1_000_000 * 1_000_000);
        assert_eq!(parsed.session_id, "ab12cd34");
    }

    #[test]
    fn part_files_parse_too() {
        let parsed =
            parse_segment_filename("20261010T154501.123Z_ab12cd34.opus.part").expect("parses");
        assert_eq!(parsed.session_id, "ab12cd34");
    }

    #[test]
    fn junk_filenames_are_rejected() {
        for name in [
            "notes.txt",
            "20261010T154501.123Z.opus",   // no session
            "20261010T154501.123Z_.opus",  // empty session
            "yesterday_ab12cd34.opus",     // bad timestamp
            "20261010T154501.123Z_s.flac", // wrong extension
        ] {
            assert_eq!(parse_segment_filename(name), None, "{name}");
        }
    }

    #[test]
    fn layout_paths() {
        let layout = StorageLayout::new(PathBuf::from("/data/vr"));
        assert_eq!(layout.index_path(), PathBuf::from("/data/vr/index.sqlite3"));
        assert_eq!(
            layout.segment_path("mic", TS, "s1234567"),
            PathBuf::from("/data/vr/segments/mic/2026/10/10/20261010T154501.123Z_s1234567.opus")
        );
    }

    #[test]
    fn format_utc_is_rfc3339() {
        assert_eq!(format_utc(TS), "2026-10-10T15:45:01.123Z");
    }
}
