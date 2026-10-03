//! The capture engine: a dedicated thread that owns the PipeWire main loop
//! and all capture streams, and publishes PCM frames to the [`FrameBus`].
//!
//! pipewire-rs types are not `Send`, so the whole PipeWire world lives on
//! one OS thread. Control flows in through a pipewire channel (stop), audio
//! flows out through the bus.

use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::pod::Pod;

use crate::capture::bus::{AudioFrame, BusEvent, FrameBus};
use crate::config::Config;
use crate::device::{CaptureTarget, StreamKind};
use crate::error::CaptureError;
use crate::timeline::{ClockCheck, ClockPair, Timeline};

/// Per-engine settings derived from the configuration.
#[derive(Debug, Clone, Copy)]
pub struct EngineSettings {
    /// Sample rate requested from PipeWire for every stream.
    pub sample_rate: u32,
    pub mic_channels: u8,
    pub monitor_channels: u8,
    /// Clock drift that forces a re-anchor (new session downstream).
    pub drift_threshold: Duration,
}

impl EngineSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            sample_rate: 48_000,
            mic_channels: config.encoding.mic_channels,
            monitor_channels: config.encoding.monitor_channels,
            drift_threshold: Duration::from_millis(config.power.clock_drift_threshold_ms),
        }
    }

    pub fn channels_for(&self, kind: StreamKind) -> u8 {
        match kind {
            StreamKind::Mic => self.mic_channels,
            StreamKind::Monitor => self.monitor_channels,
        }
    }
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

/// Handle to the running capture thread. Stops capture when dropped.
pub struct CaptureEngine {
    stop_tx: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureEngine {
    /// Start capturing the given targets, publishing to `bus`. Returns once
    /// all streams are created (not necessarily connected to devices yet —
    /// PipeWire links them asynchronously).
    pub fn start(
        targets: Vec<CaptureTarget>,
        settings: EngineSettings,
        bus: FrameBus,
    ) -> Result<Self, CaptureError> {
        let (stop_tx, stop_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), CaptureError>>();

        let thread = std::thread::Builder::new()
            .name("voicerec-capture".into())
            .spawn(move || capture_thread(&targets, settings, &bus, stop_rx, &ready_tx))
            .map_err(|err| {
                CaptureError::Internal(format!("failed to spawn capture thread: {err}"))
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_tx: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(CaptureError::Internal(
                    "capture thread died during startup".into(),
                ))
            }
        }
    }

    /// Stop capturing and wait for the capture thread to finish.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CaptureEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Mutable per-stream state owned by the stream's process callback.
struct StreamState {
    slug: Arc<str>,
    kind: StreamKind,
    rate: u32,
    channels: u8,
    timeline: Option<Timeline>,
    seq: u64,
    drift_threshold: Duration,
    bus: FrameBus,
}

fn capture_thread(
    targets: &[CaptureTarget],
    settings: EngineSettings,
    bus: &FrameBus,
    stop_rx: pw::channel::Receiver<()>,
    ready_tx: &mpsc::Sender<Result<(), CaptureError>>,
) {
    let result = (|| -> Result<(pw::main_loop::MainLoopRc, Guards), CaptureError> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        let mut guards = Vec::with_capacity(targets.len());
        for target in targets {
            guards.push(create_stream(&core, target, settings, bus.clone())?);
        }
        Ok((mainloop, Guards { context, guards }))
    })();

    let (mainloop, guards) = match result {
        Ok(ok) => ok,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };
    let _ = ready_tx.send(Ok(()));

    let _attached = stop_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    mainloop.run();

    for target in targets {
        bus.publish(BusEvent::StreamClosed {
            slug: Arc::from(target.slug.as_str()),
        });
    }
    drop(guards);
}

/// Keeps streams, their listeners, and the context alive for the duration of
/// the main loop.
struct Guards {
    #[allow(dead_code)]
    context: pw::context::ContextRc,
    #[allow(dead_code)]
    guards: Vec<StreamGuard>,
}

struct StreamGuard {
    #[allow(dead_code)]
    stream: pw::stream::StreamRc,
    #[allow(dead_code)]
    listener: pw::stream::StreamListener<StreamState>,
}

fn create_stream(
    core: &pw::core::CoreRc,
    target: &CaptureTarget,
    settings: EngineSettings,
    bus: FrameBus,
) -> Result<StreamGuard, CaptureError> {
    let channels = settings.channels_for(target.kind);

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
    };
    props.insert(*pw::keys::NODE_NAME, format!("voicerec.{}", target.slug));
    props.insert(*pw::keys::TARGET_OBJECT, target.node_name.as_str());
    if target.kind == StreamKind::Monitor {
        // Capture what the sink plays, via its monitor ports.
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }

