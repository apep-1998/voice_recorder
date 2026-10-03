use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use recorder_core::capture::bus::{BusEvent, FrameBus};
use recorder_core::capture::engine::{CaptureEngine, EngineSettings};
use recorder_core::capture::pipewire::{snapshot_graph, DEFAULT_SNAPSHOT_TIMEOUT};
use recorder_core::device::select_targets;
use recorder_core::Config;

pub fn run(config_path: Option<&Path>, seconds: u64, output_dir: &Path) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::load_default()?,
    };

    let graph = snapshot_graph(DEFAULT_SNAPSHOT_TIMEOUT)
        .context("could not enumerate PipeWire audio devices")?;
    let targets = select_targets(&graph, &config.capture)?;
    anyhow::ensure!(!targets.is_empty(), "nothing to record");

    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("could not create {}", output_dir.display()))?;

    println!("recording {} device(s) for {seconds}s:", targets.len());
    for t in &targets {
        println!("  {}", t.slug);
    }

    let bus = FrameBus::default();
    let mut rx = bus.subscribe();
    let engine = CaptureEngine::start(targets, EngineSettings::from_config(&config), bus)
        .context("could not start capture engine")?;

    let mut writers: HashMap<String, (hound::WavWriter<std::io::BufWriter<std::fs::File>>, u64)> =
        HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);

    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(BusEvent::Frame(frame)) => {
                let (writer, written) = match writers.entry(frame.slug.to_string()) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let path = wav_path(output_dir, &frame.slug);
                        let spec = hound::WavSpec {
                            channels: u16::from(frame.channels),
                            sample_rate: frame.rate,
                            bits_per_sample: 32,
                            sample_format: hound::SampleFormat::Float,
                        };
                        let writer = hound::WavWriter::create(&path, spec)
                            .with_context(|| format!("could not create {}", path.display()))?;
                        println!("  writing {}", path.display());
                        e.insert((writer, 0))
                    }
                };
                for &sample in frame.samples.iter() {
                    writer.write_sample(sample)?;
                }
                *written += frame.frame_count();
            }
            Ok(BusEvent::Discontinuity { slug, drift_ns, .. }) => {
                println!("  discontinuity on {slug}: {} ms", drift_ns / 1_000_000);
            }
            Ok(BusEvent::StreamClosed { .. }) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => anyhow::bail!("bus error: {err}"),
        }
    }
    engine.stop();

    anyhow::ensure!(
        !writers.is_empty(),
        "no audio arrived — are the configured devices active?"
    );
    for (slug, (writer, written)) in writers {
        writer.finalize()?;
        println!("  {slug}: {written} frames written");
    }
    Ok(())
}

fn wav_path(dir: &Path, slug: &str) -> PathBuf {
    dir.join(format!("{slug}.wav"))
}
