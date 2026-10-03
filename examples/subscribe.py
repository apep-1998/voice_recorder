#!/usr/bin/env python3
"""Minimal example listener for the voice_recorder fan-out socket.

Connects, subscribes to one or more devices, and prints a running peak level
per stream. This is the shape a "hey jarvis" wake-word detector would take:
read frames from the socket, feed the PCM to your own engine, trigger whatever
you like — all while voice_recorder keeps recording independently.

Usage:
    python3 subscribe.py [SOCKET_PATH] [DEVICE_SUBSTR ...]

Example:
    python3 subscribe.py "$XDG_RUNTIME_DIR/voice_recorder/audio.sock" DJI
"""
import json
import os
import socket
import struct
import sys

HEADER_LEN = 32
MAGIC = b"VRFA"
TYPE_PCM = 1
TYPE_DROP = 2


def recv_exact(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def main():
    default_sock = os.path.join(
        os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "voice_recorder", "audio.sock"
    )
    sock_path = sys.argv[1] if len(sys.argv) > 1 else default_sock
    devices = sys.argv[2:]  # substrings; empty = all

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(sock_path)

    request = {"v": 1, "subscribe": devices, "format": "s16le", "rate": 16000, "channels": 1}
    sock.sendall((json.dumps(request) + "\n").encode())

    # Read the one-line JSON reply.
    reply = bytearray()
    while not reply.endswith(b"\n"):
        b = sock.recv(1)
        if not b:
            print("server closed during handshake", file=sys.stderr)
            return 1
        reply.extend(b)
    info = json.loads(reply.decode())
    if not info.get("ok"):
        print("subscribe failed:", info.get("error"), file=sys.stderr)
        return 1
    streams = {s["id"]: s["device"] for s in info["streams"]}
    print("subscribed:", json.dumps(info["streams"], indent=2))

    while True:
        header = recv_exact(sock, HEADER_LEN)
        if header is None:
            print("stream ended")
            return 0
        assert header[0:4] == MAGIC, "bad frame magic"
        ftype = header[5]
        stream_id = struct.unpack_from("<H", header, 6)[0]
        utc_ns = struct.unpack_from("<Q", header, 8)[0]
        n_samples = struct.unpack_from("<I", header, 16)[0]
        payload_len = struct.unpack_from("<I", header, 20)[0]
        seq = struct.unpack_from("<Q", header, 24)[0]

        if ftype == TYPE_DROP:
            print(f"[DROP] missed {seq} frames (slow consumer)")
            continue

        payload = recv_exact(sock, payload_len)
        if payload is None:
            return 0
        # s16le mono: compute a peak level.
        samples = struct.unpack(f"<{n_samples}h", payload[: n_samples * 2])
        peak = max((abs(s) for s in samples), default=0) / 32767.0
        device = streams.get(stream_id, f"stream{stream_id}")
        bar = "#" * int(peak * 40)
        print(f"{device[:30]:30} seq={seq:6} {peak:5.2f} |{bar}")


if __name__ == "__main__":
    sys.exit(main())
