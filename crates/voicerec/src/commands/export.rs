use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use recorder_core::export::{export_single, ffmpeg::default_output};
use recorder_core::store::index::Index;
use recorder_core::store::layout::{format_utc, StorageLayout};
use recorder_core::Config;

use crate::commands::list::{resolve_range, RangeArgs};

pub struct ExportArgs {
    pub range: RangeArgs,
    pub device: Option<String>,
    pub output: Option<PathBuf>,
    pub compact: bool,
}

pub fn run(config_path: Option<&Path>, args: &ExportArgs) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::load_default()?,
    };
    let layout = StorageLayout::new(config.storage.root_path());
    anyhow::ensure!(
        layout.index_path().exists(),
        "no recordings yet ({} does not exist)",
        layout.index_path().display()
    );
    let index = Arc::new(Mutex::new(Index::open(&layout.index_path())?));
    let window = resolve_range(&args.range)?;

    // Resolve the device: explicit substring match, or the only one present.
    let slug = resolve_device(&index, args.device.as_deref())?;

    let output = match &args.output {
        Some(path) => path.clone(),
        None => default_output(window.start_ns, "opus"),
    };

    println!(
        "exporting {} from {} to {} -> {}",
        slug,
        format_utc(window.start_ns),
        format_utc(window.end_ns),
        output.display()
    );
    let plan = export_single(
        &layout,
        &index,
        &slug,
        window.start_ns,
        window.end_ns,
        args.compact,
        &output,
    )
    .context("export failed")?;

    let covered: u64 = plan
        .parts
        .iter()
        .filter_map(|p| match p {
            recorder_core::export::TrackPart::Segment { duration_ns, .. } => Some(*duration_ns),
            recorder_core::export::TrackPart::Silence { .. } => None,
        })
        .sum();
    println!(
        "done: {} of audio across {} parts",
        humantime::format_duration(std::time::Duration::from_secs(covered / 1_000_000_000)),
        plan.parts.len()
    );
    Ok(())
}

fn resolve_device(index: &Arc<Mutex<Index>>, filter: Option<&str>) -> anyhow::Result<String> {
    let slugs = index.lock().expect("index lock").slugs()?;
    anyhow::ensure!(!slugs.is_empty(), "no recorded devices in the index");
    match filter {
        Some(f) => {
            let matches: Vec<&String> = slugs.iter().filter(|s| s.contains(f)).collect();
            match matches.as_slice() {
                [one] => Ok((*one).clone()),
                [] => anyhow::bail!("no device matches {f:?}; try `voicerec status`"),
                many => anyhow::bail!(
                    "{:?} matches {} devices; be more specific:\n{}",
                    f,
                    many.len(),
                    many.iter()
                        .map(|s| format!("  {s}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            }
        }
        None => match slugs.as_slice() {
            [one] => Ok(one.clone()),
            _ => anyhow::bail!(
                "multiple devices recorded; choose one with --device:\n{}",
                slugs
                    .iter()
                    .map(|s| format!("  {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        },
    }
}
