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
"""buzzscan - locate and characterise a periodic buzz in a wav file.

A buzz that is inaudible in Audacity's waveform view is almost always a small
defect that *recurs at a fixed rate*. A single seam is a click; the same seam
once per period is a buzz, because an impulse train at f0 spreads its energy
flat across the whole spectrum as a comb. So the question this answers is not
"where are the jumps" but "at what rate do they recur" - that rate names the
stage that produced them (per-period seam vs per-bucket seam vs audio block).

Usage:
    buzzscan.py FILE.wav [--hp HZ] [--z N] [--from S] [--to S] [--repair DIR]

    --hp HZ       high-pass corner for the modulation analysis (default 4000)
    --z N         outlier threshold in robust sigmas (default 6)
    --from/--to   restrict analysis to a time span, in seconds
    --ref FILE    subtract FILE first and scan the residual, so the source's own
                  once-per-period features (a voice's glottal pulse!) cancel and
                  only what the render *added* is left. Valid ONLY between
                  signals meant to be sample-aligned - exact.wav vs source.wav,
                  or one render before and after a code change. A playback
                  render de-phases from the source by design; subtracting it
                  measures phase drift and nothing else.
    --vs FILE     the same subtraction for two files that are NOT sample-aligned:
                  a constant lag and a constant level are fitted out first, then
                  the residual is reported per band and scanned. This is what to
                  use on two renders of the same note - key vs contour, or one
                  key render before and after a change - and it is the only mode
                  that can see past a voice's own glottal pulse on a key. It
                  warns instead of lying when the two disagree about pitch, which
                  is the one difference a constant fit cannot absorb.
    --repair DIR  write DIR/repaired.wav (defect interpolated away) and
                  DIR/glitch_only.wav (the defect alone, +18 dB). If the buzz
                  is gone from the first and audible in the second, the events
                  reported here are the whole of it.

Reads PCM 16/24/32 and IEEE float 32/64 wavs, so it takes the float32 dumps
`dump_render` writes as well as a PulseAudio capture.

Reports:
    [1] f0 estimate (autocorrelation)
    [2] 2nd-difference outliers - catches slope breaks (a "corner"), which are
        invisible in a waveform view, not just value jumps
    [3] recurrence rate of those events        <- names the culprit
    [4] HF envelope modulation spectrum        <- same answer, independent path
    [5] harmonic vs inharmonic energy split
"""

import os
import struct
import sys

import numpy as np

# ------------------------------------------------------------------ wav io


