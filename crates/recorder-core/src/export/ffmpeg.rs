//! Execute an export plan with ffmpeg.
//!
//! Each [`TrackPlan`] becomes a set of ffmpeg inputs (segment files, trimmed
//! with `-ss`/`-t`; silences as `anullsrc`) concatenated per device. Multiple
//! devices are either written to separate files or mixed into one with
//! `amix` — and because every track is built to span the exact same window,
//! mixing stays aligned to wall-clock time.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::ExportError;
use crate::export::planner::{TrackPart, TrackPlan};

/// Build the ffmpeg argument vector for a single-device export. Returns the
/// args (excluding the leading "ffmpeg") so they can be inspected in tests.
pub fn single_track_args(
    plan: &TrackPlan,
    storage_root: &Path,
    output: &Path,
) -> Result<Vec<String>, ExportError> {
    if !plan.has_audio() && plan.parts.is_empty() {
        return Err(ExportError::NoAudio);
    }
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
    ];
    let (inputs, filter, label) = build_track(plan, storage_root, 0);
    for input in inputs {
        args.extend(input);
    }
    args.push("-filter_complex".into());
    args.push(filter);
    args.push("-map".into());
    args.push(format!("[{label}]"));
    args.push(output.to_string_lossy().into_owned());
    Ok(args)
}

/// Build the inputs and concat filter for one track. Returns (per-input arg
/// groups, filter string producing `[cN]`, output label).
fn build_track(
    plan: &TrackPlan,
    storage_root: &Path,
    base_input: usize,
) -> (Vec<Vec<String>>, String, String) {
    let mut inputs: Vec<Vec<String>> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    let rate = plan.sample_rate;
    let layout = channel_layout(plan.channels);

    for part in &plan.parts {
        // Defensively skip zero-duration parts: `-t 0` would make an
        // anullsrc source unbounded and hang the concat.
        if part.duration_ns() == 0 {
            continue;
        }
        let idx = base_input + inputs.len();
        match part {
            TrackPart::Segment {
                rel_path,
                seg_offset_ns,
                duration_ns,
            } => {
                let path = storage_root.join(rel_path);
                inputs.push(vec![
                    "-ss".into(),
                    ns_to_secs(*seg_offset_ns),
                    "-t".into(),
                    ns_to_secs(*duration_ns),
                    "-i".into(),
                    path.to_string_lossy().into_owned(),
                ]);
                // Normalize every piece to the same rate/layout so concat is valid.
                labels.push(format!(
                    "[{idx}:a]aresample={rate},aformat=sample_fmts=fltp:channel_layouts={layout}[s{idx}]"
                ));
            }
            TrackPart::Silence { duration_ns } => {
                inputs.push(vec![
                    "-f".into(),
                    "lavfi".into(),
                    "-t".into(),
                    ns_to_secs(*duration_ns),
                    "-i".into(),
                    format!("anullsrc=r={rate}:cl={layout}"),
                ]);
                labels.push(format!(
                    "[{idx}:a]aformat=sample_fmts=fltp:channel_layouts={layout}[s{idx}]"
                ));
            }
        }
    }

    let concat_inputs: String = (0..inputs.len()).fold(String::new(), |mut acc, i| {
        use std::fmt::Write;
        let _ = write!(acc, "[s{}]", base_input + i);
        acc
    });
    let out_label = format!("c{base_input}");
    let filter = format!(
        "{};{concat_inputs}concat=n={}:v=0:a=1[{out_label}]",
        labels.join(";"),
        inputs.len()
    );
    (inputs, filter, out_label)
}

fn channel_layout(channels: u8) -> &'static str {
    match channels {
        1 => "mono",
        _ => "stereo",
    }
}

/// Format nanoseconds as a seconds string with microsecond precision. A
/// zero here would make `anullsrc -t 0` unbounded, so callers must never emit
/// zero-duration parts (the planner's `GAP_EPSILON_NS` guarantees this).
fn ns_to_secs(ns: u64) -> String {
    format!("{}.{:06}", ns / 1_000_000_000, (ns % 1_000_000_000) / 1_000)
}

/// Run ffmpeg with the given args, returning an error with stderr on failure.
pub fn run_ffmpeg(args: &[String]) -> Result<(), ExportError> {
    let output = Command::new("ffmpeg")
        .args(args)
        .output()
        .map_err(|source| ExportError::Spawn { source })?;
    if !output.status.success() {
        return Err(ExportError::Ffmpeg {
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Default output path when the user gives none: an export file named by the
/// window, in the current directory.
pub fn default_output(start_ns: u64, ext: &str) -> PathBuf {
    PathBuf::from(format!(
        "voicerec-export-{}.{ext}",
        crate::store::layout::format_utc(start_ns).replace([':'], "-")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::planner::TrackPlan;

    const SEC: u64 = 1_000_000_000;

    fn plan(parts: Vec<TrackPart>, channels: u8) -> TrackPlan {
        TrackPlan {
            slug: "mic".into(),
            channels,
            sample_rate: 48_000,
            parts,
        }
    }

    fn joined(args: &[String]) -> String {
        args.join(" ")
    }

    #[test]
    fn ns_to_secs_has_micros() {
        assert_eq!(ns_to_secs(0), "0.000000");
        assert_eq!(ns_to_secs(1_500_000_000), "1.500000");
        assert_eq!(ns_to_secs(61_250_000_000), "61.250000");
        // Sub-millisecond values keep real precision (never collapse to 0).
        assert_eq!(ns_to_secs(666_667), "0.000666");
    }

    #[test]
    fn single_segment_trims_with_ss_and_t() {
        let p = plan(
            vec![TrackPart::Segment {
                rel_path: "seg.opus".into(),
                seg_offset_ns: 10 * SEC,
                duration_ns: 30 * SEC,
            }],
            1,
        );
        let args = single_track_args(&p, Path::new("/root"), Path::new("/out.wav")).unwrap();
        let s = joined(&args);
        assert!(s.contains("-ss 10.000000"), "{s}");
        assert!(s.contains("-t 30.000000"), "{s}");
        assert!(s.contains("-i /root/seg.opus"), "{s}");
        assert!(s.contains("concat=n=1:v=0:a=1"), "{s}");
        assert!(s.contains("/out.wav"));
    }

    #[test]
    fn silence_uses_anullsrc() {
        let p = plan(
            vec![
                TrackPart::Silence {
                    duration_ns: 5 * SEC,
                },
                TrackPart::Segment {
                    rel_path: "seg.opus".into(),
                    seg_offset_ns: 0,
                    duration_ns: 10 * SEC,
                },
            ],
            2,
        );
        let args = single_track_args(&p, Path::new("/root"), Path::new("/out.opus")).unwrap();
        let s = joined(&args);
        assert!(s.contains("anullsrc=r=48000:cl=stereo"), "{s}");
        assert!(s.contains("-t 5.000000"), "{s}");
        assert!(s.contains("concat=n=2:v=0:a=1"), "{s}");
    }

    #[test]
    fn empty_plan_errors() {
        let p = plan(vec![], 1);
        assert!(matches!(
            single_track_args(&p, Path::new("/root"), Path::new("/out.wav")),
            Err(ExportError::NoAudio)
        ));
    }
}
