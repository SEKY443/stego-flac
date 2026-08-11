#!/usr/bin/env python3
"""Plot carrier size against payload size for each profile.

    python3 tools/size_curve.py

Draws two figures:

  docs/size-curve.svg        0 to 500 MB, with the recommended profile marked
  docs/size-curve-small.svg  0 to 50 MB, where the ratio is not constant

The second exists because the first hides something. Over 0-500 MB the lines
look straight, and they nearly are -- carrier *duration* is exactly
proportional to payload, at 70.00 s/MiB for `dense`, measured identical at
1 MiB and 20 MiB, because the bit rate is fixed by the plan. But the FLAC size
is duration times how many bytes per second FLAC spends, and that second factor
moves with length:

    payload    1 MiB      5 MiB     20 MiB
    KiB/s      37.6       38.5      40.1     of audio

so the expansion ratio has a genuine minimum near 1 MiB (2.56x) and climbs to
~2.74x by 20 MiB, where it flattens. Below ~500 KB it climbs the other way, and
steeply -- 4.51x at 1 KB -- because the header, the 5% FEC and the container
are fixed costs amortised over less payload.

The cause of the middle trend is measurable: normalisation pins the peak at 0.9
full scale, and the peak is an isolated transient in the last few percent of the
carrier. In a short carrier that transient stands 18.4x above the body's RMS;
in a long one, 9.2x. So the short file's body is normalised ~6 dB quieter and
codes cheaper. Draw-to-draw scatter at fixed length is only +/-0.3%, so this is
a real function of size, not noise -- which is why the small-range figure plots
measured points joined up rather than a fitted ratio.

Sizes are chosen per profile so nothing absurd is written to disk: 1 MiB
through the 16-FSK plan already produces a 180 MB file.
"""

import array
import math
import os
import subprocess
import sys
import tempfile
import wave

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from figures import Svg, INK, MUTED, GRID  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "stego-flac")
DOCS = os.path.join(ROOT, "docs")

MIB = 1024 * 1024
TARGET = 500_000_000          # the "keep it under 500 MB" line
LIMIT = 256 * MIB             # MAX_PAYLOAD_LEN, the format's own ceiling

# label, colour, extra flags, payload sizes to measure (MiB).
# `@cover` expands to the synthetic cover file.
#
# The cover row stays small on purpose: its carrier has to stay shorter than
# the synthetic cover below, otherwise most of the file would be uncovered and
# the line would really be measuring plain OFDM.
#
# It also pins --cover-quality rather than leaving it on auto. Auto widens the
# band for small payloads, and every size this row can afford to measure falls
# in the widest tier -- so an auto line fitted here and extended to 500 MB would
# be claiming a 7 kHz cover at a size where auto would actually have chosen
# 3.4 kHz. The telephone band is what large payloads get, so that is what this
# line measures. The small-range figure shows the tiers properly.
PROFILES = [
    ("16-FSK (standard)", "#b45309", ["--profile", "standard"], [0.25, 0.5, 1]),
    ("4-FSK (fast)", "#a16207", ["--profile", "fast"], [0.5, 1, 2]),
    ("OFDM + cover (3.4 kHz)", "#7c3aed",
     ["@cover", "--cover-quality", "telephone"], [1, 2, 4]),
    ("dense (default)", "#dc2626", ["--profile", "dense"], [1, 4, 16, 48, 96, 150]),
    ("compact", "#2563eb", ["--profile", "compact"], [1, 4, 16, 48, 96, 150]),
    ("--qam-bits 20", "#059669", ["--qam-bits", "20"], [1, 4, 16, 48, 96, 150]),
]

# Above this the line leaves the linear panel almost immediately and the shaded
# wedge collapses to a sliver at the origin. Those plans live on the log panel.
LINEAR_MAX_RATIO = 10

COVER_SECONDS = 330     # longer than the 4 MiB cover-mode carrier