def load_wav(path):
    """Return (samples[n, ch] float64 in -1..1, sample_rate). Minimal RIFF
    parser: Python's `wave` module rejects IEEE-float wavs, which is what the
    offline dumps are written as."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError(f"{path}: not a RIFF/WAVE file")
    pos, fmt, raw = 12, None, None
    while pos + 8 <= len(data):
        cid = data[pos : pos + 4]
        (csz,) = struct.unpack("<I", data[pos + 4 : pos + 8])
        body = data[pos + 8 : pos + 8 + csz]
        if cid == b"fmt ":
            tag, ch, sr, _, _, bits = struct.unpack("<HHIIHH", body[:16])
            if tag == 0xFFFE and len(body) >= 40:  # WAVE_FORMAT_EXTENSIBLE
                tag = struct.unpack("<H", body[24:26])[0]
            fmt = (tag, ch, sr, bits)
        elif cid == b"data":
            raw = body
        pos += 8 + csz + (csz & 1)  # chunks are word-aligned
    if fmt is None or raw is None:
        raise ValueError(f"{path}: missing fmt or data chunk")
    tag, ch, sr, bits = fmt

    if tag == 3 and bits == 32:
        x = np.frombuffer(raw, "<f4").astype(np.float64)
    elif tag == 3 and bits == 64:
        x = np.frombuffer(raw, "<f8").astype(np.float64)
    elif tag == 1 and bits == 16:
        x = np.frombuffer(raw, "<i2").astype(np.float64) / 32768.0
    elif tag == 1 and bits == 32:
        x = np.frombuffer(raw, "<i4").astype(np.float64) / 2147483648.0
    elif tag == 1 and bits == 8:
        x = np.frombuffer(raw, "u1").astype(np.float64) / 128.0 - 1.0
    elif tag == 1 and bits == 24:
        b = np.frombuffer(raw, "u1").reshape(-1, 3).astype(np.int32)
        v = b[:, 0] | (b[:, 1] << 8) | (b[:, 2] << 16)
        v = np.where(v & 0x800000, v - 0x1000000, v)
        x = v.astype(np.float64) / 8388608.0
    else:
        raise ValueError(f"{path}: unsupported format tag {tag}, {bits} bits")

    n = (len(x) // ch) * ch
    return x[:n].reshape(-1, ch), float(sr)


def write_wav_f32(path, x, sr):
    x = np.atleast_2d(x.T).T if x.ndim == 1 else x
    ch, n = x.shape[1], x.shape[0]
    body = x.astype("<f4").tobytes()
    hdr = b"RIFF" + struct.pack("<I", 36 + len(body)) + b"WAVEfmt "
    hdr += struct.pack("<IHHIIHH", 16, 3, ch, int(sr), int(sr) * ch * 4, ch * 4, 32)
    with open(path, "wb") as f:
        f.write(hdr + b"data" + struct.pack("<I", len(body)) + body)


# ------------------------------------------------------------------ helpers


def steady_region(mono, sr):
    """Largest span above -40 dB of peak, trimmed 50 ms at each end. Analysing
    across digital silence poisons every robust statistic below."""
    k = max(1, int(sr) // 200)
    env = np.convolve(np.abs(mono), np.ones(k) / k, mode="same")
    if env.max() <= 0:
        return 0, len(mono)
    idx = np.flatnonzero(env > env.max() * 0.01)
    if len(idx) == 0:
        return 0, len(mono)
    a, b = idx[0], idx[-1] + 1
    pad = int(sr) // 20
    return int(min(a + pad, b)), int(max(b - pad, a + 1))


def estimate_f0(x, sr, fmin=40.0, fmax=1200.0):
    n = min(len(x), 1 << 15)
    if n < 64:
        return 0.0
    seg = x[:n] * np.hanning(n)
    seg = seg - seg.mean()
    sp = np.fft.rfft(seg, 2 * n)
    ac = np.fft.irfft(sp * np.conj(sp))[:n]
    if ac[0] <= 0:
        return 0.0
    ac /= ac[0]
    lo, hi = int(sr / fmax), min(int(sr / fmin), n - 1)
    if hi <= lo:
        return 0.0
    win = ac[lo:hi]
    best = float(win.max())
    if best <= 0:
        return 0.0
    # Autocorrelation peaks just as hard at every *multiple* of the true period,
    # so argmax lands on a subharmonic often enough to mislabel every rate below
    # it. Take the shortest lag that is a local maximum within 10% of the best.
    cand = np.flatnonzero(
        (win[1:-1] >= win[:-2]) & (win[1:-1] >= win[2:]) & (win[1:-1] >= 0.9 * best)
    )
    lag = lo + (int(cand[0]) + 1 if len(cand) else int(np.argmax(win)))
    if 0 < lag < n - 1:  # parabolic refine - the period is rarely an integer
        y0, y1, y2 = ac[lag - 1], ac[lag], ac[lag + 1]
        d = y0 - 2 * y1 + y2
        if d != 0:
            lag += 0.5 * (y0 - y2) / d
    return sr / lag if lag else 0.0


def robust_z(d2, block=4096):
    """|d2| in units of a *locally* estimated sigma. The scale is re-estimated
    per block so a quiet passage is judged against its own noise floor and a
    silent one cannot drive the global MAD to zero."""
    z = np.zeros(len(d2))
    for i in range(0, len(d2), block):
        blk = d2[i : i + block]
        if len(blk) == 0:
            continue
        mad = np.median(np.abs(blk - np.median(blk)))
        scale = 1.4826 * mad if mad > 0 else blk.std()
        if scale > 0:
            z[i : i + block] = np.abs(blk) / scale
    return z


def find_events(x, zthr):
    """Indices into x of 2nd-difference outliers, one per cluster."""
    z = robust_z(np.diff(x, n=2))
    hits = np.flatnonzero(z > zthr) + 1
    hits = hits[(hits > 4) & (hits < len(x) - 4)]
    if len(hits) == 0:
        return hits, z
    grp = np.split(hits, np.flatnonzero(np.diff(hits) > 8) + 1)
    return np.array([g[np.argmax(z[g - 1])] for g in grp]), z


def label_rate(rate, f0):
    if f0 <= 0 or rate <= 0:
        return ""
    r = rate / f0
    if abs(r - round(r)) < 0.06 and round(r) == 1:
        return "  == f0   (per-period seam)"
    if abs(r - round(r)) < 0.06 and round(r) > 1:
        return f"  == {round(r)}x f0"
    if r > 0 and abs(1 / r - round(1 / r)) < 0.06 and round(1 / r) >= 2:
        return f"  == f0/{round(1 / r)}  (every {round(1 / r)} periods)"
    return ""


# ------------------------------------------------------------------ stages


def recurrence(peaks, sr, f0):
    if len(peaks) < 4:
        print("  too few events to establish a rate")
        return
    ioi = np.diff(peaks)
    ioi = ioi[ioi > 1]
    if len(ioi) == 0:
        return
    hi = int(np.percentile(ioi, 97))
    bins = np.bincount(np.clip(ioi, 0, hi))
    print("  inter-event intervals (most common first):")
    for t in np.argsort(bins)[::-1][:5]:
        if t == 0 or bins[t] == 0:
            continue
        tag = label_rate(sr / t, f0)
        if t in (64, 128, 256, 512, 1024, 2048):
            tag += f"  [{t} = audio block size?]"
        print(
            f"    {t:6d} samples  {1000 * t / sr:8.3f} ms  {sr / t:9.2f} Hz  "
            f"x{bins[t]:<5d}{tag}"
        )


def event_shape(x, peaks, w=8):
    """Mean deviation of each event from a cubic fit of the samples around it,
    normalised and sign-aligned. Tells a 1-sample impulse (one wrong sample)
    apart from a step (level jump) or a corner (phase mismatch)."""
    dev, amp = [], []
    t = np.arange(-w, w + 1)
    mask = (t < -3) | (t > 2)
    for s in peaks:
        if s - w < 0 or s + w + 1 > len(x):
            continue
        seg = x[s - w : s + w + 1]
        r = seg - np.polyval(np.polyfit(t[mask], seg[mask], 3), t)
        i = int(np.argmax(np.abs(r)))
        if abs(r[i]) > 1e-9:
            dev.append(r / abs(r[i]) * np.sign(r[i]))
            amp.append(r[i])
    if not dev:
        return
    dev, amp = np.array(dev), np.array(amp)
    m, sd = dev.mean(0), dev.std(0)
    print(f"  mean deviation from a cubic fit of the surrounding waveform (n={len(dev)}):")
    print("    offset :", " ".join(f"{v:+5d}" for v in t))
    print("    mean   :", " ".join(f"{v:+5.2f}" for v in m))
    print("    sd     :", " ".join(f"{v:5.2f}" for v in sd))

    # Width: how many samples around the centre carry the deviation.
    wide = np.abs(m) > 0.3
    width = 0
    for i in range(w, -1, -1):
        if not wide[i]:
            break
        width += 1
    for i in range(w + 1, len(m)):
        if not wide[i]:
            break
        width += 1
    # Step: does the waveform come back to the level it left?
    pre = m[: w - 3].mean() if w > 3 else 0.0
    post = m[w + 4 :].mean() if len(m) > w + 4 else 0.0
    if abs(post - pre) > 0.3:
        kind = "STEP — settles at a new level (gain/offset change at the seam)"
    elif width <= 2:
        kind = "IMPULSE — 1-2 wrong samples, waveform resumes on trend"
    else:
        kind = f"PULSE — {width} samples wide (a transient, not a single bad sample)"
    consistency = float(sd[w - 1 : w + 2].mean())
    print(f"  shape    : {kind}")
    print(
        f"  amplitude: median |A| = {np.median(np.abs(amp)):.5f}, "
        f"sign {int(np.sum(amp > 0))} pos / {int(np.sum(amp < 0))} neg"
        + ("  (random sign)" if 0.35 < np.mean(amp > 0) < 0.65 else "  (consistent polarity)")
    )
    print(
        f"  consistency: sd {consistency:.2f} across the core — "
        + (
            "the same shape every time"
            if consistency < 0.2
            else "varies per occurrence" if consistency > 0.35 else "moderately variable"
        )
    )
    print(
        "  NOTE: a periodic source has periodic *features* of its own — a voice's\n"
        "        glottal pulse is a wide, consistent, single-polarity pulse once per\n"
        "        period and is not a defect. Use --ref to subtract the reference and\n"
        "        scan only what the render added."
    )


def hf_modulation(x, sr, hp):
    """Peaks of the high band's envelope spectrum, plus the modulation depth.

    Read this as *rate confirmation for the events in [2]*, never on its own: a
    clean harmonic tone already pulses its high band once per period, because
    the harmonics re-align there. It is the depth, and agreement with [2], that
    separates a defect from ordinary periodicity."""
    n = len(x)
    sp = np.fft.rfft(x)
    sp[np.fft.rfftfreq(n, 1 / sr) < hp] = 0
    env = np.abs(np.fft.irfft(sp, n))
    k = max(1, int(sr) // 4000)
    env = np.convolve(env, np.ones(k) / k, mode="same")
    depth = env.std() / env.mean() if env.mean() > 0 else 0.0
    env = env - env.mean()
    m = min(len(env), 1 << 19)
    if m < 1024:
        return depth, []
    esp = np.abs(np.fft.rfft(env[:m] * np.hanning(m)))
    ef = np.fft.rfftfreq(m, 1 / sr)
    band = (ef > 15) & (ef < 2000)
    esp, ef = esp[band], ef[band]
    if len(esp) == 0 or esp.max() <= 0:
        return depth, []
    esp = esp / esp.max()
    pk = np.flatnonzero((esp[1:-1] > esp[:-2]) & (esp[1:-1] > esp[2:])) + 1
    out, used = [], []
    for p in pk[np.argsort(esp[pk])[::-1]]:
        if any(abs(ef[p] - u) < 8 for u in used):
            continue
        used.append(ef[p])
        out.append((ef[p], 20 * np.log10(esp[p] + 1e-30)))
        if len(out) == 6:
            break
    return depth, out


def align_to(x, ref, sr, max_lag=2000):
    """Best integer lag and gain that fit `ref` onto `x`, and the residual.

    `--ref` demands the two files already line up sample for sample, which two
    *renders* never do: they differ by a constant level and by a few samples of
    start offset even when nothing is wrong. Refusing to subtract them is why
    this tool could only ever be pointed at a signal on its own, where a voice's
    glottal pulse buries everything the render added.

    A constant lag and a constant gain are not defects, so fitting them out
    costs nothing and makes the subtraction legitimate. What it cannot fit out —
    and must not — is a *drifting* phase: if the two renders disagree about
    pitch, the residual grows with time and says nothing about buzz. The caller
    gets `drift` to check exactly that.
    """
    n = min(len(x), len(ref))
    x, ref = x[:n], ref[:n]
    m = 1 << int(np.ceil(np.log2(2 * n)))
    cc = np.fft.irfft(np.fft.rfft(x, m) * np.conj(np.fft.rfft(ref, m)), m)
    cc = np.concatenate([cc[-max_lag:], cc[: max_lag + 1]])
    lag = int(np.argmax(np.abs(cc))) - max_lag
    if lag > 0:
        a, b = x[lag:], ref[: n - lag]
    elif lag < 0:
        a, b = x[: n + lag], ref[-lag:]
    else:
        a, b = x, ref
    g = float(np.dot(a, b) / max(np.dot(b, b), 1e-30))
    r = a - g * b
    db = 10 * np.log10(np.dot(r, r) / max(np.dot(b, b), 1e-30) + 1e-30)
    # Drift check: the same fit on the first and last thirds. A pitch
    # disagreement makes the second far worse than the first.
    k = len(a) // 3
    def part(u, v):
        gg = float(np.dot(u, v) / max(np.dot(v, v), 1e-30))
        rr = u - gg * v
        return 10 * np.log10(np.dot(rr, rr) / max(np.dot(v, v), 1e-30) + 1e-30)
    drift = part(a[-k:], b[-k:]) - part(a[:k], b[:k]) if k > 512 else 0.0
    return r, lag, g, db, drift


def band_residual(r, ref, sr):
    """Residual energy per band, as dB below the reference's total."""
    R = np.abs(np.fft.rfft(r)) ** 2
    tot = float((np.abs(np.fft.rfft(ref[: len(r)])) ** 2).sum())
    f = np.fft.rfftfreq(len(r), 1 / sr)
    out = []
    for lo, hi in ((0, 500), (500, 2000), (2000, 5000), (5000, 10000), (10000, 20000)):
        sel = (f >= lo) & (f < hi)
        if sel.any():
            out.append((lo, hi, 10 * np.log10(R[sel].sum() / max(tot, 1e-30) + 1e-30)))
    return out


