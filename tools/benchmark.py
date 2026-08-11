#!/usr/bin/env python3
"""Compare the encodings across the axes they actually use.

    python3 tools/benchmark.py [payload-bytes]

Runs each configuration through a full encode and decode, measuring wall time,
peak resident memory and output size, and verifies the payload came back
byte-for-byte before reporting anything. Prints a Markdown table.

Peak RSS comes from `/usr/bin/time -l`, which is macOS/BSD. On Linux the field
is `Maximum resident set size` from `/usr/bin/time -v`; the parser accepts
either, and reports memory as "-" if it finds neither rather than inventing a
number.

The payload is incompressible on purpose. Compressible input would measure
zstd's ratio on that particular file rather than the waveform, and the whole
point here is to compare waveforms.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "stego-flac")

# label, axes description, encode flags
CONFIGS = [
    ("16-FSK mono", "time only", ["--profile", "standard"]),
    ("4-FSK mono", "time only", ["--profile", "fast"]),
    ("OFDM QPSK", "time x freq x 2-bit cell", ["--qam-bits", "2"]),
    ("OFDM 64-QAM", "time x freq x 6-bit cell", ["--qam-bits", "6"]),
    ("OFDM 4096-QAM", "time x freq x 12-bit cell", ["--profile", "dense"]),
    ("OFDM 65536-QAM", "time x freq x 16-bit cell", ["--profile", "compact"]),
    ("OFDM 1M-QAM", "time x freq x 20-bit cell", ["--qam-bits", "20"]),
    ("4096-QAM, 2ch", "+ channel", ["--profile", "dense", "--channels", "2"]),
    ("4096-QAM, 8ch", "+ channel", ["--profile", "dense", "--channels", "8"]),
    ("65536-QAM, 8ch", "+ channel", ["--profile", "compact", "--channels", "8"]),
]


def incompressible(path, size):
    with open("/dev/urandom", "rb") as source, open(path, "wb") as sink:
        sink.write(source.read(size))


def run_timed(args):
    """Run a command, returning (seconds, peak_rss_bytes_or_None, stdout)."""
    started = time.monotonic()
    try:
        proc = subprocess.run(
            ["/usr/bin/time", "-l", *args],
            capture_output=True, text=True, timeout=1800,
        )
    except FileNotFoundError:
        proc = subprocess.run(args, capture_output=True, text=True, timeout=1800)
    elapsed = time.monotonic() - started
    if proc.returncode != 0:
        raise RuntimeError(f"{args[1] if len(args) > 1 else args[0]} failed:\n{proc.stderr[-800:]}")

    rss = None
    for pattern in (r"(\d+)\s+maximum resident set size",
                    r"Maximum resident set size[^:]*:\s*(\d+)"):
        found = re.search(pattern, proc.stderr)
        if found:
            rss = int(found.group(1))
            # GNU time reports kilobytes; BSD reports bytes.
            if "Maximum resident" in pattern:
                rss *= 1024
            break
    return elapsed, rss, proc.stdout


def human(size):
    for unit in ("B", "KiB", "MiB", "GiB"):
        if size < 1024 or unit == "GiB":
            return f"{size:.0f} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024


def duration_of(report):
    found = re.search(r"carrier\s+(.+)", report)
    return found.group(1).strip().split(" across")[0] if found else "?"


def main():
    if not os.path.exists(BIN):
        sys.exit(f"build first: cargo build --release  (missing {BIN})")

    payload_size = int(sys.argv[1]) if len(sys.argv) > 1 else 256 * 1024
    work = tempfile.mkdtemp(prefix="stego-bench-")
    payload = os.path.join(work, "payload.bin")
    incompressible(payload, payload_size)

    rows = []
    try:
        for label, axes, flags in CONFIGS:
            carrier = os.path.join(work, "c.flac")
            landing = os.path.join(work, "c.out")

            enc_s, enc_rss, report = run_timed(
                [BIN, "encode", payload, "-o", carrier, "--no-encrypt", "--force", *flags]
            )
            size = os.path.getsize(carrier)
            dec_s, dec_rss, _ = run_timed(
                [BIN, "decode", carrier, "-o", landing, "--force", "--quiet"]
            )

            with open(payload, "rb") as a, open(landing, "rb") as b:
                if a.read() != b.read():
                    raise RuntimeError(f"{label} did not round-trip")

            rows.append({
                "label": label, "axes": axes, "size": size,
                "duration": duration_of(report),
                "enc": enc_s, "dec": dec_s,
                "enc_rss": enc_rss, "dec_rss": dec_rss,
            })
            print(f"  {label:<18} ok", file=sys.stderr)
    finally:
        shutil.rmtree(work, ignore_errors=True)

    baseline = rows[0]["size"]
    print(f"\nPayload: {human(payload_size)} incompressible, unencrypted.\n")
    print("| encoding | axes used | carrier | file | vs 1-D | encode | decode | peak RSS |")
    print("|---|---|---|---|---|---|---|---|")
    for r in rows:
        mem = f"{r['enc_rss'] / 1048576:.0f} MB" if r["enc_rss"] else "-"
        print(
            f"| {r['label']} | {r['axes']} | {r['duration']} | {human(r['size'])} "
            f"| {baseline / r['size']:.0f}x | {r['enc']:.2f} s | {r['dec']:.2f} s | {mem} |"
        )


if __name__ == "__main__":
    main()
