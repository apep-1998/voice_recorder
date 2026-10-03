# Architecture

`voice_recorder` is a continuous background audio recorder for Linux built on
PipeWire. This document describes the overall design; details land together
with the code that implements them.

## Overview

```
                 ┌──────────────────────── daemon (voicerec daemon) ───────────────────────┐
                 │                                                                         │
 PipeWire        │  capture thread            frame bus                subscribers         │
 ┌─────────┐     │  ┌──────────────┐     ┌──────────────────┐     ┌──────────────────┐     │
 │ mic     ├──────▶│              │     │ tokio::broadcast │  ┌─▶│ Opus/Ogg encoder │──▶ segments + SQLite index
 │ monitor ├──────▶│  pipewire-rs │────▶│ <AudioFrame>     │──┤  └──────────────────┘     │
 │ ...     ├──────▶│  MainLoop    │     │                  │  │  ┌──────────────────┐     │
 └─────────┘     │  └──────────────┘     └──────────────────┘  └─▶│ fan-out socket   │──▶ external listeners
                 │                                                └──────────────────┘     │
                 │  retention task · logind watcher · device supervisor                    │
                 └─────────────────────────────────────────────────────────────────────────┘

 CLI (voicerec list / export / status / devices) reads the index and segments,
 and uses ffmpeg to trim/concat/mix exports.
```

## Crates

- `recorder-core` — library: config, device selection, capture, encoding,
  storage/index, retention, export planning, fan-out protocol. UI-agnostic.
- `voicerec` — binary: clap CLI plus the `voicerec daemon` subcommand.

## Key decisions

### Capture: native PipeWire (pipewire-rs)

The PipeWire registry gives device hotplug events, capture from sink
`.monitor` sources by node name, and precise sample timestamps. pipewire-rs
types are not `Send`, so a dedicated OS thread owns the `MainLoop` and all
streams; PCM frames (`f32`, 48 kHz) are pushed over channels to the rest of
the daemon.

### Frame bus (listener foundation)

The capture thread publishes `AudioFrame { device, utc_ns, seq, rate,
channels, samples: Arc<[f32]> }` on a `tokio::sync::broadcast` bus. The
Opus encoder is just one subscriber; the future listener fan-out socket (for
wake-word detectors and similar external programs) is another. Slow
subscribers can lag and drop frames without ever blocking recording.

### Storage: Opus-in-Ogg, 60-second segments

Each device records to its own stream of 60-second `.opus` files:

```
~/.local/share/voice_recorder/
  index.sqlite3
  segments/<device_slug>/<YYYY>/<MM>/<DD>/<UTC-start>_<session>.opus
```

Ogg is page-granular (~1 s), so a segment truncated by power loss is still
decodable up to the last complete page. Segments are written as `*.opus.part`
and renamed on clean finalize; orphaned `.part` files are salvaged on daemon
start.

The SQLite index stores segment time spans and session records, and is a
rebuildable cache (`voicerec reindex`): filenames plus Ogg granule positions
are the source of truth.

### Time, gaps, suspend

Every segment anchors wall clock (`CLOCK_REALTIME`) and monotonic clock at
its first sample; sample position then gives the exact UTC time of any
instant in the segment. Drift between the two clocks beyond a threshold
(suspend, clock step) closes the segment and starts a new session. Gaps are
simply the absence of segments; exports fill them with silence so that
mic/monitor tracks stay aligned to wall-clock for mixing.

A logind `PrepareForSleep` watcher (with a delay inhibitor) finalizes
segments before suspend; the drift detector is the fallback when the signal
is missed.

### Export

The export planner (pure, unit-testable) maps a requested time range to
overlapping segments per device and produces a trim/gap-fill/concat plan.
ffmpeg executes the plan; mixed exports align tracks with `adelay` and mix
with `amix`.

## Roadmap

See the PR sequence in the repository issues/PRs. Major steps: device
enumeration → capture engine + frame bus → segment writer + index → daemon +
systemd unit → retention/status/reindex → time parsing + list → export →
mixing → power/hotplug hardening → listener fan-out socket.
