#!/usr/bin/env python3
# Copyright 2026 Jakub Hlavnicka
#
# Draws the ReSynth Studio app icon: a waveform resolved into its harmonic
# spectrum, which is what the app does to a recording. Writes the SVG for two
# layouts — macOS (Apple's icon grid: an 824 px body on a 1024 canvas) and
# full (the body nearly fills the canvas, which is what Windows and Linux
# expect) — then the PNG sizes, the Windows .ico and the macOS .icns.
#
#   python3 packaging/icon/make_icon.py        (needs rsvg-convert + Pillow)
import math
import subprocess
import sys
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
OUT = HERE / "build"


def wave_path(x0, x1, y_mid, amp, steps=240):
    """Two periods of a tone with three harmonics: the sound the bars below add up to."""
    pts = []
    for i in range(steps + 1):
        t = i / steps
        ph = t * 4.0 * math.pi
        y = math.sin(ph) + math.sin(2 * ph) / 2.5 + math.sin(3 * ph) / 4.0
        pts.append((x0 + (x1 - x0) * t, y_mid - amp * y / 1.25))
    d = f"M{pts[0][0]:.1f},{pts[0][1]:.1f}"
    d += "".join(f" L{x:.1f},{y:.1f}" for x, y in pts[1:])
    return d


def svg(body_x, body_size, radius):
    s = body_size / 824.0          # every measure below is on the 824 grid
    X = lambda v: body_x + v * s   # noqa: E731
    bars = []
    n_bars = 6
    left, right, base = 132, 692, 708
    pitch = (right - left) / n_bars
    width = pitch * 0.6
    for n in range(1, n_bars + 1):
        h = 300 / n ** 0.8
        x = left + (n - 1) * pitch + (pitch - width) / 2
        bars.append(
            f'<rect x="{X(x):.1f}" y="{X(base - h):.1f}" width="{width * s:.1f}" '
            f'height="{h * s:.1f}" rx="{width * s / 2:.1f}" fill="url(#bar)" '
            f'opacity="{1.0 - (n - 1) * 0.08:.2f}"/>')
    wave = wave_path(X(112), X(712), X(270), 135 * s)
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#1d3170"/>
      <stop offset="1" stop-color="#090d20"/>
    </linearGradient>
    <radialGradient id="glow" cx="0.5" cy="0.42" r="0.6">
      <stop offset="0" stop-color="#3f6fd8" stop-opacity="0.55"/>
      <stop offset="1" stop-color="#3f6fd8" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="bar" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#64d8ff"/>
      <stop offset="1" stop-color="#7348ff"/>
    </linearGradient>
    <linearGradient id="rim" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.22"/>
      <stop offset="1" stop-color="#ffffff" stop-opacity="0.02"/>
    </linearGradient>
    <filter id="blur" x="-20%" y="-20%" width="140%" height="140%">
      <feGaussianBlur stdDeviation="{18 * s:.1f}"/>
    </filter>
    <clipPath id="body">
      <rect x="{body_x}" y="{body_x}" width="{body_size}" height="{body_size}" rx="{radius}"/>
    </clipPath>
  </defs>
  <g clip-path="url(#body)">
    <rect x="{body_x}" y="{body_x}" width="{body_size}" height="{body_size}" fill="url(#bg)"/>
    <rect x="{body_x}" y="{body_x}" width="{body_size}" height="{body_size}" fill="url(#glow)"/>
    {"".join(bars)}
    <path d="{wave}" fill="none" stroke="#7fe3ff" stroke-width="{44 * s:.1f}"
          stroke-linecap="round" stroke-linejoin="round" filter="url(#blur)" opacity="0.9"/>
    <path d="{wave}" fill="none" stroke="#eefaff" stroke-width="{34 * s:.1f}"
          stroke-linecap="round" stroke-linejoin="round"/>
  </g>
  <rect x="{body_x + 3 * s}" y="{body_x + 3 * s}" width="{body_size - 6 * s}" height="{body_size - 6 * s}"
        rx="{radius - 3 * s}" fill="none" stroke="url(#rim)" stroke-width="{6 * s:.1f}"/>
</svg>
'''


def render(svg_file, size, png_file):
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), "-o",
                    str(png_file), str(svg_file)], check=True)


def main():
    OUT.mkdir(exist_ok=True)
    layouts = {
        "macos": svg(100, 824, 185),   # Apple's template: 824 body, 185 corner
        "full": svg(24, 976, 200),
    }
    for name, text in layouts.items():
        (HERE / f"resynth-studio-{name}.svg").write_text(text)

    full = HERE / "resynth-studio-full.svg"
    mac = HERE / "resynth-studio-macos.svg"
    # The app's window icon (X11 and Windows title bar / taskbar).
    render(full, 256, HERE / "resynth-studio-256.png")
    render(full, 512, HERE / "resynth-studio-512.png")

    # Windows .ico: every size Explorer asks for, each rendered from the vector
    # rather than scaled down from one bitmap, so 16 px stays sharp.
    ico_sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    imgs = []
    for size in ico_sizes:
        p = OUT / f"full-{size}.png"
        render(full, size, p)
        imgs.append(Image.open(p).convert("RGBA"))
    imgs[-1].save(HERE / "resynth-studio.ico", sizes=[(s, s) for s in ico_sizes],
                  append_images=imgs[:-1])

    # macOS .icns, from the Apple-grid layout.
    p = OUT / "macos-1024.png"
    render(mac, 1024, p)
    Image.open(p).convert("RGBA").save(HERE / "AppIcon.icns")
    print("wrote", ", ".join(sorted(f.name for f in HERE.iterdir() if f.is_file())))


if __name__ == "__main__":
    sys.exit(main())