def harmonic_split(x, sr, f0):
    if f0 <= 0:
        return None
    n = min(len(x), 1 << 16)
    sp = np.abs(np.fft.rfft(x[:n] * np.hanning(n))) ** 2
    f = np.fft.rfftfreq(n, 1 / sr)
    tol = max(2.0, f0 * 0.03)
    mask = np.zeros(len(f), bool)
    for k in range(1, int(sr / 2 / f0) + 1):
        mask |= np.abs(f - k * f0) < tol
    tot = sp.sum()
    if tot <= 0:
        return None
    prof = []
    for a, b in zip([0, 500, 2000, 5000, 10000], [500, 2000, 5000, 10000, sr / 2]):
        sel = (f >= a) & (f < b) & ~mask
        prof.append((a, b, 10 * np.log10(sp[sel].sum() / tot + 1e-30)))
    return 10 * np.log10((tot - sp[mask].sum()) / tot + 1e-30), prof


def repair(xs, sr, zthr, outdir):
    """Interpolate every detected event away, per channel, and write both the
    repaired signal and the removed residual."""
    os.makedirs(outdir, exist_ok=True)
    out, glitch = xs.copy(), np.zeros_like(xs)
    t = np.array([-4, -3, -2, 2, 3, 4])
    for c in range(xs.shape[1]):
        x = xs[:, c]
        peaks, _ = find_events(x, zthr)
        y = x.copy()
        for s in peaks:
            if s - 4 < 0 or s + 5 > len(x):
                continue
            c3 = np.polyfit(t, x[s + t], 3)
            for k in (-1, 0, 1):
                y[s + k] = np.polyval(c3, k)
        out[:, c], glitch[:, c] = y, x - y
        e = (x**2).sum()
        print(
            f"  ch{c}: repaired {len(peaks)} events, removed "
            f"{10 * np.log10((glitch[:, c] ** 2).sum() / e + 1e-30):.1f} dB of total energy"
        )
    write_wav_f32(os.path.join(outdir, "repaired.wav"), out, sr)
    write_wav_f32(os.path.join(outdir, "glitch_only.wav"), np.clip(glitch * 8, -1, 1), sr)
    print(f"  wrote {outdir}/repaired.wav and {outdir}/glitch_only.wav (glitch +18 dB)")


