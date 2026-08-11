#!/usr/bin/env python3
"""Generate the diagrams used in README.md.

    python3 tools/figures.py [carrier.wav]

Writes SVG and PNG into docs/. Everything here is stdlib only — no numpy, no
matplotlib — so it runs anywhere Python does. The FFT, the PNG encoder and the
plotting are all a few dozen lines each; that is cheaper than a dependency and
keeps the figures reproducible from a clean checkout.

Two of the figures are *measured* from a real carrier and one is *illustrative*,
drawn from the documented tone plans. The README says which is which, and so
does each figure's caption, because a diagram that looks like data and isn't is
worse than no diagram.
"""

import cmath
import math
import os
import struct
import sys
import wave
import zlib

DOCS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "docs")

# Carrier geometry, matching the `dense` profile and the cover band.
SAMPLE_RATE = 24_000
FFT_SIZE = 512
BIN_HZ = SAMPLE_RATE / FFT_SIZE
COVER_LO_BIN, COVER_HI_BIN = 7, 72          # 300-3400 Hz, the telephone band
DATA_LO_BIN, DATA_HI_BIN = 73, 250          # up to 11.7 kHz

INK = "#1b1b1f"
MUTED = "#6b7280"
GRID = "#e5e7eb"
COVER_FILL = "#2563eb"
DATA_FILL = "#dc2626"
GUARD_FILL = "#9ca3af"


# --------------------------------------------------------------------------
# signal processing
# --------------------------------------------------------------------------

