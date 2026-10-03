//! Startup salvage: promote `.opus.part` files left behind by crashes or
//! power loss into indexed, playable segments.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::device::StreamKind;
use crate::encode::salvage_part_file;
use crate::error::RecorderError;
use crate::store::index::{Index, SegmentRecord};
use crate::store::layout::{parse_segment_filename, StorageLayout};

/// Walk the segments tree, salvage every `.part` file, and index the
/// recovered segments (marked `clean_close = false`). Returns how many
/// segments were recovered.
pub fn salvage_startup(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
) -> Result<u64, RecorderError> {
    let segments_dir = layout.segments_dir();
    if !segments_dir.exists() {
        return Ok(0);
    }
    let mut recovered = 0;
    // segments/<slug>/<YYYY>/<MM>/<DD>/<file>
    for slug_entry in read_dir_sorted(&segments_dir)? {
        let slug_dir = slug_entry;
        let Some(slug) = slug_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
        else {
            continue;
        };
        for part_path in find_part_files(&slug_dir)? {
            match salvage_one(layout, index, &slug, &part_path) {
                Ok(true) => recovered += 1,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(path = %part_path.display(), "salvage failed: {err}");
                }
            }
        }
    }
    Ok(recovered)
}

fn salvage_one(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
    slug: &str,
    part_path: &Path,
) -> Result<bool, RecorderError> {
    let Some(file_name) = part_path.file_name().and_then(|n| n.to_str()) else {
        return Ok(false);
    };
    let Some(name) = parse_segment_filename(file_name) else {
        tracing::warn!(path = %part_path.display(), "unrecognized .part file; skipping");
        return Ok(false);
    };
    let Some((final_path, info)) = salvage_part_file(part_path)? else {
        return Ok(false); // header-only stub, deleted
    };
    let utc_end_ns = name.utc_start_ns
        + u64::try_from(u128::from(info.n_frames) * 1_000_000_000 / 48_000).unwrap_or(0);
    let rel_path = final_path
        .strip_prefix(layout.root())
        .unwrap_or(&final_path)
        .to_string_lossy()
        .into_owned();
    // Monitors carry a ".monitor" slug suffix (see CaptureTarget::monitor).
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
            clean_close: false,
        })?;
    Ok(true)
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<std::path::PathBuf>, RecorderError> {
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

fn find_part_files(dir: &Path) -> Result<Vec<std::path::PathBuf>, RecorderError> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        for path in read_dir_sorted(&current)? {
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "part") {
                found.push(path);
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::SegmentWriter;
    use crate::store::layout::segment_rel_path;

    #[test]
    fn salvages_crashed_segments_into_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path().to_owned());
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));

        let start_ns = 1_791_647_101_000_000_000_u64;
        let rel = segment_rel_path("crashed-mic.monitor", start_ns, "sess0001");
        let abs = layout.root().join(&rel);
        let mut writer = SegmentWriter::create(abs, 2, 64_000).unwrap();
        // Two full page groups (2 x 50 packets) of stereo audio.
        writer.write_samples(&vec![0.2_f32; 96_000 * 2]).unwrap();
        drop(writer); // crash

        // Plus a header-only stub that must be cleaned up silently.
        let stub_rel =
            segment_rel_path("crashed-mic.monitor", start_ns + 60_000_000_000, "sess0001");
        let stub_writer = SegmentWriter::create(layout.root().join(&stub_rel), 2, 64_000).unwrap();
        drop(stub_writer);

        let recovered = salvage_startup(&layout, &index).unwrap();
        assert_eq!(recovered, 1);

        let rows = index
            .lock()
            .unwrap()
            .overlapping("crashed-mic.monitor", 0, u64::MAX)
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.kind, StreamKind::Monitor);
        assert_eq!(row.session_id, "sess0001");
        assert_eq!(row.utc_start_ns, start_ns);
        assert!(!row.clean_close);
        assert_eq!(row.n_frames, 96_000);
        assert_eq!(row.utc_end_ns, start_ns + 2_000_000_000);
        // The salvaged file was renamed and is decodable.
        assert!(layout.root().join(&row.rel_path).exists());
        // No .part files remain anywhere.
        assert_eq!(find_part_files(&layout.segments_dir()).unwrap().len(), 0);
    }

    #[test]
    fn empty_store_salvages_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path().to_owned());
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        assert_eq!(salvage_startup(&layout, &index).unwrap(), 0);
    }
}
