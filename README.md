# voice_recorder

Continuous background audio recorder for Linux, written in Rust.

`voice_recorder` runs as a systemd user service and continuously records your
microphone and your system audio output (what you hear from your speakers —
meetings, music, calls) into small, crash-safe Opus segments. Recordings are
kept for a configurable retention window (e.g. the last 5 days) and older audio
is deleted automatically. A CLI (`voicerec`) lets you list what was recorded
and export any time range — "the last 2 hours", "from 8 hours ago to 4 hours
ago", "yesterday 15:50 to 16:20" — as a single audio file, including a mixed
mic + output export (e.g. a complete meeting with both sides of the
conversation).

## Features

- **Continuous recording** of the default microphone and the default output
  monitor (configurable; an `all` mode records every mic and every output
  device to separate files).
- **Retention window**: keep the last N hours/days (`retention = "5d"`),
  older segments are purged automatically.
- **Time-range export**: `voicerec export --last 2h -o out.opus`, with
  mic/output mixing aligned by wall-clock time:

  ```sh
  voicerec list --last 2h                 # what was recorded, with gaps
  voicerec export --last 2h -o out.opus    # one device, last 2 hours
  voicerec export --from "8h ago" --to "4h ago" -o window.opus
  voicerec export --from "yesterday 15:50" --to "yesterday 16:20" -o mtg.opus
  voicerec export --last 1h --mix -o meeting.opus      # mic + output mixed
  voicerec export --last 1h --devices DJI,monitor      # one file per device
  ```
- **Crash & power safe**: 60-second Ogg/Opus segments survive power loss,
  suspend, lid close, and reboots; gaps are tracked, not corrupted.
- **Listener fan-out**: external programs (e.g. a "hey jarvis" wake-word
  detector) can subscribe to the live audio stream over a Unix socket while
  recording continues — see [docs/PROTOCOL.md](docs/PROTOCOL.md) and
  [examples/subscribe.py](examples/subscribe.py). Enable with `[fanout]
  enabled = true`.

## Status

Under active development. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for
the design.

## Requirements

- Linux with PipeWire (tested with PipeWire 1.6)
- `libopus` (system library)
- `ffmpeg` (for export)
- Rust stable toolchain (to build)

## Quick start

```sh
cargo install --path crates/voicerec

# write the default config to ~/.config/voice_recorder/config.toml
voicerec config init

# see your devices and what would be recorded
voicerec devices

# run the recorder in the foreground (Ctrl-C to stop)
voicerec daemon
```

## Run as a service

```sh
mkdir -p ~/.config/systemd/user
cp systemd/voice-recorder.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now voice-recorder

journalctl --user -u voice-recorder -f   # watch the logs
```

The service restarts automatically and finalizes segments cleanly on stop,
suspend, and shutdown. To record before you log in (e.g. right after boot),
enable lingering: `loginctl enable-linger $USER` (optional).

## Configuration

The config file lives at `~/.config/voice_recorder/config.toml` and is created
with defaults by `voicerec config init`:

```toml
[storage]
root = "~/.local/share/voice_recorder"
retention = "5d"            # how long to keep recordings: "12h", "7d", ...
segment_duration = "60s"
# max_disk_bytes = "30GB"   # optional hard cap, oldest segments evicted first

[capture]
mode = "selected"           # "selected" | "all"
mic = "default"             # or an exact PipeWire node name
output = "default"          # sink whose monitor is recorded; "none" disables

[encoding]
mic_bitrate = 32000
monitor_bitrate = 64000
mic_channels = 1
monitor_channels = 2

[fanout]
enabled = false
socket_path = "$XDG_RUNTIME_DIR/voice_recorder/audio.sock"

[power]
logind_integration = true
clock_drift_threshold_ms = 250
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
