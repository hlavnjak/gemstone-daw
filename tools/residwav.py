#!/usr/bin/env python3
"""Write the resampcmp residual to a wav so it can be scanned and heard."""
import sys
import numpy as np
sys.path.insert(0, __file__.rsplit("/", 1)[0])
from buzzscan import load_wav, write_wav_f32
from resampcmp import resample_to, fit_out, refine_ratio

ref_p, test_p, out_p = sys.argv[1], sys.argv[2], sys.argv[3]
gain = float(sys.argv[4]) if len(sys.argv) > 4 else 1.0
ref, sr = load_wav(ref_p); ref = ref.mean(axis=1)
test, _ = load_wav(test_p); test = test.mean(axis=1)
n_out = refine_ratio(ref, test)
ideal = resample_to(ref, int(round(n_out)))
n = min(len(ideal), len(test)); m = int(0.05 * n)
ideal, cut = ideal[m:n - m], test[m:n - m]
aligned, lag, g = fit_out(cut, ideal, span=8.0)
r = cut - aligned
print(f"lag {lag:+.2f} gain {g:.4f} rms {np.sqrt(np.mean(r**2)):.6f} -> {out_p}")
write_wav_f32(out_p, (r * gain).astype(np.float32), sr)
write_wav_f32(out_p.replace(".wav", "_ideal.wav"), aligned.astype(np.float32), sr)
