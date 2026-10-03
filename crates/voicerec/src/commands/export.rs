use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use recorder_core::export::{export_mixed, export_separate, export_single, ffmpeg::default_output};
use recorder_core::store::index::Index;
use recorder_core::store::layout::{format_utc, StorageLayout};
use recorder_core::Config;

use crate::commands::list::{resolve_range, RangeArgs};

pub struct ExportArgs {
    pub range: RangeArgs,
    pub device: Option<String>,
    /// Comma-separated device substrings for multi-device export.
    pub devices: Option<String>,
    pub output: Option<PathBuf>,
    pub compact: bool,
    /// Mix all selected devices into one aligned file.
    pub mix: bool,
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

    // Multi-device mode (--devices or --mix) vs. single device.
    if args.mix || args.devices.is_some() {
        let slugs = resolve_devices(&index, args.devices.as_deref())?;
        if args.mix {
            let output = args
                .output
                .clone()
                .unwrap_or_else(|| default_output(window.start_ns, "opus"));
            println!(
                "mixing {} devices from {} to {} -> {}",
                slugs.len(),
                format_utc(window.start_ns),
                format_utc(window.end_ns),
                output.display()
            );
            export_mixed(
                &layout,
                &index,
                &slugs,
                window.start_ns,
                window.end_ns,
                args.compact,
                &output,
            )
            .context("mixed export failed")?;
            println!("done: mixed {} devices", slugs.len());
        } else {
            let stem = args
                .output
                .clone()
                .unwrap_or_else(|| default_output(window.start_ns, "opus"));
            let written = export_separate(
                &layout,
                &index,
                &slugs,
                window.start_ns,
                window.end_ns,
                args.compact,
                |slug| per_device_path(&stem, slug),
            )
            .context("export failed")?;
            for (slug, path) in &written {
                println!("  {slug} -> {}", path.display());
            }
            println!("done: {} files", written.len());
        }
        return Ok(());
    }

    // Single-device mode.
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

/// Insert a device slug before the output file's extension:
/// `meeting.opus` + `mic` -> `meeting.mic.opus`.
fn per_device_path(stem: &Path, slug: &str) -> PathBuf {
    let ext = stem.extension().and_then(|e| e.to_str()).unwrap_or("opus");
    let base = stem.with_extension("");
    PathBuf::from(format!("{}.{slug}.{ext}", base.display()))
}

/// Resolve `--devices a,b` substrings (or all devices if None) to slugs.
fn resolve_devices(
    index: &Arc<Mutex<Index>>,
    filters: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let slugs = index.lock().expect("index lock").slugs()?;
    anyhow::ensure!(!slugs.is_empty(), "no recorded devices in the index");
    match filters {
        None => Ok(slugs),
        Some(csv) => {
            let mut selected = Vec::new();
            for part in csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let matches: Vec<&String> = slugs.iter().filter(|s| s.contains(part)).collect();
                match matches.as_slice() {
                    [one] => selected.push((*one).clone()),
                    [] => anyhow::bail!("no device matches {part:?}"),
                    _ => anyhow::bail!("{part:?} matches multiple devices; be more specific"),
                }
            }
            selected.sort();
            selected.dedup();
            anyhow::ensure!(!selected.is_empty(), "no devices selected");
            Ok(selected)
        }
    }
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
