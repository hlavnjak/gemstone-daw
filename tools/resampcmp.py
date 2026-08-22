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
"""resampcmp - measure a Synth-timeline key render against the one thing it is
supposed to be.

The Synth timeline lays exactly one grain per bucket, and every bucket's period
is the source's true period times a single constant (`base_period / nominal`).
The whole render is therefore a *uniform* time-scale of the source: a pure
resampling and nothing else. That makes the perfect reference computable —
band-limited-resample the exact inverse by the same factor and subtract. What is
left is the grain pipeline's own error.

Two things are fitted out first, because neither is the defect and both swamp it:

  * the exact time-scale, refined from the drift of a per-block lag fit. Taking
    it as `len(test)/len(ref)` is wrong by a few parts in 10^5 — a tenth of a
    cent, inaudible — and that alone reads as -12 dB of "distortion".
  * a constant gain and a *fractional* delay. An integer-lag fit reports a
    sub-sample offset as distortion; this problem has already cost a day to that
    trap once.

Usage:
    resampcmp.py REF.wav TEST.wav [--blocks] [--quiet]
"""

import sys

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from buzzscan import load_wav  # noqa: E402

BANDS = [(0, 500), (500, 1000), (1000, 2000), (2000, 4000),
         (4000, 6000), (6000, 9000), (9000, 12000)]


def resample_to(x, n_out):
    """Band-limited resample to `n_out` samples via the DFT. Exact for a
    band-limited signal; the ends are treated as periodic, so callers drop a
    margin at both."""
    n_in = len(x)
    X = np.fft.rfft(x)
    n_keep = min(len(X), n_out // 2 + 1)
    Y = np.zeros(n_out // 2 + 1, dtype=complex)
    Y[:n_keep] = X[:n_keep]
    return np.fft.irfft(Y, n_out) * (n_out / n_in)


def fit_out(test, ref, span=1.0):
    """Remove a constant fractional delay and gain from `ref`. Returns
    (aligned_ref, lag, gain)."""
    n = min(len(test), len(ref))
    t, r = test[:n], ref[:n]
    T, R = np.fft.rfft(t), np.fft.rfft(r)
    xc = np.fft.irfft(T * np.conj(R), n)
    lag0 = int(np.argmax(xc))
    if lag0 > n // 2:
        lag0 -= n
    f = np.fft.rfftfreq(n)
    best = (0.0, 1.0, np.inf)
    for frac in np.arange(-span, span + 1e-9, 0.02):
        lag = lag0 + frac
        shifted = np.fft.irfft(R * np.exp(-2j * np.pi * f * lag), n)
        g = np.dot(t, shifted) / max(np.dot(shifted, shifted), 1e-30)
        e = float(np.sum((t - g * shifted) ** 2))
        if e < best[2]:
            best = (lag, g, e)
    lag, g, _ = best
    return g * np.fft.irfft(R * np.exp(-2j * np.pi * f * lag), n), lag, g


def block_lags(test, ref, block=4096):
    out = []
    for i in range(0, len(test) - block, block):
        a, lag, g = fit_out(test[i:i + block], ref[i:i + block])
        e = float(np.dot(test[i:i + block] - a, test[i:i + block] - a))
        s = float(np.dot(a, a))
        out.append((i, lag, g, 10 * np.log10(max(e / max(s, 1e-30), 1e-30))))
    return out


def refine_ratio(ref, test, margin=0.05):
    """The render's true time-scale, from the slope of the per-block lag."""
    n_out = float(len(test))
    for _ in range(3):
        ideal = resample_to(ref, int(round(n_out)))
        n = min(len(ideal), len(test))
        m = int(margin * n)
        lags = block_lags(test[m:n - m], ideal[m:n - m])
        if len(lags) < 3:
            break
        xs = np.array([i for i, _, _, _ in lags], dtype=float)
        ys = np.array([l for _, l, _, _ in lags])
        slope = np.polyfit(xs, ys, 1)[0]
        if abs(slope) < 1e-7:
            break
        # A lag that grows by `slope` per sample means the reference is that
        # much too long; shortening it by the same fraction removes the drift.
        n_out *= 1.0 + slope
    return n_out


def band_levels(x, sr):
    X = np.abs(np.fft.rfft(x)) ** 2
    f = np.fft.rfftfreq(len(x), 1.0 / sr)
    return [float(X[(f >= lo) & (f < hi)].sum()) for lo, hi in BANDS]


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    show_blocks = "--blocks" in sys.argv
    if len(args) < 2:
        print(__doc__)
        return 1
    ref_path, test_path = args[0], args[1]
    ref, sr = load_wav(ref_path)
    test, _ = load_wav(test_path)
    ref = ref.mean(axis=1)
    test = test.mean(axis=1)

    n_out = refine_ratio(ref, test)
    ideal = resample_to(ref, int(round(n_out)))
    n = min(len(ideal), len(test))
    m = int(0.05 * n)
    ideal, cut = ideal[m:n - m], test[m:n - m]
    aligned, lag, g = fit_out(cut, ideal, span=8.0)
    resid = cut - aligned

    e_r = float(np.dot(resid, resid))
    e_s = float(np.dot(aligned, aligned))
    print(f"{test_path}")
    print(f"  vs {ref_path} resampled x{n_out / len(ref):.6f} "
          f"(naive {len(test) / len(ref):.6f}), lag {lag:+.2f}, gain {g:.4f}")
    print(f"  RESIDUAL {10 * np.log10(max(e_r / max(e_s, 1e-30), 1e-30)):6.1f} dB"
          f"   over {len(cut)} samples")
    rb, sb = band_levels(resid, sr), band_levels(aligned, sr)
    tot = sum(sb)
    for (lo, hi), r, s in zip(BANDS, rb, sb):
        rel = 10 * np.log10(max(r / max(s, 1e-30), 1e-30))
        abs_ = 10 * np.log10(max(r / max(tot, 1e-30), 1e-30))
        sig = 10 * np.log10(max(s / max(tot, 1e-30), 1e-30))
        print(f"    {lo:5d}-{hi:5d} Hz  signal {sig:6.1f}  residual {abs_:6.1f}"
              f"  ({rel:+6.1f} dB rel)")
    if show_blocks:
        print("   blk      lag    gain   resid")
        for i, l, gg, d in block_lags(cut, ideal):
            print(f"  {i // 4096:4d} {l:+8.2f} {gg:7.4f} {d:7.1f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
