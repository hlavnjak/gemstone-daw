#!/usr/bin/env python3
"""Sample-aligned residual of a render against `psolaref`'s ideal, per band."""
import sys
import numpy as np
sys.path.insert(0, __file__.rsplit("/", 1)[0])
from buzzscan import load_wav, write_wav_f32
from resampcmp import band_levels, BANDS

ref, sr = load_wav(sys.argv[1]); ref = ref.mean(axis=1)
test, _ = load_wav(sys.argv[2]); test = test.mean(axis=1)
n = min(len(ref), len(test)); m = int(0.02 * n)
r, t = ref[m:n - m], test[m:n - m]
g = float(np.dot(t, r) / max(np.dot(r, r), 1e-30))
e = t - g * r
print(f"{sys.argv[2]}  vs  {sys.argv[1]}   (gain {g:.4f}, {n} samples)")
print(f"  RESIDUAL {10 * np.log10(max(np.dot(e, e) / max(np.dot(r, r), 1e-30), 1e-30)):6.1f} dB")
br, bs = band_levels(e, sr), band_levels(g * r, sr)
tot = sum(bs)
for (lo, hi), rr, ss in zip(BANDS, br, bs):
    print(f"    {lo:5d}-{hi:5d} Hz  signal {10*np.log10(max(ss/tot,1e-30)):6.1f}"
          f"  residual {10*np.log10(max(rr/tot,1e-30)):6.1f}"
          f"  ({10*np.log10(max(rr,1e-30)/max(ss,1e-30)):+6.1f} dB rel)")
if len(sys.argv) > 3:
    write_wav_f32(sys.argv[3], e.astype(np.float32), sr)
