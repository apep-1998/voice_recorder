//! Time-range export: plan which segments cover a window and render them to a
//! single audio file with ffmpeg.

pub mod ffmpeg;
pub mod planner;

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::error::ExportError;
use crate::store::index::Index;
use crate::store::layout::StorageLayout;
use crate::timeline::UtcNs;

pub use planner::{plan_track, TrackPart, TrackPlan};

/// Export `[start_ns, end_ns)` of one device to `output` (format from the
/// output file extension). Returns the rendered track plan for inspection.
///
/// # Panics
/// Panics if the index mutex is poisoned (a prior holder panicked).
pub fn export_single(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
    slug: &str,
    start_ns: UtcNs,
    end_ns: UtcNs,
    compact: bool,
    output: &Path,
) -> Result<TrackPlan, ExportError> {
    let segments = index
        .lock()
        .expect("index lock")
        .overlapping(slug, start_ns, end_ns)?;
    let plan = plan_track(slug, &segments, start_ns, end_ns, compact);
    if !plan.has_audio() {
        return Err(ExportError::NoAudio);
    }
    let args = ffmpeg::single_track_args(&plan, layout.root(), output)?;
    ffmpeg::run_ffmpeg(&args)?;
    Ok(plan)
}

/// Export several devices mixed into one aligned file (mic + monitor =
/// complete meeting).
///
/// # Panics
/// Panics if the index mutex is poisoned.
pub fn export_mixed(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
    slugs: &[String],
    start_ns: UtcNs,
    end_ns: UtcNs,
    compact: bool,
    output: &Path,
) -> Result<Vec<TrackPlan>, ExportError> {
    let plans = plan_tracks(index, slugs, start_ns, end_ns, compact)?;
    if !plans.iter().any(TrackPlan::has_audio) {
        return Err(ExportError::NoAudio);
    }
    let args = ffmpeg::mixed_args(&plans, layout.root(), output)?;
    ffmpeg::run_ffmpeg(&args)?;
    Ok(plans)
}

/// Export several devices to a separate file each (via `output_for(slug)`).
/// Devices with no audio in the window are skipped.
///
/// # Panics
/// Panics if the index mutex is poisoned.
pub fn export_separate(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
    slugs: &[String],
    start_ns: UtcNs,
    end_ns: UtcNs,
    compact: bool,
    output_for: impl Fn(&str) -> std::path::PathBuf,
) -> Result<Vec<(String, std::path::PathBuf)>, ExportError> {
    let plans = plan_tracks(index, slugs, start_ns, end_ns, compact)?;
    let mut written = Vec::new();
    for plan in plans {
        if !plan.has_audio() {
            continue;
        }
        let path = output_for(&plan.slug);
        let args = ffmpeg::single_track_args(&plan, layout.root(), &path)?;
        ffmpeg::run_ffmpeg(&args)?;
        written.push((plan.slug, path));
    }
    if written.is_empty() {
        return Err(ExportError::NoAudio);
    }
    Ok(written)
}

fn plan_tracks(
    index: &Arc<Mutex<Index>>,
    slugs: &[String],
    start_ns: UtcNs,
    end_ns: UtcNs,
    compact: bool,
) -> Result<Vec<TrackPlan>, ExportError> {
    let guard = index.lock().expect("index lock");
    let mut plans = Vec::with_capacity(slugs.len());
    for slug in slugs {
        let segments = guard.overlapping(slug, start_ns, end_ns)?;
        plans.push(plan_track(slug, &segments, start_ns, end_ns, compact));
    }
    Ok(plans)
}
