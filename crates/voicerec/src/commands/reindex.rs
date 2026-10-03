use std::path::Path;
use std::sync::{Arc, Mutex};

use recorder_core::store::index::Index;
use recorder_core::store::layout::StorageLayout;
use recorder_core::store::reindex::reindex;
use recorder_core::Config;

pub fn run(config_path: Option<&Path>) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::load_default()?,
    };
    let layout = StorageLayout::new(config.storage.root_path());
    let index = Arc::new(Mutex::new(Index::open(&layout.index_path())?));
    let stats = reindex(&layout, &index)?;
    println!(
        "reindexed {} segments ({} salvaged from .part files, {} skipped)",
        stats.indexed, stats.salvaged, stats.skipped
    );
    Ok(())
}
