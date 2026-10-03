//! End-to-end export test: write real Opus segments to a store, index them,
//! export a time window, and verify the output with ffprobe.
//!
//! Requires `ffmpeg`/`ffprobe` on PATH; skips if absent.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::process::Command;
use std::sync::{Arc, Mutex};

use recorder_core::device::StreamKind;
use recorder_core::encode::SegmentWriter;
use recorder_core::export::export_single;
use recorder_core::store::index::{Index, SegmentRecord};
use recorder_core::store::layout::{segment_rel_path, StorageLayout};

const SEC: u64 = 1_000_000_000;
const RATE: u32 = 48_000;

fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn probe_duration_secs(path: &std::path::Path) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("duration parses")
}

/// Write a real Opus segment of `secs` seconds of a sine tone and index it.
fn write_segment(
    layout: &StorageLayout,
    index: &Arc<Mutex<Index>>,
    slug: &str,
    start_ns: u64,
    secs: u64,
    freq: f32,
) {
    let rel = segment_rel_path(slug, start_ns, "sess0001");
    let abs = layout.root().join(&rel);
    let mut writer = SegmentWriter::create(abs, 1, 32_000).unwrap();
    let total = (secs * u64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..total)
        .map(|i| (i as f32 * freq * 2.0 * std::f32::consts::PI / RATE as f32).sin() * 0.5)
        .collect();
    writer.write_samples(&samples).unwrap();
    let finalized = writer.finalize().unwrap();
    index
        .lock()
        .unwrap()
        .insert_segment(&SegmentRecord {
            slug: slug.to_owned(),
            kind: StreamKind::Mic,
            session_id: "sess0001".into(),
            utc_start_ns: start_ns,
            utc_end_ns: start_ns + secs * SEC,
            sample_rate: RATE,
            channels: 1,
            n_frames: finalized.n_frames,
            rel_path: rel.to_string_lossy().into_owned(),
            clean_close: true,
        })
        .unwrap();
}

#[test]
fn exports_window_with_gap_at_correct_duration() {
    if !ffprobe_available() {
        eprintln!("ffprobe not found; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let layout = StorageLayout::new(dir.path().to_owned());
    let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));

    let base = 1_000_000_000_000_u64;
    // 10s tone, 10s gap, 10s tone.
    write_segment(&layout, &index, "mic", base, 10, 440.0);
    write_segment(&layout, &index, "mic", base + 20 * SEC, 10, 880.0);

    // Export the whole 30s window (gaps filled with silence).
    let out = dir.path().join("out.wav");
    let plan = export_single(&layout, &index, "mic", base, base + 30 * SEC, false, &out).unwrap();

    assert!(out.exists());
    // 3 parts: segment, silence, segment.
    assert_eq!(plan.parts.len(), 3);
    assert_eq!(plan.total_ns(), 30 * SEC);

    let dur = probe_duration_secs(&out);
    assert!((dur - 30.0).abs() < 0.1, "expected ~30s, got {dur}");
}

#[test]
fn compact_export_skips_the_gap() {
    if !ffprobe_available() {
        eprintln!("ffprobe not found; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let layout = StorageLayout::new(dir.path().to_owned());
    let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));

    let base = 1_000_000_000_000_u64;
    write_segment(&layout, &index, "mic", base, 10, 440.0);
    write_segment(&layout, &index, "mic", base + 20 * SEC, 10, 880.0);

    let out = dir.path().join("compact.wav");
    export_single(&layout, &index, "mic", base, base + 30 * SEC, true, &out).unwrap();

    // Compact output is just the 20s of real audio, no silent gap.
    let dur = probe_duration_secs(&out);
    assert!((dur - 20.0).abs() < 0.1, "expected ~20s, got {dur}");
}

#[test]
fn exporting_an_empty_window_errors() {
    let dir = tempfile::tempdir().unwrap();
    let layout = StorageLayout::new(dir.path().to_owned());
    let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
    let out = dir.path().join("none.wav");
    let err = export_single(&layout, &index, "mic", 0, 100 * SEC, false, &out);
    assert!(err.is_err());
    assert!(!out.exists());
}