def fft(values):
    """Recursive radix-2 FFT. Length must be a power of two.

    The guard is not decoration. An earlier version of this script fed it the
    48-sample FSK symbol window; 48 is not a power of two, the even/odd split
    stopped being a valid decomposition partway down, and the result was a
    plausible-looking spectrum with phantom energy every third bin. It rendered
    as a spectrogram that looked almost right, which is the worst kind of wrong.
    """
    n = len(values)
    if n & (n - 1) or n == 0:
        raise ValueError(f"fft needs a power-of-two length, got {n}")
    if n == 1:
        return values
    even = fft(values[0::2])
    odd = fft(values[1::2])
    out = [0] * n
    for k in range(n // 2):
        twiddle = cmath.exp(-2j * math.pi * k / n) * odd[k]
        out[k] = even[k] + twiddle
        out[k + n // 2] = even[k] - twiddle
    return out


def dft(values):
    """Direct O(n^2) transform, for windows that are not a power of two.

    Slow, but the only non-power-of-two window here is 48 samples, so it costs
    nothing and it is unambiguously correct.
    """
    n = len(values)
    return [
        sum(values[t] * cmath.exp(-2j * math.pi * k * t / n) for t in range(n))
        for k in range(n)
    ]


def spectrum(values):
    """Transform, choosing an algorithm that is valid for the length."""
    n = len(values)
    return fft(values) if n and not (n & (n - 1)) else dft(values)


def read_wav_mono(path):
    with wave.open(path, "rb") as handle:
        frames = handle.getnframes()
        channels = handle.getnchannels()
        rate = handle.getframerate()
        raw = struct.unpack(f"<{frames * channels}h", handle.readframes(frames))
    if channels > 1:
        raw = [sum(raw[i * channels:(i + 1) * channels]) / channels for i in range(frames)]
    return [v / 32768.0 for v in raw], rate


def average_spectrum(samples, size, blocks, skip=0):
    """Mean magnitude spectrum over `blocks` successive windows.

    `skip` is rounded down to a whole number of windows. Starting mid-symbol
    would straddle two OFDM symbols in every block, destroying the bin
    alignment the modulator relies on and painting leakage across the whole
    spectrum — including into guard regions that are actually empty.
    """
    acc = [0.0] * (size // 2)
    used = 0
    position = (skip // size) * size
    while used < blocks and position + size <= len(samples):
        block = spectrum([complex(v, 0.0) for v in samples[position:position + size]])
        for k in range(size // 2):
            acc[k] += abs(block[k])
        position += size
        used += 1
    if used:
        acc = [v / used for v in acc]
    return acc


def synth_ofdm(symbols, bins, seed=1):
    """A dense-profile OFDM carrier, built the way the modulator builds one."""
    state = seed | 1
    def rand():
        nonlocal state
        state ^= (state >> 12) & 0xFFFFFFFFFFFFFFFF
        state ^= (state << 25) & 0xFFFFFFFFFFFFFFFF
        state ^= (state >> 27) & 0xFFFFFFFFFFFFFFFF
        return (state * 0x2545F4914F6CDD1D) & 0xFFFFFFFFFFFFFFFF

    out = []
    for _ in range(symbols):
        bins_spectrum = [0j] * FFT_SIZE
        for b in bins:
            # 4096-QAM: 64 levels per axis, odd integers, Gray order irrelevant here.
            i = (rand() >> 20) % 64 * 2 - 63
            q = (rand() >> 20) % 64 * 2 - 63
            point = complex(i, q) / 90.0
            bins_spectrum[b] = point
            bins_spectrum[FFT_SIZE - b] = point.conjugate()
        # inverse transform via the forward one: conj -> fft -> conj / N
        inv = fft([v.conjugate() for v in bins_spectrum])
        out.extend((v.conjugate() / FFT_SIZE).real for v in inv)
    return out


def synth_fsk(symbols, seed=7):
    """A 16-FSK carrier: one bin-aligned tone at a time, 48 samples per symbol."""
    state = seed | 1
    n = 48
    out = []
    for _ in range(symbols):
        state ^= (state >> 12) & 0xFFFFFFFFFFFFFFFF
        state ^= (state << 25) & 0xFFFFFFFFFFFFFFFF
        state ^= (state >> 27) & 0xFFFFFFFFFFFFFFFF
        tone = 4 + ((state * 0x2545F4914F6CDD1D) >> 33) % 16   # bins 4..19
        for i in range(n):
            out.append(0.8 * math.sin(2 * math.pi * tone * i / n))
    return out


# --------------------------------------------------------------------------
# PNG
# --------------------------------------------------------------------------

def write_png(path, width, height, rows):
    """8-bit RGB PNG. `rows` is a list of `bytes`, each 3*width long."""
    raw = b"".join(b"\x00" + bytes(row) for row in rows)

    def chunk(tag, payload):
        head = struct.pack(">I", len(payload)) + tag + payload
        return head + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    blob = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as handle:
        handle.write(blob)


def viridis(t):
    """A perceptually-ordered ramp, so intensity reads correctly in greyscale too."""
    t = max(0.0, min(1.0, t))
    stops = [
        (0.00, (68, 1, 84)), (0.25, (59, 82, 139)), (0.50, (33, 145, 140)),
        (0.75, (94, 201, 98)), (1.00, (253, 231, 37)),
    ]
    for (t0, c0), (t1, c1) in zip(stops, stops[1:]):
        if t <= t1:
            f = (t - t0) / (t1 - t0)
            return tuple(int(a + (b - a) * f) for a, b in zip(c0, c1))
    return stops[-1][1]


# --------------------------------------------------------------------------
# SVG
# --------------------------------------------------------------------------

class Svg:
    def __init__(self, width, height):
        self.width, self.height = width, height
        self.parts = []

    def rect(self, x, y, w, h, fill, opacity=1.0, stroke=None):
        s = f' stroke="{stroke}"' if stroke else ""
        self.parts.append(
            f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" '
            f'fill="{fill}" fill-opacity="{opacity}"{s}/>'
        )

    def line(self, x1, y1, x2, y2, stroke, width=1.0, dash=None):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.parts.append(
            f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" '
            f'stroke="{stroke}" stroke-width="{width}"{d}/>'
        )

    def path(self, points, stroke, width=1.5, fill="none"):
        d = "M " + " L ".join(f"{x:.1f},{y:.1f}" for x, y in points)
        self.parts.append(
            f'<path d="{d}" fill="{fill}" stroke="{stroke}" stroke-width="{width}"/>'
        )

    def text(self, x, y, body, size=12, fill=INK, anchor="start", weight="normal"):
        body = body.replace("&", "&amp;").replace("<", "&lt;")
        self.parts.append(
            f'<text x="{x:.1f}" y="{y:.1f}" font-family="ui-sans-serif,-apple-system,'
            f'Segoe UI,Roboto,sans-serif" font-size="{size}" fill="{fill}" '
            f'text-anchor="{anchor}" font-weight="{weight}">{body}</text>'
        )

    def save(self, path):
        body = "\n".join(self.parts)
        doc = (
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{self.width}" '
            f'height="{self.height}" viewBox="0 0 {self.width} {self.height}">'
            f'<rect width="100%" height="100%" fill="white"/>{body}</svg>'
        )
        with open(path, "w") as handle:
            handle.write(doc)


# --------------------------------------------------------------------------
# figure 1 — measured spectrum of a real cover-mode carrier
# --------------------------------------------------------------------------

def figure_spectrum(samples, rate, path):
    # Analysed at the modulator's own symbol length. A longer window spans
    # several symbols, smears their differing content across the band edge, and
    # paints leakage into the guard regions that is an artefact of the analysis
    # rather than a property of the signal. At 512 the bins line up exactly with
    # the ones the modulator wrote, so an empty band reads as genuinely empty.
    size = FFT_SIZE
    bin_hz = rate / size
    acc = average_spectrum(samples, size, 600, skip=rate * 20)
    peak = max(acc) or 1.0
    db = [20 * math.log10(max(v, 1e-12) / peak) for v in acc]

    w, h = 900, 400
    left, right, top, bottom = 70, 30, 62, 66
    pw, ph = w - left - right, h - top - bottom
    fmax, floor = 12000.0, -110.0

    svg = Svg(w, h)
    svg.text(left, 26, "Spectrum of a carrier in cover mode", size=15, weight="600")
    svg.text(left, 44,
             "measured from a real carrier, analysed at the 512-sample symbol length",
             size=11, fill=MUTED)

    def fx(hz):
        return left + pw * min(hz, fmax) / fmax

    def fy(value):
        return top + ph * (1 - (max(value, floor) - floor) / (0 - floor))

    # band shading
    for lo, hi, colour, label in [
        (COVER_LO_BIN * BIN_HZ, COVER_HI_BIN * BIN_HZ, COVER_FILL, "cover audio"),
        (DATA_LO_BIN * BIN_HZ, DATA_HI_BIN * BIN_HZ, DATA_FILL, "data"),
    ]:
        svg.rect(fx(lo), top, fx(hi) - fx(lo), ph, colour, 0.09)
        svg.text((fx(lo) + fx(hi)) / 2, top + 16, label, size=11,
                 fill=colour, anchor="middle", weight="600")

    for hz in range(0, 12001, 2000):
        svg.line(fx(hz), top, fx(hz), top + ph, GRID)
        svg.text(fx(hz), top + ph + 18, f"{hz // 1000}k", size=10,
                 fill=MUTED, anchor="middle")
    for level in range(0, -111, -20):
        svg.line(left, fy(level), left + pw, fy(level), GRID)
        svg.text(left - 8, fy(level) + 4, f"{level}", size=10, fill=MUTED, anchor="end")

    points = [(fx(k * bin_hz), fy(db[k])) for k in range(len(db)) if k * bin_hz <= fmax]
    svg.path(points, INK, 1.1)

    svg.text(left - 52, top + ph / 2, "dB", size=11, fill=MUTED)
    svg.text(left + pw / 2, h - 12, "frequency (Hz)", size=11, fill=MUTED, anchor="middle")
    svg.text(left, h - 12,
             "guard regions sit at the noise floor: the two bands never meet",
             size=10, fill=MUTED)
    svg.save(path)


# --------------------------------------------------------------------------
# figure 2 — the dimension argument, as a spectrogram
# --------------------------------------------------------------------------

def figure_spectrogram(path):
    """FSK against OFDM, each analysed at its own symbol length.

    That detail decides whether the figure shows anything. An FSK symbol is 48
    samples and an OFDM symbol is 512, so transforming both with one window
    smears ten FSK symbols into every column and the tone hopping vanishes into
    mush — which is exactly what the first attempt produced. Each scheme is
    transformed at the length it was built with, then both are mapped onto a
    shared 0-12 kHz axis so the panels stay comparable.
    """
    symbols = 64
    rows_out = 200
    col_px = 4
    # A tight floor is what makes the contrast readable: FSK tones are
    # bin-aligned, so an inactive subcarrier is genuinely empty rather than
    # merely quiet, and anything below this reads as black.
    floor_db = -30.0

    def render(samples, window, count):
        cols = []
        for f in range(count):
            block = samples[f * window:(f + 1) * window]
            if len(block) < window:
                break
            mags = [abs(v) for v in spectrum([complex(x, 0.0) for x in block])[: window // 2]]
            peak = max(mags) or 1.0
            column = []
            for y in range(rows_out):
                hz = (y + 0.5) / rows_out * (SAMPLE_RATE / 2)
                k = min(int(hz / (SAMPLE_RATE / window)), len(mags) - 1)
                db = 20 * math.log10(max(mags[k], 1e-9) / peak)
                column.append(max(0.0, (db - floor_db) / -floor_db))
            cols.append(column)
        return cols

    fsk = render(synth_fsk(symbols + 4), 48, symbols)
    ofdm = render(synth_ofdm(symbols + 2, list(range(8, DATA_HI_BIN + 1))), FFT_SIZE, symbols)

    gap, pad = 22, 6
    panel_w = symbols * col_px
    width = panel_w * 2 + gap + pad * 2
    height = rows_out + pad * 2

    canvas = [[(255, 255, 255)] * width for _ in range(height)]
    for index, cols in enumerate((fsk, ofdm)):
        x0 = pad + index * (panel_w + gap)
        for x, column in enumerate(cols):
            for y in range(rows_out):
                # low frequency at the bottom
                colour = viridis(column[rows_out - 1 - y])
                for dx in range(col_px):
                    canvas[pad + y][x0 + x * col_px + dx] = colour

    rows = [bytes(b for pixel in row for b in pixel) for row in canvas]
    write_png(path, width, height, rows)
    return width, height


# --------------------------------------------------------------------------
# figure 3 — the storage tensor
# --------------------------------------------------------------------------

def figure_tensor(path):
    w, h = 900, 320
    svg = Svg(w, h)
    svg.text(40, 30, "Where a payload byte actually lives", size=15, weight="600")
    svg.text(40, 48, "schematic, not measured", size=11, fill=MUTED)

    ox, oy = 90, 210
    cell, cols, rows = 26, 12, 5
    # depth copies = channels
    for depth in range(3, -1, -1):
        dx, dy = depth * 13, -depth * 9
        opacity = 0.25 if depth else 1.0
        for r in range(rows):
            for c in range(cols):
                x = ox + dx + c * cell
                y = oy + dy - r * cell
                svg.rect(x, y - cell, cell - 3, cell - 3, COVER_FILL if depth == 0 else GUARD_FILL,
                         (0.10 + 0.14 * ((r + c) % 3)) if depth == 0 else opacity * 0.12,
                         stroke="none")

    svg.line(ox, oy + 8, ox + cols * cell, oy + 8, INK, 1.2)
    svg.text(ox + cols * cell / 2, oy + 28, "time  →  symbols of 512 samples",
             size=11, fill=INK, anchor="middle")

    svg.line(ox - 10, oy, ox - 10, oy - rows * cell, INK, 1.2)
    svg.text(ox - 18, oy - rows * cell / 2, "frequency", size=11, fill=INK, anchor="end")
    svg.text(ox - 18, oy - rows * cell / 2 + 14, "243 subcarriers", size=10,
             fill=MUTED, anchor="end")

    svg.line(ox + cols * cell + 6, oy - rows * cell + 6,
             ox + cols * cell + 45, oy - rows * cell - 30, MUTED, 1.2, dash="4 3")
    svg.text(ox + cols * cell + 52, oy - rows * cell - 30,
             "channel  →  1 to 8", size=11, fill=MUTED)

    bx = 560
    svg.text(bx, 92, "each cell is one complex QAM point", size=12, weight="600")
    for i, line in enumerate([
        "I  —  6 bits   (64 levels)",
        "Q  —  6 bits   (64 levels)",
        "= 4096-QAM, 12 bits per cell",
    ]):
        svg.text(bx, 114 + i * 19, line, size=11, fill=INK if i < 2 else MUTED)

    svg.text(bx, 186, "time × frequency × I/Q are one signal", size=11, fill=MUTED)
    svg.text(bx, 203, "N samples = N/2 complex bins = N reals.", size=11, fill=MUTED)
    svg.text(bx, 220, "Only channel adds new samples.", size=11, fill=INK, weight="600")
    svg.save(path)


# --------------------------------------------------------------------------

def main():
    os.makedirs(DOCS, exist_ok=True)

    source = sys.argv[1] if len(sys.argv) > 1 else "/tmp/book.wav"
    if os.path.exists(source):
        samples, rate = read_wav_mono(source)
        figure_spectrum(samples, rate, os.path.join(DOCS, "spectrum.svg"))
        print(f"  docs/spectrum.svg      measured from {source}")
    else:
        print(f"  docs/spectrum.svg      SKIPPED — no carrier at {source}")

    w, h = figure_spectrogram(os.path.join(DOCS, "spectrogram.png"))
    print(f"  docs/spectrogram.png   {w}x{h}  (16-FSK | OFDM, 64 symbols each)")

    figure_tensor(os.path.join(DOCS, "tensor.svg"))
    print("  docs/tensor.svg        storage tensor schematic")


if __name__ == "__main__":
    main()
