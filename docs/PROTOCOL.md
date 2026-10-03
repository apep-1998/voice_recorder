# Fan-out socket protocol

The recorder can expose the **live** audio of the devices it is capturing to
external programs over a Unix domain socket, while it keeps writing segments.
This is the integration point for things like a "hey jarvis" wake-word
detector, a live transcriber, or any program that wants to react to audio in
real time: it subscribes, reads PCM frames, and triggers whatever it likes —
it is never part of the recorder and can never block or corrupt recording (a
slow reader is told it lagged and the recorder moves on).

Enable it in the config:

```toml
[fanout]
enabled = true
socket_path = "$XDG_RUNTIME_DIR/voice_recorder/audio.sock"
```

A complete example client is in [`examples/subscribe.py`](../examples/subscribe.py).

## Handshake

1. The client connects to the socket and sends **one newline-terminated line
   of JSON**:

   ```json
   {"v":1,"subscribe":["default-mic","monitor"],"format":"s16le","rate":16000,"channels":1}
   ```

   | Field | Meaning |
   |-------|---------|
   | `v` | Protocol version (currently `1`) |
   | `subscribe` | Device-slug substrings to receive; empty/omitted = all recorded devices |
   | `format` | `s16le` (default) or `f32le` |
   | `rate` | Desired sample rate; `0`/omitted = native 48000 |
   | `channels` | Desired channel count; `0`/omitted = native |

2. The server replies with **one line of JSON**:

   ```json
   {"v":1,"ok":true,"streams":[{"id":0,"device":"alsa_input...","rate":16000,"channels":1}]}
   ```

   On error: `{"v":1,"ok":false,"error":"no matching devices"}` and the socket
   closes.

## Binary frames

After a successful handshake the server streams binary frames until the client
disconnects. Each frame is a fixed **32-byte header** followed by its payload.
All integers are little-endian.

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | magic `"VRFA"` |
| 4 | 1 | version |
| 5 | 1 | frame type: `1` = PCM, `2` = DROP |
| 6 | 2 | stream id (`id` from the handshake) |
| 8 | 8 | `utc_ns` — UTC of the first sample (ns since the Unix epoch) |
| 16 | 4 | `n_samples` — samples per channel in the payload |
| 20 | 4 | `payload_len` — payload bytes following the header |
| 24 | 8 | `seq` — per-stream sequence number |

- **PCM** frames carry `payload_len` bytes of interleaved PCM in the requested
  `format`.
- **DROP** frames have no payload (`payload_len = 0`); `seq` holds the number
  of frames the server had to drop because this client was reading too slowly.
  The recorder is never slowed down by a slow listener — it drops for that one
  client and tells it how many.

## Notes

- `utc_ns` lets a listener align fan-out audio with exported segments (both use
  the same wall clock).
- Rate/channel conversion beyond format conversion is a planned refinement;
  today the server reports and sends the native 48 kHz stream with the
  requested sample *format*.
