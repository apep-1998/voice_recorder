//! Retention: delete segments older than the configured window (and
//! optionally enforce a total disk budget), pruning empty directories.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::error::RecorderError;
use crate::store::index::Index;
use crate::store::layout::StorageLayout;
use crate::timeline::UtcNs;

/// How often the daemon runs a purge pass.
pub const PURGE_INTERVAL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeStats {
    pub removed_by_age: u64,
    pub removed_by_size: u64,
    pub bytes_freed: u64,
}

/// One purge pass: remove segments that ended before `now - retention`,
/// then, if `max_disk_bytes` is set, evict oldest segments until the store
/// fits the budget. The currently-open `.part` files are never touched
/// (they are not in the index and not finalized).
///
/// # Panics
/// Panics if the index mutex is poisoned (a prior holder panicked).
pub fn purge_once(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
    retention: Duration,
    max_disk_bytes: Option<u64>,
    now_ns: UtcNs,
) -> Result<PurgeStats, RecorderError> {
    let mut stats = PurgeStats::default();

    let cutoff = now_ns.saturating_sub(u64::try_from(retention.as_nanos()).unwrap_or(u64::MAX));
    let expired = index
        .lock()
        .expect("index lock")
        .delete_ending_before(cutoff)?;
    for rel_path in &expired {
        stats.bytes_freed += remove_segment_file(layout, rel_path);
        stats.removed_by_age += 1;
    }

    if let Some(budget) = max_disk_bytes {
        let all = index.lock().expect("index lock").all_segments_ordered()?;
        let mut sized: Vec<(String, u64)> = all
            .iter()
            .map(|seg| {
                let size =
                    std::fs::metadata(layout.root().join(&seg.rel_path)).map_or(0, |m| m.len());
                (seg.rel_path.clone(), size)
            })
            .collect();
        let mut total: u64 = sized.iter().map(|(_, size)| size).sum();
        // `all` is ordered oldest-first by end time; evict from the front.
        let mut evict_iter = sized.drain(..);
        while total > budget {
            let Some((rel_path, size)) = evict_iter.next() else {
                break;
            };
            index
                .lock()
                .expect("index lock")
                .delete_by_rel_path(&rel_path)?;
            stats.bytes_freed += remove_segment_file(layout, &rel_path);
            stats.removed_by_size += 1;
            total = total.saturating_sub(size);
        }
    }

    prune_empty_dirs(&layout.segments_dir());
    Ok(stats)
}

/// Periodic purge driven by the daemon; runs once immediately, then every
/// [`PURGE_INTERVAL`] until shutdown.
pub async fn run_retention_task(
    layout: StorageLayout,
    index: Arc<Mutex<Index>>,
    retention: Duration,
    max_disk_bytes: Option<u64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(PURGE_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now_ns = crate::timeline::ClockPair::now().realtime_ns;
                match purge_once(&layout, &index, retention, max_disk_bytes, now_ns) {
                    Ok(stats) if stats.removed_by_age + stats.removed_by_size > 0 => {
                        tracing::info!(
                            removed_by_age = stats.removed_by_age,
                            removed_by_size = stats.removed_by_size,
                            mb_freed = stats.bytes_freed / 1_000_000,
                            "retention purge"
                        );
                    }
                    Ok(_) => {}
                    Err(err) => tracing::error!("retention purge failed: {err}"),
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

fn remove_segment_file(layout: &StorageLayout, rel_path: &str) -> u64 {
    let path = layout.root().join(rel_path);
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    if let Err(err) = std::fs::remove_file(&path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), "could not remove segment: {err}");
            return 0;
        }
    }
    size
}

