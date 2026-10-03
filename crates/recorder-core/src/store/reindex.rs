//! Rebuild the SQLite index from the files on disk.
//!
//! Filenames carry the UTC start and session id; Ogg granule positions carry
//! the duration. Together they fully reconstruct the index, which makes the
//! database a disposable cache.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::device::StreamKind;
use crate::encode::read_segment_info;
use crate::error::RecorderError;
use crate::store::index::{Index, SegmentRecord};
use crate::store::layout::{parse_segment_filename, StorageLayout};
use crate::store::salvage::salvage_startup;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReindexStats {
    pub indexed: u64,
    pub salvaged: u64,
    pub skipped: u64,
}

/// Drop all segment rows and rebuild them by scanning the store. `.part`
/// files are salvaged first.
///
/// # Panics
/// Panics if the index mutex is poisoned (a prior holder panicked).
pub fn reindex(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
) -> Result<ReindexStats, RecorderError> {
    let mut stats = ReindexStats {
        salvaged: salvage_startup(layout, index)?,
        ..ReindexStats::default()
    };
    // Salvage already indexed its recoveries, but a full rebuild follows and
    // re-discovers those files from disk anyway.
    index.lock().expect("index lock").clear_segments()?;

    let segments_dir = layout.segments_dir();
    if !segments_dir.exists() {
        return Ok(stats);
    }
    for slug_dir in sorted_dirs(&segments_dir)? {
        let Some(slug) = slug_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
        else {
            continue;
        };
        let mut stack = vec![slug_dir.clone()];
        while let Some(dir) = stack.pop() {
            for path in sorted_entries(&dir)? {
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "opus") {
                    match index_one(layout, index, &slug, &path) {
                        Ok(true) => stats.indexed += 1,
                        Ok(false) => stats.skipped += 1,
                        Err(err) => {
                            tracing::warn!(path = %path.display(), "reindex skipped: {err}");
                            stats.skipped += 1;
                        }
                    }
                }
            }
        }
    }
    Ok(stats)
}

fn index_one(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
    slug: &str,
    path: &Path,
) -> Result<bool, RecorderError> {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(false);
    };
    let Some(name) = parse_segment_filename(file_name) else {
        return Ok(false);
    };
    let info = read_segment_info(path)?;
    let utc_end_ns = name.utc_start_ns
        + u64::try_from(u128::from(info.n_frames) * 1_000_000_000 / 48_000).unwrap_or(0);
    let rel_path = path
        .strip_prefix(layout.root())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let kind = if slug.ends_with(".monitor") {
        StreamKind::Monitor
    } else {
        StreamKind::Mic
    };
    index
        .lock()
        .expect("index lock")
        .insert_segment(&SegmentRecord {
            slug: slug.to_owned(),
            kind,
            session_id: name.session_id,
            utc_start_ns: name.utc_start_ns,
            utc_end_ns,
            sample_rate: 48_000,
            channels: info.channels,
            n_frames: info.n_frames,
            rel_path,
            clean_close: info.clean_close,
        })?;
    Ok(true)
}

fn sorted_entries(dir: &Path) -> Result<Vec<std::path::PathBuf>, RecorderError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|source| RecorderError::Io {
            path: dir.to_owned(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    Ok(entries)
}

fn sorted_dirs(dir: &Path) -> Result<Vec<std::path::PathBuf>, RecorderError> {
    Ok(sorted_entries(dir)?
        .into_iter()
        .filter(|p| p.is_dir())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::SegmentWriter;
    use crate::store::layout::segment_rel_path;

    #[test]
    fn rebuild_matches_original_index() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path().to_owned());
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));

        let base_ns = 1_791_647_101_000_000_000_u64;
        // Two devices, two clean segments each, plus one crashed .part.
        for (slug, session, offset_s) in [
            ("mic-a", "sessaaaa", 0_u64),
            ("mic-a", "sessaaaa", 60),
            ("sink-b.monitor", "sessbbbb", 0),
            ("sink-b.monitor", "sessbbbb", 60),
        ] {
            let start = base_ns + offset_s * 1_000_000_000;
            let channels = if slug.ends_with(".monitor") { 2 } else { 1 };
            let abs = layout.root().join(segment_rel_path(slug, start, session));
            let mut writer = SegmentWriter::create(abs, channels, 32_000).unwrap();
            writer
                .write_samples(&vec![0.1_f32; 48_000 * usize::from(channels)])
                .unwrap();
            writer.finalize().unwrap();
        }
        let crashed = layout.root().join(segment_rel_path(
            "mic-a",
            base_ns + 120 * 1_000_000_000,
            "sessaaaa",
        ));
        let mut writer = SegmentWriter::create(crashed, 1, 32_000).unwrap();
        // 100 packets = two complete 50-packet page groups; both pages are
        // flushed, so the full 96000 frames survive a crash.
        writer.write_samples(&vec![0.1_f32; 96_000]).unwrap();
        drop(writer); // crash

        let stats = reindex(&layout, &index).unwrap();
        assert_eq!(stats.salvaged, 1);
        assert_eq!(stats.indexed, 5, "4 clean + 1 salvaged file");
        assert_eq!(stats.skipped, 0);

        let locked = index.lock().unwrap();
        assert_eq!(locked.slugs().unwrap(), vec!["mic-a", "sink-b.monitor"]);
        let mic_rows = locked.overlapping("mic-a", 0, u64::MAX).unwrap();
        assert_eq!(mic_rows.len(), 3);
        assert_eq!(mic_rows[0].utc_start_ns, base_ns);
        assert_eq!(mic_rows[0].n_frames, 48_000);
        assert!(mic_rows[0].clean_close);
        assert_eq!(mic_rows[0].session_id, "sessaaaa");
        // The salvaged one is marked unclean and holds the complete pages.
        assert!(!mic_rows[2].clean_close);
        assert_eq!(mic_rows[2].n_frames, 96_000);
        let monitor_rows = locked.overlapping("sink-b.monitor", 0, u64::MAX).unwrap();
        assert_eq!(monitor_rows.len(), 2);
        assert_eq!(monitor_rows[0].kind, StreamKind::Monitor);
        assert_eq!(monitor_rows[0].channels, 2);
    }

    #[test]
    fn reindex_replaces_stale_rows() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path().to_owned());
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        // A bogus row pointing at a file that doesn't exist.
        index
            .lock()
            .unwrap()
            .insert_segment(&crate::store::index::SegmentRecord {
                slug: "ghost".into(),
                kind: StreamKind::Mic,
                session_id: "sessdead".into(),
                utc_start_ns: 1,
                utc_end_ns: 2,
                sample_rate: 48_000,
                channels: 1,
                n_frames: 1,
                rel_path: "segments/ghost/nope.opus".into(),
                clean_close: true,
            })
            .unwrap();
        let stats = reindex(&layout, &index).unwrap();
        assert_eq!(stats.indexed, 0);
        assert_eq!(index.lock().unwrap().segment_count().unwrap(), 0);
    }
}
