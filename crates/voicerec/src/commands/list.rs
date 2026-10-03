use std::path::Path;

use anyhow::Context;
use recorder_core::store::coverage::coverage;
use recorder_core::store::index::Index;
use recorder_core::store::layout::{format_utc, StorageLayout};
use recorder_core::timeparse::{parse_last, parse_range, ParseContext, TimeRange};
use recorder_core::Config;

/// Arguments shared by `list` and `export` for selecting a time window.
pub struct RangeArgs {
    pub last: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Resolve the requested window, defaulting to the last hour.
pub fn resolve_range(args: &RangeArgs) -> anyhow::Result<TimeRange> {
    let ctx = ParseContext::system();
    match (&args.last, &args.from) {
        (Some(last), _) => parse_last(last, ctx).context("invalid --last duration"),
        (None, Some(from)) => {
            parse_range(from, args.to.as_deref(), ctx).context("invalid --from/--to range")
        }
        (None, None) => parse_last("1h", ctx).map_err(Into::into),
    }
}

pub fn run(
    config_path: Option<&Path>,
    range: &RangeArgs,
    device_filter: Option<&str>,
) -> anyhow::Result<()> {
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
    let index = Index::open(&layout.index_path())?;
    let window = resolve_range(range)?;

    println!(
        "recordings from {} to {}",
        format_utc(window.start_ns),
        format_utc(window.end_ns)
    );

    let slugs: Vec<String> = match device_filter {
        Some(filter) => index
            .slugs()?
            .into_iter()
            .filter(|s| s.contains(filter))
            .collect(),
        None => index.slugs()?,
    };
    if slugs.is_empty() {
        println!("  (no matching devices)");
        return Ok(());
    }

    for slug in slugs {
        let segments = index.overlapping(&slug, window.start_ns, window.end_ns)?;
        // Bridge sub-second gaps (dropped frames) but show real gaps.
        let cov = coverage(&segments, window.start_ns, window.end_ns, 1_000_000_000);
        println!("\n{slug}");
        println!(
            "  {} segments, {} recorded, {} missing",
            cov.segment_count,
            format_duration_ns(cov.covered_ns()),
            format_duration_ns(cov.gap_ns())
        );
        for span in &cov.covered {
            println!(
                "    audio {} .. {} ({})",
                format_utc(span.start_ns),
                format_utc(span.end_ns),
                format_duration_ns(span.duration_ns())
            );
        }
        for gap in &cov.gaps {
            if gap.duration_ns() >= 1_000_000_000 {
                println!(
                    "    gap   {} .. {} ({})",
                    format_utc(gap.start_ns),
                    format_utc(gap.end_ns),
                    format_duration_ns(gap.duration_ns())
                );
            }
        }
    }
    Ok(())
}

fn format_duration_ns(ns: u64) -> String {
    let secs = ns / 1_000_000_000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}