# ------------------------------------------------------------------ main


def arg(name, default=None, cast=float):
    if name in sys.argv:
        i = sys.argv.index(name)
        if i + 1 < len(sys.argv):
            return cast(sys.argv[i + 1])
    return default


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if not args or "-h" in sys.argv or "--help" in sys.argv:
        print(__doc__)
        return 1
    path = args[0]
    hp = arg("--hp", 4000.0)
    zthr = arg("--z", 6.0)
    t0, t1 = arg("--from", None), arg("--to", None)
    outdir = arg("--repair", None, str)

    xs, sr = load_wav(path)
    mono = xs.mean(axis=1)

    # --ref: scan the residual against a reference render instead of the signal
    # itself. Everything the source legitimately does once per period - a
    # voice's glottal pulse above all - cancels here, so whatever events survive
    # were added by the render under test.
    # --vs: the same, but for two files that are *not* sample-aligned - two
    # renders of the same material, or one render before and after a change. A
    # constant lag and a constant level are fitted out first, which is exactly
    # what --ref refuses to do and why it could not be pointed at a key.
    vs_path = arg("--vs", None, str)
    if vs_path:
        vs, vsr = load_wav(vs_path)
        if abs(vsr - sr) > 0.5:
            print(f"--vs rate {vsr:.0f} != {sr:.0f}; resample first")
            return 1
        vm = vs.mean(axis=1)
        r, lag, g, rdb, drift = align_to(mono, vm, sr)
        print(f"compared  : {vs_path}")
        print(f"alignment : lag {lag:+d} samples, gain {g:.4f}")
        print(f"residual  : {rdb:.1f} dB below it")
        for lo, hi, db in band_residual(r, vm, sr):
            print(f"    {lo:6.0f}-{hi:6.0f} Hz : {db:7.2f} dB")
        # Three ways this comparison can be meaningless, each with its own tell.
        # Say so loudly: a residual that is really a mismatch reads exactly like
        # a catastrophic defect, and that mistake has been made here before.
        if rdb > -6.0 or g <= 0.0:
            print(
                "  *** these two are not the same signal - the residual is as big as\n"
                "      the reference"
                + (" and the gain fit came out negative" if g <= 0.0 else "")
                + ". Nothing below is a defect;\n"
                "      it is the difference between two unrelated renders. Compare a\n"
                "      key against the same note, not another one."
            )
        elif drift > 6.0:
            print(
                f"  *** the residual grows {drift:.1f} dB from the first third to the\n"
                "      last: the two disagree about *pitch*, so this is drift, not a\n"
                "      defect. Nothing below is meaningful. Compare renders that are\n"
                "      meant to be the same note."
            )
        print(
            "  (a constant lag and level are not defects; what is left is what one\n"
            "   render has and the other does not, with the source's own once-per-\n"
            "   period features - the glottal pulse - cancelled)"
        )
        mono = r
        xs = mono.reshape(-1, 1)

    ref_path = arg("--ref", None, str)
    if ref_path:
        rs, rsr = load_wav(ref_path)
        if abs(rsr - sr) > 0.5:
            print(f"--ref rate {rsr:.0f} != {sr:.0f}; cannot subtract sample-aligned")
            return 1
        rm = rs.mean(axis=1)
        n = min(len(mono), len(rm))
        if len(mono) != len(rm):
            print(f"--ref length {len(rm)} vs {len(mono)}; comparing the first {n}")
        e = float(((mono[:n] - rm[:n]) ** 2).sum())
        p = float((rm[:n] ** 2).sum())
        rdb = 10 * np.log10(e / max(p, 1e-30) + 1e-30)
        print(f"reference : {ref_path}")
        print(f"residual  : {rdb:.1f} dB below the reference")
        if rdb > -6.0:
            print(
                "  *** the two files are not sample-aligned — this residual is phase\n"
                "      drift, not a defect, and the scan below means nothing. Subtract\n"
                "      only signals that are supposed to line up sample for sample:\n"
                "      exact.wav vs source.wav, or the same render before/after a change.\n"
                "      play_*.wav renders a constant period against a vibrato'd source,\n"
                "      so it de-phases by design and can never be subtracted this way."
            )
        mono = mono[:n] - rm[:n]
        xs = xs[:n] - rs[:n] if xs.shape == rs.shape else mono.reshape(-1, 1)
    if t0 is not None or t1 is not None:
        a = int((t0 or 0) * sr)
        b = int(t1 * sr) if t1 is not None else len(mono)
    else:
        a, b = steady_region(mono, sr)
    x = mono[a:b]
    if len(x) < 1024:
        print(f"{path}: analysis window too short ({len(x)} samples)")
        return 1

    peak = np.abs(x).max()
    print(f"file      : {path}")
    print(f"format    : {sr:.0f} Hz, {xs.shape[1]} ch, {len(mono) / sr:.3f} s")
    print(f"analysing : {a / sr:.3f}-{b / sr:.3f} s ({len(x)} samples)")
    print(
        f"peak      : {20 * np.log10(peak + 1e-30):.2f} dBFS"
        + ("   *** CLIPPED ***" if peak >= 0.999 else "")
    )

    f0 = estimate_f0(x, sr)
    if f0 > 0:
        print(f"\n[1] f0 estimate: {f0:.2f} Hz  (period {sr / f0:.3f} samples)")
    else:
        print("\n[1] f0: not periodic enough to estimate")

    print(f"\n[2] 2nd-difference outliers (z > {zthr:g})")
    peaks, z = find_events(x, zthr)
    print(f"  events: {len(peaks)}   ({len(peaks) / (len(x) / sr):.1f} per second)")
    if len(peaks):
        for i in np.argsort(z[peaks - 1])[::-1][:10]:
            s = peaks[i]
            print(
                f"    t={(a + s) / sr:9.6f} s  sample={a + s:<9d} z={z[s - 1]:8.1f}  "
                f"d1={x[s] - x[s - 1]:+.5f}  d2={x[s + 1] - 2 * x[s] + x[s - 1]:+.5f}"
            )
        print("\n[3] recurrence of those events")
        recurrence(peaks, sr, f0)
        event_shape(x, peaks)
    else:
        print("\n[3] recurrence: nothing to correlate")

    print(f"\n[4] HF (>{hp:g} Hz) envelope modulation spectrum")
    depth, peaks_mod = hf_modulation(x, sr, hp)
    print(f"  depth: {depth:.3f} (sd/mean of the high-band envelope)")
    for fr, db in peaks_mod:
        # No "seam" verdict here — a clean harmonic tone peaks at f0 too. The
        # multiples are shown only so [2]'s rate can be matched against them.
        tag = label_rate(fr, f0).replace("   (per-period seam)", "")
        print(f"    {fr:9.2f} Hz   {db:7.2f} dB rel. peak{tag}")
    print("  (rate confirmation for [2] only — periodicity alone peaks at f0)")

    hs = harmonic_split(x, sr, f0)
    if hs:
        inh, prof = hs
        print(f"\n[5] inharmonic energy: {inh:.2f} dB below total")
        for lo, hi, db in prof:
            print(f"    {lo:6.0f}-{hi:6.0f} Hz : {db:7.2f} dB")
        print("    (unreliable if f0 drifts across the window - vibrato smears the")
        print("     harmonic comb and lands real harmonic energy in this bucket)")

    if outdir:
        print(f"\n[6] repair")
        repair(xs, sr, zthr, outdir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
