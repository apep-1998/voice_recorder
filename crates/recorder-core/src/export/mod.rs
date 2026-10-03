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