# ---- the small-range figure ----------------------------------------------
#
# Log-spaced from 4 KB, because the interesting part is the knee and the knee is
# all below 1 MB. Above 50 MB the ratio is flat and the big figure covers it.
SMALL_SIZES = [1e3, 3e3, 10e3, 30e3, 100e3, 300e3, 1e6, 2e6, 4e6, 8e6,
               16e6, 32e6, 50e6]

# Cover mode is measured over the same range, so the cover has to outlast the
# longest of those carriers: 50 MB at roughly 96 s/MiB is about 76 minutes.
SMALL_COVER_SECONDS = 80 * 60

SMALL_PROFILES = [
    ("OFDM + cover (auto quality)", "#7c3aed", ["@cover"]),
    ("dense (default)", "#dc2626", ["--profile", "dense"]),
    ("compact", "#2563eb", ["--profile", "compact"]),
    ("--qam-bits 20", "#059669", ["--qam-bits", "20"]),
]

# Where `--cover-quality auto` changes tier, in *frame* bytes. Frame is about
# 1.07x payload for incompressible input, which is what is measured here.
FRAME_OVERHEAD = 1.07
COVER_TIERS = [(4 * MIB, "7 kHz cover"), (32 * MIB, "5 kHz cover"), (None, "3.4 kHz")]