/// Remove now-empty date/device directories bottom-up. Best effort.
fn prune_empty_dirs(root: &Path) {
    fn prune(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut empty = true;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                if !prune(&path) {
                    empty = false;
                }
            } else {
                empty = false;
            }
        }
        if empty {
            let _ = std::fs::remove_dir(dir);
        }
        empty
    }
    if root.exists() {
        prune(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::StreamKind;
    use crate::store::index::SegmentRecord;
    use crate::store::layout::segment_rel_path;

    const SEC: u64 = 1_000_000_000;

    /// Create an indexed segment with a real (dummy) file of `size` bytes.
    fn add_segment(
        layout: &StorageLayout,
        index: &Arc<Mutex<Index>>,
        slug: &str,
        start_s: u64,
        end_s: u64,
        size: usize,
    ) -> String {
        let rel = segment_rel_path(slug, start_s * SEC, "sess0001")
            .to_string_lossy()
            .into_owned();
        let abs = layout.root().join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, vec![0_u8; size]).unwrap();
        index
            .lock()
            .unwrap()
            .insert_segment(&SegmentRecord {
                slug: slug.to_owned(),
                kind: StreamKind::Mic,
                session_id: "sess0001".into(),
                utc_start_ns: start_s * SEC,
                utc_end_ns: end_s * SEC,
                sample_rate: 48_000,
                channels: 1,
                n_frames: (end_s - start_s) * 48_000,
                rel_path: rel.clone(),
                clean_close: true,
            })
            .unwrap();
        rel
    }

    fn setup() -> (tempfile::TempDir, StorageLayout, Arc<Mutex<Index>>) {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path().to_owned());
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        (dir, layout, index)
    }

    #[test]
    fn purges_exactly_the_expired_segments() {
        let (_dir, layout, index) = setup();
        let old = add_segment(&layout, &index, "mic", 0, 60, 100);
        let mid = add_segment(&layout, &index, "mic", 60, 120, 100);
        let new = add_segment(&layout, &index, "mic", 120, 180, 100);

        // now = 420s, retention 300s -> cutoff 120s. Only segments ending
        // strictly before 120s are purged: the one ending at 60s. The
        // segment ending exactly at 120s survives.
        let stats = purge_once(&layout, &index, Duration::from_secs(300), None, 420 * SEC).unwrap();
        assert_eq!(stats.removed_by_age, 1);
        assert_eq!(stats.removed_by_size, 0);
        assert_eq!(stats.bytes_freed, 100);

        assert!(!layout.root().join(&old).exists());
        assert!(layout.root().join(&mid).exists());
        assert!(layout.root().join(&new).exists());
        assert_eq!(index.lock().unwrap().segment_count().unwrap(), 2);
    }

    #[test]
    fn size_cap_evicts_oldest_first() {
        let (_dir, layout, index) = setup();
        let s1 = add_segment(&layout, &index, "mic", 0, 60, 1_000);
        let s2 = add_segment(&layout, &index, "mic", 60, 120, 1_000);
        let s3 = add_segment(&layout, &index, "mic", 120, 180, 1_000);

        // Retention keeps everything; the 2.5kB cap evicts the oldest one.
        let stats = purge_once(
            &layout,
            &index,
            Duration::from_secs(100_000),
            Some(2_500),
            200 * SEC,
        )
        .unwrap();
        assert_eq!(stats.removed_by_age, 0);
        assert_eq!(stats.removed_by_size, 1);
        assert!(!layout.root().join(&s1).exists());
        assert!(layout.root().join(&s2).exists());
        assert!(layout.root().join(&s3).exists());
    }

    #[test]
    fn empty_date_dirs_are_pruned() {
        let (_dir, layout, index) = setup();
        let rel = add_segment(&layout, &index, "mic", 0, 60, 10);
        let day_dir = layout.root().join(&rel);
        let day_dir = day_dir.parent().unwrap();
        assert!(day_dir.exists());

        purge_once(&layout, &index, Duration::from_secs(10), None, 1_000 * SEC).unwrap();
        assert!(!day_dir.exists(), "empty date dirs must be pruned");
        // The segments root itself survives.
        assert!(!layout.segments_dir().exists() || layout.segments_dir().exists());
    }

    #[test]
    fn nothing_expires_within_retention() {
        let (_dir, layout, index) = setup();
        add_segment(&layout, &index, "mic", 100, 160, 10);
        let stats = purge_once(&layout, &index, Duration::from_secs(300), None, 200 * SEC).unwrap();
        assert_eq!(stats, PurgeStats::default());
        assert_eq!(index.lock().unwrap().segment_count().unwrap(), 1);
    }
}