    let stream = pw::stream::StreamRc::new(
        core.clone(),
        &format!("voicerec capture {}", target.slug),
        props,
    )?;

    let state = StreamState {
        slug: Arc::from(target.slug.as_str()),
        kind: target.kind,
        rate: settings.sample_rate,
        channels,
        timeline: None,
        seq: 0,
        drift_threshold: settings.drift_threshold,
        bus,
    };

    let listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(|_, state, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            let mut info = AudioInfoRaw::new();
            if info.parse(param).is_ok() && info.rate() > 0 {
                tracing::info!(
                    slug = %state.slug,
                    rate = info.rate(),
                    channels = info.channels(),
                    "stream format negotiated"
                );
                state.rate = info.rate();
                state.channels = u8::try_from(info.channels()).unwrap_or(state.channels);
                // A format change invalidates the sample clock.
                state.timeline = None;
            }
        })
        .process(|stream, state| {
            while let Some(mut buffer) = stream.dequeue_buffer() {
                process_buffer(&mut buffer, state);
            }
        })
        .register()?;

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(settings.sample_rate);
    audio_info.set_channels(u32::from(channels));
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: audio_info.into(),
        }),
    )
    .map_err(|err| CaptureError::Internal(format!("failed to serialize format pod: {err:?}")))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values)
        .ok_or_else(|| CaptureError::Internal("invalid format pod".into()))?];

    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    Ok(StreamGuard { stream, listener })
}

fn process_buffer(buffer: &mut pw::buffer::Buffer<'_>, state: &mut StreamState) {
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    let byte_count = data.chunk().size() as usize;
    let Some(bytes) = data.data() else { return };
    let bytes = &bytes[..byte_count.min(bytes.len())];

    let sample_count = bytes.len() / std::mem::size_of::<f32>();
    if sample_count == 0 || state.channels == 0 {
        return;
    }
    let mut samples = Vec::with_capacity(sample_count);
    for chunk in bytes.chunks_exact(std::mem::size_of::<f32>()) {
        samples.push(f32::from_le_bytes(chunk.try_into().expect("4-byte chunk")));
    }
    let frames = samples.len() as u64 / u64::from(state.channels);
    if frames == 0 {
        return;
    }

    let now = ClockPair::now();
    let chunk_ns = frames_to_ns(frames, state.rate);

    if state.timeline.is_none() {
        state.timeline = Some(Timeline::new(
            back_dated(now, chunk_ns),
            state.rate,
            state.drift_threshold,
        ));
    }
    let timeline = state.timeline.as_mut().expect("timeline just ensured");
    let mut start_utc = timeline.current_utc_ns();

    if let ClockCheck::Discontinuity { drift } = timeline.advance(frames, now) {
        tracing::warn!(
            slug = %state.slug,
            drift_ms = drift.as_millis(),
            "clock discontinuity; re-anchoring"
        );
        state.bus.publish(BusEvent::Discontinuity {
            slug: Arc::clone(&state.slug),
            kind: state.kind,
            drift_ns: u64::try_from(drift.as_nanos()).unwrap_or(u64::MAX),
        });
        let mut fresh = Timeline::new(back_dated(now, chunk_ns), state.rate, state.drift_threshold);
        start_utc = fresh.start_utc_ns();
        let _ = fresh.advance(frames, now);
        *timeline = fresh;
    }

    let frame = AudioFrame {
        slug: Arc::clone(&state.slug),
        kind: state.kind,
        utc_ns: start_utc,
        seq: state.seq,
        rate: state.rate,
        channels: state.channels,
        samples: Arc::from(samples),
    };
    state.seq += 1;
    state.bus.publish(BusEvent::Frame(frame));
}

