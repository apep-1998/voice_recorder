//! The recorder service: wires config → device selection → capture engine →
//! per-device segment sinks. This is what `voicerec daemon` runs.

pub mod sink;

use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::capture::bus::{BusEvent, FrameBus};
use crate::capture::engine::{CaptureEngine, EngineSettings};
use crate::capture::pipewire::{snapshot_graph, DEFAULT_SNAPSHOT_TIMEOUT};
use crate::config::Config;
use crate::device::{select_targets, StreamKind};
use crate::error::RecorderError;
use crate::power::{SleepEvent, SleepWatcher};
use crate::store::index::Index;
use crate::store::layout::StorageLayout;
use crate::store::salvage::salvage_startup;

use self::sink::{run_sink, SinkSettings};

/// Run the recorder until `shutdown` flips to `true`. Blocks (async) for the
/// whole daemon lifetime.
pub async fn run(config: &Config, shutdown: watch::Receiver<bool>) -> Result<(), RecorderError> {
    let layout = StorageLayout::new(config.storage.root_path());
    let index = Arc::new(Mutex::new(Index::open(&layout.index_path())?));

    // Promote any .part files a previous crash left behind.
    let salvaged = salvage_startup(&layout, &index)?;
    if salvaged > 0 {
        tracing::info!(salvaged, "salvaged segments from a previous run");
    }

    let graph = snapshot_graph(DEFAULT_SNAPSHOT_TIMEOUT)?;
    let targets = select_targets(&graph, &config.capture)?;
    if targets.is_empty() {
        return Err(RecorderError::NothingToRecord);
    }
    for target in &targets {
        tracing::info!(slug = %target.slug, kind = target.kind.as_str(), "recording");
    }

    let bus = FrameBus::default();
    let mut sinks = Vec::with_capacity(targets.len());
    for target in &targets {
        let settings = SinkSettings {
            bitrate: match target.kind {
                StreamKind::Mic => config.encoding.mic_bitrate,
                StreamKind::Monitor => config.encoding.monitor_bitrate,
            },
            segment_duration: config.storage.segment_duration,
        };
        sinks.push(tokio::spawn(run_sink(
            bus.subscribe(),
            Arc::from(target.slug.as_str()),
            target.kind,
            layout.clone(),
            Arc::clone(&index),
            settings,
            shutdown.clone(),
        )));
    }

    let engine = CaptureEngine::start(targets, EngineSettings::from_config(config), bus.clone())?;

    let retention_task = tokio::spawn(crate::store::retention::run_retention_task(
        layout.clone(),
        Arc::clone(&index),
        config.storage.retention,
        config.storage.max_disk_bytes,
        shutdown.clone(),
    ));

    // Optional: finalize segments before the machine suspends. The clock-drift
    // detector in the capture engine already turns a suspend into a clean gap,
    // so this is a best-effort refinement — failure to connect to logind is
    // logged, not fatal.
    if config.power.logind_integration {
        spawn_sleep_watcher(bus.clone(), shutdown.clone());
    }

    // Wait for shutdown, then stop capture so sinks see StreamClosed events
    // and finalize their segments.
    let mut shutdown_rx = shutdown.clone();
    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }
    tracing::info!("shutting down: finalizing segments");
    engine.stop();
    drop(bus);

    for sink in sinks {
        match sink.await {
            Ok(Ok(segments)) => tracing::debug!(segments, "sink finished"),
            Ok(Err(err)) => tracing::error!("sink failed: {err}"),
            Err(err) => tracing::error!("sink task panicked: {err}"),
        }
    }
    let _ = retention_task.await;
    Ok(())
}

/// Spawn a background thread that watches logind for suspend events and asks
/// the sinks to flush before the machine sleeps. Best-effort: if logind is
/// unavailable the thread logs and exits, leaving the drift detector as the
/// safety net.
fn spawn_sleep_watcher(bus: FrameBus, shutdown: watch::Receiver<bool>) {
    std::thread::Builder::new()
        .name("voicerec-sleep".into())
        .spawn(move || {
            let mut watcher = match crate::power::ZbusSleepWatcher::connect() {
                Ok(w) => w,
                Err(err) => {
                    tracing::warn!("logind unavailable; relying on drift detection: {err}");
                    return;
                }
            };
            tracing::info!("watching logind for suspend/resume");
            while !*shutdown.borrow() {
                match watcher.next_event() {
                    Some(SleepEvent::GoingToSleep) => {
                        tracing::info!("suspend imminent; flushing segments");
                        bus.publish(BusEvent::FlushAll);
                        // Give sinks a moment to finalize within the inhibitor
                        // window before the kernel freezes us.
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    Some(SleepEvent::Resumed) => {
                        tracing::info!("resumed from suspend");
                    }
                    None => break,
                }
            }
        })
        .ok();
}
