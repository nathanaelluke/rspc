#!/usr/bin/env python3
"""Measure cached catalog RPCs with buffered I/O; never print library contents."""
import argparse
import json
import os
from pathlib import Path
import socket
import statistics
import time


def default_socket():
    state = Path(os.environ.get("XDG_STATE_HOME", str(Path.home() / ".local/state"))) / "rspc"
    runtime = Path(os.environ.get("XDG_RUNTIME_DIR", str(state / "run")))
    return runtime / "rspc/mpd.sock"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", type=Path, default=default_socket())
    parser.add_argument("--samples", type=int, default=20)
    parser.add_argument("--artist", help="Also measure the first release-index page for this artist")
    args = parser.parse_args()
    if args.artist and any(c in args.artist for c in "\r\n"):
        parser.error("artist must be one line")
    if args.samples < 1:
        parser.error("samples must be positive")
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(20)
        client.connect(str(args.socket))
        stream = client.makefile("rwb")
        assert stream.readline().startswith(b"OK MPD ")

        def request(command):
            start = time.perf_counter_ns()
            stream.write(command.encode() + b"\n")
            stream.flush()
            count = 0
            while True:
                line = stream.readline()
                if line == b"OK\n":
                    return (time.perf_counter_ns() - start) / 1e6, count
                if not line or line.startswith(b"ACK "):
                    raise RuntimeError(line.decode(errors="replace"))
                count += len(line)

        commands = ["status", "list Artist", "list Album", "listplaylists"]
        if args.artist:
            commands.append(f"rspc_artist_albums {json.dumps(args.artist, ensure_ascii=False)} 0")
        for command in commands:
            request(command)  # prime derived cache; measure warm requests below
            samples = [request(command) for _ in range(args.samples)]
            timings = sorted(t for t, _ in samples)
            print(json.dumps({"command": "artist release index" if command.startswith("rspc_artist_albums") else command, "samples": args.samples,
                              "median_ms": round(statistics.median(timings), 3),
                              "p95_ms": round(timings[min(len(timings) - 1, int(len(timings) * .95))], 3),
                              "response_bytes": samples[-1][1]}))


if __name__ == "__main__":
    main()