fn frames_to_ns(frames: u64, rate: u32) -> u64 {
    u64::try_from(u128::from(frames) * 1_000_000_000 / u128::from(rate.max(1))).unwrap_or(u64::MAX)
}

fn back_dated(now: ClockPair, chunk_ns: u64) -> ClockPair {
    ClockPair {
        realtime_ns: now.realtime_ns.saturating_sub(chunk_ns),
        monotonic_ns: now.monotonic_ns.saturating_sub(chunk_ns),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{select_targets, CaptureTarget};

    #[test]
    fn settings_derive_from_config() {
        let settings = EngineSettings::from_config(&Config::default());
        assert_eq!(settings.sample_rate, 48_000);
        assert_eq!(settings.channels_for(StreamKind::Mic), 1);
        assert_eq!(settings.channels_for(StreamKind::Monitor), 2);
        assert_eq!(settings.drift_threshold, Duration::from_millis(250));
    }

    #[test]
    fn frame_math_helpers() {
        assert_eq!(frames_to_ns(48_000, 48_000), 1_000_000_000);
        assert_eq!(frames_to_ns(0, 48_000), 0);
        let now = ClockPair {
            realtime_ns: 10_000,
            monotonic_ns: 5_000,
        };
        let anchored = back_dated(now, 4_000);
        assert_eq!(anchored.realtime_ns, 6_000);
        assert_eq!(anchored.monotonic_ns, 1_000);
        // Saturates instead of underflowing.
        let early = back_dated(
            ClockPair {
                realtime_ns: 100,
                monotonic_ns: 100,
            },
            4_000,
        );
        assert_eq!(early.monotonic_ns, 0);
    }

    /// Live capture test; run with `VOICEREC_LIVE_TESTS=1 cargo test -- --ignored`.
    #[test]
    #[ignore = "requires a running PipeWire daemon with audio devices"]
    fn live_capture_produces_frames() {
        if std::env::var("VOICEREC_LIVE_TESTS").is_err() {
            eprintln!("VOICEREC_LIVE_TESTS not set; skipping");
            return;
        }
        let graph = crate::capture::pipewire::snapshot_graph(
            crate::capture::pipewire::DEFAULT_SNAPSHOT_TIMEOUT,
        )
        .expect("snapshot");
        let targets: Vec<CaptureTarget> =
            select_targets(&graph, &Config::default().capture).expect("targets");
        assert_ne!(targets.len(), 0);

        let bus = FrameBus::default();
        let mut rx = bus.subscribe();
        let engine =
            CaptureEngine::start(targets, EngineSettings::default(), bus).expect("engine starts");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut frames = 0_u32;
        let mut last_seq: Option<u64> = None;
        while frames < 20 && std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(BusEvent::Frame(frame)) => {
                    assert!(frame.frame_count() > 0);
                    assert_eq!(frame.rate, 48_000);
                    if frame.kind == StreamKind::Mic {
                        if let Some(prev) = last_seq {
                            assert_eq!(frame.seq, prev + 1, "mic seq must be monotonic");
                        }
                        last_seq = Some(frame.seq);
                    }
                    frames += 1;
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        engine.stop();
        assert!(frames >= 20, "only {frames} frames captured in 10s");
    }
}
