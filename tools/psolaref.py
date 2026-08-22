#!/usr/bin/env python3
# Copyright 2026 Jakub Hlavnicka
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
"""psolaref - build the render a key is *trying* to produce, so the grain
pipeline can be measured on its own.

A key's output is one grain per output period, grain `n` drawn from bucket
`b(n)` and played at the key's period. The ideal such output needs no harmonics
at all: it is the exact inverse read straight off, one true source period
stretched onto one key period,

    ideal(tau_n + u * p_n) = source(start_b + u * P_b),    u in [0, 1)

with `start_b` and `P_b` the bucket's own place and true period. Everything the
renderer does beyond this — cutting a period into harmonics, capping the band,
cross-fading the grains — shows up as a residual against it, and everything
inherent to *pitch-shifting by repeating and skipping periods* does not, because
this reference repeats and skips exactly the same ones.

That separation is the point. Comparing a "preserve seconds" render against a
plain resampling cannot tell the two apart, and blames the pipeline for the
method.

Usage:
    psolaref.py SOURCE.wav BUCKETS.txt --note HZ [--synth-timeline] --out OUT.wav

`BUCKETS.txt` is what `dump_render` writes: one `length pitch_ratio` line per
bucket, with the analysis base frequency and rate in the header.
"""

import sys

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from buzzscan import load_wav, write_wav_f32  # noqa: E402

TAPS = 32


def true_periods(lengths, ratios):
    """The plugin's own `true_periods`: shape from the pitch contour, scale from
    the lengths, with the last bucket left out of the scale."""
    shape = 1.0 / np.maximum(ratios, 1e-3)
    upto = max(len(lengths) - 1, 1)
    scale = lengths[:upto].sum() / shape[:upto].sum()
    return np.maximum(shape * scale, 2.0)


def sinc_read(s, pos):
    """Band-limited read at fractional positions — a Kaiser-windowed sinc, the
    same width as the plugin's `sinc_read`."""
    pos = np.asarray(pos, float)
    n0 = np.floor(pos).astype(int)
    frac = pos - n0
    out = np.zeros_like(pos)
    for j in range(-TAPS + 1, TAPS + 1):
        idx = n0 + j
        x = np.abs(frac - j)
        t = np.clip(x / TAPS, 0, 1)
        w = np.sinc(frac - j) * np.i0(10.0 * np.sqrt(np.maximum(1 - t * t, 0))) / np.i0(10.0)
        ok = (idx >= 0) & (idx < len(s))
        out[ok] += s[idx[ok]] * w[ok]
    return out


def band_limit(x, keep):
    """Lowpass to `keep` of Nyquist. Reading the source faster than it was
    written multiplies every frequency in it, and a plain fractional read has
    nothing to stop what lands past Nyquist from folding back — so a reference
    for an *upward* key has to be band-limited first or it is aliased noise,
    and the renderer's correct anti-alias cap then measures as its error."""
    X = np.fft.rfft(x)
    X[int(len(X) * keep):] = 0
    return np.fft.irfft(X, len(x))


def build(src, lengths, ratios, base_freq, rate, note, synth_timeline):
    P = true_periods(lengths, ratios)
    start = np.concatenate([[0.0], np.cumsum(P)])
    k = (rate / note) / (rate / base_freq)
    if k < 1.0:
        src = band_limit(src, k)
    p = P * k
    nb = len(P)
    total = len(src) if not synth_timeline else float(p.sum())
    acc = float(P.sum())
    def bucket_at(tau, n):
        if synth_timeline:
            return n if n < nb else None
        along = tau / total * acc
        return max(min(int(np.searchsorted(start, along, side="right")) - 1, nb - 1), 0)

    # Where each period boundary sits, as a value. A grain that is not followed
    # by the very next bucket has to be tilted onto the one that *is* played
    # next, or the reference carries a step of its own at every splice — and
    # then it cannot say anything about the renderer's steps, because it has the
    # same ones. This is the reference for a *continuous* render.
    origin = sinc_read(src, start[:nb])

    out = np.zeros(int(np.ceil(total)))
    tau, n = 0.0, 0
    while tau < total:
        b = bucket_at(tau, n)
        if b is None:
            break
        pn = p[b]
        idx = np.arange(int(np.ceil(tau)), int(np.floor(tau + pn)) + 1)
        u = (idx - tau) / pn
        keep = (u > 0) & (u < 1) & (idx < len(out))
        idx, u = idx[keep], u[keep]
        nb_next = bucket_at(tau + pn, n + 1)
        if nb_next is None:
            nb_next = min(b + 1, nb - 1)
        tilt = origin[nb_next] - origin[min(b + 1, nb - 1)]
        out[idx] = sinc_read(src, start[b] + u * P[b]) + tilt * u
        tau += pn
        n += 1
    return out[: int(total)]


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    note = float(sys.argv[sys.argv.index("--note") + 1])
    out_path = sys.argv[sys.argv.index("--out") + 1]
    synth = "--synth-timeline" in sys.argv
    src, sr = load_wav(args[0])
    src = src.mean(axis=1)
    head = open(args[1]).readline()
    base_freq = float(head.split("base_freq")[1].split()[0])
    rate = float(head.split("rate")[1].split(")")[0])
    rows = [l.split() for l in open(args[1]) if not l.startswith("#")]
    lengths = np.array([float(a) for a, _ in rows])
    ratios = np.array([float(b) for _, b in rows])
    y = build(src, lengths, ratios, base_freq, rate, note, synth)
    write_wav_f32(out_path, y.astype(np.float32), sr)
    print(f"wrote {out_path}  ({len(y)} samples, note {note} Hz, "
          f"{'synth timeline' if synth else 'preserve seconds'})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