def cover_wav(path, seconds=COVER_SECONDS, rate=24_000):
    """A plain tone bed, so the script needs no external audio.

    Built as one whole-numbered-period block repeated to length: an 80-minute
    bed is 115 million samples, and generating those one at a time in Python
    takes longer than every encode in this script put together. Choosing
    frequencies that close over the block keeps the repeat free of clicks.

    The noise term matters. Cover audio sits 25 dB *above* the data, so it is
    what FLAC mostly sees, and pure tones are so predictable that an all-tone
    bed would report cover-mode files far smaller than any real recording
    produces. Tones plus noise brackets real audio from the incompressible side.
    """
    import random

    rng = random.Random(20260810)
    block_seconds = 10
    n = rate * block_seconds
    block = array.array("h", bytes(2 * n))
    for i in range(n):
        t = i / rate
        block[i] = int(0.5 * 32767 * (
            0.6 * math.sin(2 * math.pi * 320 * t)
            + 0.3 * math.sin(2 * math.pi * 900 * t)
            + 0.1 * rng.uniform(-1.0, 1.0)))
    samples = block * max(1, seconds // block_seconds)
    with wave.open(path, "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(rate)
        handle.writeframes(samples.tobytes())


class Series:
    def __init__(self, label, colour, points, ratio, best, worst):
        self.label, self.colour, self.points = label, colour, points
        self.ratio, self.best, self.worst = ratio, best, worst


def measure(work, flags, mib, cover):
    return measure_bytes(work, flags, int(mib * MIB), cover)


def measure_bytes(work, flags, size, cover):
    payload = os.path.join(work, "p.bin")
    with open("/dev/urandom", "rb") as source, open(payload, "wb") as sink:
        sink.write(source.read(int(size)))
    carrier = os.path.join(work, "c.flac")

    args = [BIN, "encode", payload, "-o", carrier, "--no-encrypt", "--force"]
    for flag in flags:
        args += ["--cover", cover] if flag == "@cover" else [flag]
    subprocess.run(args, capture_output=True, check=True)
    return os.path.getsize(payload), os.path.getsize(carrier)


def main():
    if not os.path.exists(BIN):
        sys.exit("build first: cargo build --release")
    os.makedirs(DOCS, exist_ok=True)

    work = tempfile.mkdtemp(prefix="stego-curve-")
    cover = os.path.join(work, "cover.wav")
    cover_wav(cover)

    series = []
    try:
        for label, colour, flags, sizes in PROFILES:
            points = [measure(work, flags, mib, cover) for mib in sizes]
            each = [out / inp for inp, out in points]
            # Fit through the origin: the relationship is proportional, and the
            # fixed header is a couple of kilobytes against megabytes.
            ratio = sum(o for _, o in points) / sum(i for i, _ in points)
            series.append(Series(label, colour, points, ratio,
                                 min(each), max(each)))
            print(f"  {label:<20} {ratio:.3f}x  "
                  f"(spread {min(each):.3f}-{max(each):.3f})", file=sys.stderr)
        # The small-range sweep needs a much longer bed, so it is written only
        # once the wide sweep is done with the short one.
        cover_wav(cover, SMALL_COVER_SECONDS)
        small = []
        for label, colour, flags in SMALL_PROFILES:
            points = [measure_bytes(work, flags, size, cover)
                      for size in SMALL_SIZES]
            small.append(Series(label, colour, points, 0, 0, 0))
            worst = max(o / i for i, o in points)
            best = min(o / i for i, o in points)
            print(f"  {label:<28} {best:.2f}x at best, {worst:.2f}x at worst",
                  file=sys.stderr)
    finally:
        import shutil
        shutil.rmtree(work, ignore_errors=True)

    bands = recommendations(series)
    draw(series, bands, os.path.join(DOCS, "size-curve.svg"))
    draw_small(small, os.path.join(DOCS, "size-curve-small.svg"))
    print("  docs/size-curve.svg")
    print("  docs/size-curve-small.svg")

    print()
    print("| plan | carrier per payload byte | max payload under 500 MB |")
    print("|---|---|---|")
    for s in series:
        # Worst observed ratio, so the stated ceiling holds on a bad draw.
        fits = TARGET / s.worst
        note = f"{fits / 1e6:.0f} MB"
        if fits > LIMIT:
            note = f"{LIMIT / 1e6:.0f} MB (payload cap, not size)"
        print(f"| {s.label} | {s.ratio:.2f}x (worst {s.worst:.2f}x) | {note} |")

    print()
    print("| payload | recommended |")
    print("|---|---|")
    for low, high, label, _ in bands:
        span = (f"up to {high / 1e6:.0f} MB" if low == 0 else
                f"over {low / 1e6:.0f} MB" if high >= LIMIT else
                f"{low / 1e6:.0f}-{high / 1e6:.0f} MB")
        print(f"| {span} | {label} |")


def recommendations(series, budget=TARGET):
    """Cheapest-margin plan that still fits `budget`, as payload-size bands.

    Ordered from most to least margin, each plan is kept until its own worst
    measured ratio would break the budget, and then the next one takes over.
    The point is to spend the *least* aggressive constellation that works: a
    denser one is measurably more fragile to any later resampling or gain
    change, so it should only be reached for when size forces it.
    """
    order = ["dense (default)", "compact", "--qam-bits 20"]
    by_label = {s.label: s for s in series}

    bands, low = [], 0
    for label in order:
        s = by_label.get(label)
        if s is None:
            continue
        high = min(budget / s.worst, LIMIT)
        if high <= low:
            continue
        bands.append((low, high, label, s.colour))
        low = high
        if high >= LIMIT:
            break
    if low < LIMIT:
        bands.append((low, LIMIT, order[-1], by_label[order[-1]].colour))
    return bands


def draw(series, bands, path):
    # Taller than the panels need: the recommendation strip lives between the
    # tick labels and the axis title.
    w, h = 980, 458
    pad_l, pad_r, pad_t, pad_b = 74, 20, 66, 86
    gap = 78
    panel_w = (w - pad_l - pad_r - gap) / 2
    panel_h = h - pad_t - pad_b

    svg = Svg(w, h)
    svg.text(pad_l, 26, "Carrier size against payload size", size=15, weight="600")
    svg.text(pad_l, 44,
             "dots are measured encodes; lines extend the fitted ratio, "
             "shaded to the best and worst ratio seen",
             size=11, fill=MUTED)

    x_max = 500e6

    # ---- left panel: linear, the range anyone actually uses ----------------
    ox = pad_l
    y_max = 1500e6

    def lx(v):
        return ox + panel_w * min(v, x_max) / x_max

    def ly(v):
        return pad_t + panel_h * (1 - min(v, y_max) / y_max)

    svg.text(ox, pad_t - 12, "linear, OFDM plans only", size=11, fill=INK,
             weight="600")

    # Recommendation strip along the bottom: which plan to reach for at a given
    # payload size. Drawn under the grid so the curves stay readable.
    strip_y = pad_t + panel_h + 26
    for low, high, label, colour in bands:
        svg.rect(lx(low), strip_y, lx(high) - lx(low), 13, colour, opacity=0.16)
        if lx(high) - lx(low) > 46:
            svg.text((lx(low) + lx(high)) / 2, strip_y + 10, label.split(" ")[0],
                     size=8.5, fill=colour, anchor="middle", weight="600")
    # Past the payload cap nothing is recommended, because nothing is possible.
    svg.rect(lx(LIMIT), strip_y, lx(x_max) - lx(LIMIT), 13, MUTED, opacity=0.10)
    svg.text(lx(LIMIT) + 6, strip_y + 10, "over the 256 MiB payload cap",
             size=8.5, fill=MUTED)
    svg.text(ox - 8, strip_y + 10, "use", size=9, fill=MUTED, anchor="end")

    for gv in range(0, 1501, 250):
        svg.line(ox, ly(gv * 1e6), ox + panel_w, ly(gv * 1e6), GRID)
        svg.text(ox - 8, ly(gv * 1e6) + 4, f"{gv}", size=9, fill=MUTED, anchor="end")
    for gv in range(0, 501, 100):
        svg.line(lx(gv * 1e6), pad_t, lx(gv * 1e6), pad_t + panel_h, GRID)
        svg.text(lx(gv * 1e6), pad_t + panel_h + 16, f"{gv}", size=9,
                 fill=MUTED, anchor="middle")

    # the 500 MB output budget
    svg.line(ox, ly(TARGET), ox + panel_w, ly(TARGET), "#111827", 1.2, dash="5 3")
    svg.text(ox + panel_w - 4, ly(TARGET) - 6, "500 MB output budget", size=10,
             fill="#111827", anchor="end")

    marked = []
    for s in series:
        if s.ratio > LINEAR_MAX_RATIO:
            continue
        # The spread wedge: same origin, fanning out to best and worst ratio.
        wedge = [(lx(0), ly(0)),
                 (lx(x_max), ly(x_max * s.best)),
                 (lx(x_max), ly(x_max * s.worst))]
        d = "M " + " L ".join(f"{x:.1f},{y:.1f}" for x, y in wedge) + " Z"
        svg.parts.append(
            f'<path d="{d}" fill="{s.colour}" fill-opacity="0.18" stroke="none"/>'
        )
        svg.path([(lx(0), ly(0)), (lx(x_max), ly(x_max * s.ratio))], s.colour, 1.8)
        for inp, out in s.points:
            if out <= y_max and inp <= x_max:
                svg.parts.append(
                    f'<circle cx="{lx(inp):.1f}" cy="{ly(out):.1f}" r="2.6" '
                    f'fill="{s.colour}"/>'
                )
        # Largest payload that still fits the budget on a bad draw.
        crossing = TARGET / s.worst
        if crossing <= x_max:
            svg.parts.append(
                f'<circle cx="{lx(crossing):.1f}" cy="{ly(TARGET):.1f}" r="3.6" '
                f'fill="white" stroke="{s.colour}" stroke-width="1.8"/>'
            )
            # Neighbouring profiles cross within a few tens of MB of each
            # other, so alternate rows keep the numbers apart.
            drop = 15 if len(marked) % 2 == 0 else 27
            svg.text(lx(crossing), ly(TARGET) + drop, f"{crossing / 1e6:.0f}",
                     size=9, fill=s.colour, anchor="middle", weight="600")
            marked.append(crossing)

    svg.text(ox + panel_w / 2, h - 16, "payload (MB)", size=11, fill=MUTED,
             anchor="middle")
    # Rotated, otherwise it sits on top of the tick labels.
    svg.parts.append(
        f'<text transform="translate({ox - 52:.1f},{pad_t + panel_h / 2:.1f}) '
        f'rotate(-90)" font-family="ui-sans-serif,-apple-system,Segoe UI,Roboto,'
        f'sans-serif" font-size="11" fill="{MUTED}" text-anchor="middle">'
        f'carrier (MB)</text>'
    )

    # One legend for both panels, in the empty corner above the OFDM lines.
    ly0 = pad_t + 14
    for row, s in enumerate(series):
        y = ly0 + row * 15
        # Dashed swatch = drawn on the log panel only.
        dash = "3 2" if s.ratio > LINEAR_MAX_RATIO else None
        svg.line(ox + 12, y - 4, ox + 30, y - 4, s.colour, 2.4, dash=dash)
        svg.text(ox + 36, y, f"{s.label}  {s.ratio:.2f}x", size=10, fill=INK)

    # ---- right panel: log, so the FSK plans fit on the page ----------------
    ox2 = pad_l + panel_w + gap
    lo, hi = 1e6, 1e11

    def rx(v):
        return ox2 + panel_w * min(v, x_max) / x_max

    def ry(v):
        v = max(v, lo)
        return pad_t + panel_h * (1 - (math.log10(v) - math.log10(lo))
                                  / (math.log10(hi) - math.log10(lo)))

    svg.text(ox2, pad_t - 12, "log scale, including the FSK plans",
             size=11, fill=INK, weight="600")
    for exp in range(6, 12):
        svg.line(ox2, ry(10 ** exp), ox2 + panel_w, ry(10 ** exp), GRID)
        tag = {6: "1 MB", 7: "10 MB", 8: "100 MB", 9: "1 GB", 10: "10 GB",
               11: "100 GB"}[exp]
        svg.text(ox2 - 8, ry(10 ** exp) + 4, tag, size=9, fill=MUTED, anchor="end")
    for gv in range(0, 501, 100):
        svg.line(rx(gv * 1e6), pad_t, rx(gv * 1e6), pad_t + panel_h, GRID)
        svg.text(rx(gv * 1e6), pad_t + panel_h + 16, f"{gv}", size=9,
                 fill=MUTED, anchor="middle")

    svg.line(ox2, ry(TARGET), ox2 + panel_w, ry(TARGET), "#111827", 1.2, dash="5 3")

    # A proportional line bends on a log axis, so it needs enough points to
    # stop looking like a polygon -- densest near the origin where it turns.
    samples = [x_max * (i / 240) ** 2.2 for i in range(1, 241)]
    for s in series:
        svg.path([(rx(v), ry(v * s.ratio)) for v in samples], s.colour, 1.8)

    svg.text(ox2 + panel_w / 2, h - 16, "payload (MB)", size=11, fill=MUTED,
             anchor="middle")

    # the format's own ceiling
    svg.line(rx(LIMIT), pad_t, rx(LIMIT), pad_t + panel_h, MUTED, 1.0, dash="2 3")
    svg.text(rx(LIMIT) - 4, pad_t + 12, "256 MiB payload cap", size=9,
             fill=MUTED, anchor="end")

    svg.save(path)


def draw_small(series, path):
    """The 0-50 MB view, where the ratio is a curve rather than a constant."""
    w, h = 980, 440
    pad_l, pad_r, pad_t, pad_b = 74, 22, 78, 58
    gap = 86
    panel_w = (w - pad_l - pad_r - gap) / 2
    panel_h = h - pad_t - pad_b

    svg = Svg(w, h)
    svg.text(pad_l, 26, "The first 50 MB, where the ratio is not constant",
             size=15, weight="600")
    svg.text(pad_l, 44,
             "every point is a measured encode -- these are joined, not fitted",
             size=11, fill=MUTED)

    x_max = 50e6

    # ---- left panel: size, linear -----------------------------------------
    ox = pad_l
    y_max = 180e6

    def lx(v):
        return ox + panel_w * min(v, x_max) / x_max

    def ly(v):
        return pad_t + panel_h * (1 - min(v, y_max) / y_max)

    svg.text(ox, pad_t - 14, "carrier size", size=11, fill=INK, weight="600")
    for gv in range(0, 181, 30):
        svg.line(ox, ly(gv * 1e6), ox + panel_w, ly(gv * 1e6), GRID)
        svg.text(ox - 8, ly(gv * 1e6) + 4, f"{gv}", size=9, fill=MUTED,
                 anchor="end")
    for gv in range(0, 51, 10):
        svg.line(lx(gv * 1e6), pad_t, lx(gv * 1e6), pad_t + panel_h, GRID)
        svg.text(lx(gv * 1e6), pad_t + panel_h + 16, f"{gv}", size=9,
                 fill=MUTED, anchor="middle")

    for s in series:
        svg.path([(lx(i), ly(o)) for i, o in s.points if i <= x_max],
                 s.colour, 1.8)
        for i, o in s.points:
            if i <= x_max and o <= y_max:
                svg.parts.append(
                    f'<circle cx="{lx(i):.1f}" cy="{ly(o):.1f}" r="2.4" '
                    f'fill="{s.colour}"/>'
                )

    svg.text(ox + panel_w / 2, h - 16, "payload (MB)", size=11, fill=MUTED,
             anchor="middle")
    svg.parts.append(
        f'<text transform="translate({ox - 52:.1f},{pad_t + panel_h / 2:.1f}) '
        f'rotate(-90)" font-family="ui-sans-serif,-apple-system,Segoe UI,Roboto,'
        f'sans-serif" font-size="11" fill="{MUTED}" text-anchor="middle">'
        f'carrier (MB)</text>'
    )

    # ---- right panel: the ratio itself, on a log payload axis --------------
    #
    # This is the panel that earns the figure. On the left the lines look
    # straight through the origin; here the knee below 1 MB is unmissable.
    ox2 = pad_l + panel_w + gap
    lo_x, hi_x = 1e3, 50e6
    r_max = 10.0

    def rx(v):
        v = min(max(v, lo_x), hi_x)
        return ox2 + panel_w * (math.log10(v) - math.log10(lo_x)) \
            / (math.log10(hi_x) - math.log10(lo_x))

    def ry(v):
        return pad_t + panel_h * (1 - min(v, r_max) / r_max)

    svg.text(ox2, pad_t - 14, "expansion ratio, log payload axis",
             size=11, fill=INK, weight="600")
    for gv in range(0, 11):
        svg.line(ox2, ry(gv), ox2 + panel_w, ry(gv), GRID)
        svg.text(ox2 - 8, ry(gv) + 4, f"{gv}x", size=9, fill=MUTED, anchor="end")
    for decade, tag in ((1e3, "1 KB"), (1e4, "10 KB"), (1e5, "100 KB"),
                        (1e6, "1 MB"), (1e7, "10 MB")):
        svg.line(rx(decade), pad_t, rx(decade), pad_t + panel_h, GRID)
        svg.text(rx(decade), pad_t + panel_h + 16, tag, size=9, fill=MUTED,
                 anchor="middle")

    # Where --cover-quality auto changes tier.
    for frame_bytes, tag in COVER_TIERS:
        if frame_bytes is None or frame_bytes / FRAME_OVERHEAD > hi_x:
            continue
        at = frame_bytes / FRAME_OVERHEAD
        svg.line(rx(at), pad_t, rx(at), pad_t + panel_h, "#7c3aed", 1.0,
                 dash="2 3")
        svg.text(rx(at) - 4, pad_t + 12, tag, size=8.5, fill="#7c3aed",
                 anchor="end")

    for s in series:
        svg.path([(rx(i), ry(o / i)) for i, o in s.points], s.colour, 1.8)
        for i, o in s.points:
            svg.parts.append(
                f'<circle cx="{rx(i):.1f}" cy="{ry(o / i):.1f}" r="2.4" '
                f'fill="{s.colour}"/>'
            )

    svg.text(ox2 + panel_w / 2, h - 16, "payload (log scale)", size=11,
             fill=MUTED, anchor="middle")

    # Legend sits above every curve except the cover's first two points, and to
    # the right of those, so nothing overlaps at any size.
    legend_x = rx(3e4)
    for row, s in enumerate(series):
        y = ry(8.8) + row * 15
        svg.line(legend_x, y - 4, legend_x + 18, y - 4, s.colour, 2.4)
        svg.text(legend_x + 24, y, s.label, size=9.5, fill=INK)

    svg.save(path)


if __name__ == "__main__":
    main()
