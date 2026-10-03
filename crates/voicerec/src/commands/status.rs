use std::path::Path;

use recorder_core::store::index::Index;
use recorder_core::store::layout::{format_utc, StorageLayout};
use recorder_core::Config;

pub fn run(config_path: Option<&Path>) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::load_default()?,
    };
    let layout = StorageLayout::new(config.storage.root_path());
    println!("storage root : {}", layout.root().display());
    println!(
        "retention    : {}",
        humantime::format_duration(config.storage.retention)
    );

    if !layout.index_path().exists() {
        println!("no recordings yet (index does not exist)");
        return Ok(());
    }
    let index = Index::open(&layout.index_path())?;

    let count = index.segment_count()?;
    println!("segments     : {count}");
    if let Some((oldest, newest)) = index.time_span()? {
        println!("oldest audio : {}", format_utc(oldest));
        println!("newest audio : {}", format_utc(newest));
    }
    let disk_bytes = dir_size(&layout.segments_dir());
    println!(
        "disk usage   : {} MB{}",
        disk_bytes / 1_000_000,
        config
            .storage
            .max_disk_bytes
            .map(|cap| format!(" (cap {} MB)", cap / 1_000_000))
            .unwrap_or_default()
    );

    let summaries = index.slug_summaries()?;
    if !summaries.is_empty() {
        println!("devices:");
        for (slug, segments, last_end) in summaries {
            println!(
                "  {slug}: {segments} segments, last audio {}",
                format_utc(last_end)
            );
        }
    }
    Ok(())
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                total += entry.metadata().map_or(0, |m| m.len());
            }
        }
    }
    total
}
