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
"""joinstep - measure the discontinuity the renderer leaves at each grain join.

`psolaref` cannot see this one: it lays its grains on the same epochs from the
same buckets, so whatever the renderer does at a *splice* the reference does too
and the two cancel. This looks at the render alone and asks, at each epoch the
renderer used, how far the waveform coming in disagrees with the waveform going
out — a cubic extrapolated from four samples either side, which is the same
"corner" measure `buzzscan` uses and is blind to the smooth material around it.

A join is a **continuation** when the next grain's bucket is the previous one
plus 1: consecutive periods of the source, which must line up exactly. It is a
**splice** when the key's period made the renderer skip or repeat a bucket, and
those are the only joins a step can legitimately appear at.

Usage:
    joinstep.py RENDER.wav BUCKETS.txt --note HZ [--synth-timeline]
"""

import sys

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from buzzscan import load_wav  # noqa: E402
from psolaref import true_periods  # noqa: E402


def epochs(lengths, ratios, base_freq, rate, note, synth, n_out):
    P = true_periods(lengths, ratios)
    start = np.concatenate([[0.0], np.cumsum(P)])
    k = (rate / note) / (rate / base_freq)
    p, nb = P * k, len(P)
    total = float(n_out) if not synth else float(p.sum())
    acc = float(P.sum())
    out, tau, n = [], 0.0, 0
    while tau < total:
        if synth:
            if n >= nb:
                break
            b = n
        else:
            along = tau / total * acc
            b = max(min(int(np.searchsorted(start, along, side="right")) - 1, nb - 1), 0)
        out.append((tau, b))
        tau += p[b]
        n += 1
    return out


def corner(x, i, w=4):
    """How far the two sides disagree at sample `i`, each extrapolated there by a
    cubic through `w` samples of its own."""
    if i - w < 0 or i + w + 1 > len(x):
        return None
    t = np.arange(-w, 0)
    left = np.polyval(np.polyfit(t, x[i - w:i], 3), 0.0)
    right = np.polyval(np.polyfit(np.arange(1, w + 1), x[i + 1:i + 1 + w], 3), 0.0)
    return right - left


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    note = float(sys.argv[sys.argv.index("--note") + 1])
    synth = "--synth-timeline" in sys.argv
    x, sr = load_wav(args[0])
    x = x.mean(axis=1)
    head = open(args[1]).readline()
    base_freq = float(head.split("base_freq")[1].split()[0])
    rate = float(head.split("rate")[1].split(")")[0])
    rows = [l.split() for l in open(args[1]) if not l.startswith("#")]
    lengths = np.array([float(a) for a, _ in rows])
    ratios = np.array([float(b) for _, b in rows])

    ep = epochs(lengths, ratios, base_freq, rate, note, synth, len(x))
    cont, splice = [], []
    for j in range(1, len(ep) - 1):
        tau, b = ep[j]
        prev_b = ep[j - 1][1]
        c = corner(x, int(round(tau)))
        if c is None:
            continue
        (cont if b == prev_b + 1 else splice).append(abs(c))
    rms = np.sqrt(np.mean(x ** 2))
    # The control. A cubic through four samples is a poor model of a waveform
    # carrying content near Nyquist, so at a high key the measure below reads
    # large everywhere — including where there is no join at all. Without this
    # row the renderer gets blamed for the signal's own curvature.
    rng = np.random.default_rng(0)
    joins = {int(round(t)) for t, _ in ep}
    ctrl = []
    for i in rng.integers(8, len(x) - 8, size=2000):
        if int(i) in joins:
            continue
        c = corner(x, int(i))
        if c is not None:
            ctrl.append(abs(c))
    print(f"{args[0]}  note {note:g} Hz   {len(ep)} grains, signal rms {rms:.4f}")
    for name, v in (("no join", ctrl), ("continuation", cont), ("splice", splice)):
        if not v:
            print(f"  {name:12s} none")
            continue
        v = np.array(v)
        print(f"  {name:12s} {len(v):5d} joins ({100*len(v)/max(len(cont)+len(splice),1):5.1f}%)  "
              f"median step {np.median(v):.5f}  rms {np.sqrt(np.mean(v**2)):.5f}  "
              f"max {v.max():.4f}   ({20*np.log10(max(np.sqrt(np.mean(v**2)),1e-12)/rms):+.1f} dB vs signal)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
