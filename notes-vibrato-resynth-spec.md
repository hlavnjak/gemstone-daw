# Spec: Vibrato-aware bucketing & resynthesis

Status (2026-06-11): **ALL STAGES DONE and tested** (A, bridge, B, C analysis +
C playback). Vibrato is now both captured in the charts and audible on playback.
Spans two repos — `gemstone-daw` (host segmentation) and `lesynth-fourier`
(analysis + playback) — plus the C ABI bridge.

Done:
- A: `Subtrack.pitch_track` + `freq_at` keep the per-frame contour (host).
- Bridge: `lesynth_fourier_analyze`/`_push_analysis` + `AnalysisJob` carry a
  `contour` (ptr,len) of absolute Hz; null/empty ⇒ flat/legacy. Host builds it
  in `gui/resynth.rs::build_contour` (256 pts) from `freq_at`.
- B: `build_bucket_specs` places period-synchronous boundaries when
  `num_buckets == 0`; `AnalysisResult` gains real `bucket_periods` + `pitch_ratio`.
- C analysis: per-bucket DFT runs at `(h+1)·f_local` from the contour.
- C playback: `SharedParams.bucket_pitch_ratio` (set by `load_analysis`); both
  render loops (`assemble_buffer_for_key` + `compute_buffer_for_key_static`)
  scale each bucket's period by `1/ratio` via `bucket_period()`, gated to
  Analysis mode via `bucket_pitch_ratios()`. Per-bucket `max_h = min(.., p/2)`
  guards aliasing at sharpened pitches.

PLAYBACK PERF FIX (2026-06-11): period-synchronous bucketing initially produced
hundreds of buckets (294 for a 2s D5). Since each bucket renders one period per
note, that bloated every per-note buffer (~10s for the lowest key) and the
async all-keys precompute, so key presses fell back to slow synchronous renders
and the DAW froze in Analysis mode. Fixes in `synth_compute_engine.rs`:
- Cap analysis playback buckets at `ANALYSIS_MAX_PLAYBACK_BUCKETS = 128`
  (`analyze_and_load` passes it as `max_buckets`; period-sync coarsens to fit).
- Hoist the `harmonic_ampl/phase_enabled` mutex locks out of the per-sample
  loop in `assemble_buffer_for_key` (was a lock round-trip per output sample).
Result (debug): buckets 294→128, load 659→289ms, key0 render 1561→660ms; far
faster in the release `.so` the DAW runs. New tests: bucket cap, assembled
chart non-empty/non-silent, instance+static buffers audible across keys, synth
mode ignores stale ratios, contour→playback, and FFI push/claim contour round-trip.

KEY REALIZATION (simpler than the spec feared): this engine renders **exactly
one fundamental cycle per bucket**, so every harmonic completes an integer
number of cycles regardless of the bucket's period length — bucket boundaries
stay phase-aligned automatically. The `carry_phase` stitching the spec worried
about is **not needed**; varying `p_b` alone is click-free. Synth mode is byte-
identical to before (ratios only read in Analysis mode).

PLAYBACK LENGTH + FLOOR REWORK (2026-06-18): superseded the 128-bucket cap.
- The `ANALYSIS_MAX_PLAYBACK_BUCKETS` cap is **gone**. It conflated analysis
  resolution with playback length (1 bucket = 1 rendered period), so a 3.2 s
  note played back as ~0.2 s. Both render loops are unified into one
  `render_key_buffer(target_samples, …)`: `0` ⇒ Synth (one period per bucket,
  legacy, byte-identical); `>0` ⇒ Analysis **"preserve seconds"** — render
  `analysis_duration_secs * sample_rate` samples and pick each one-cycle chunk's
  bucket by its time position. A note then lasts the source's wall-clock
  duration at *every* key (low keys: few long periods; high keys: many short
  ones), so per-key buffers stay bounded and the 88-key precompute is safe with
  no cap. New `SharedParams.sample_rate` + `analysis_duration_secs`.
- Harmonic visibility: the absolute `AMP_FLOOR` zeroed quiet upper harmonics on
  real (quiet) recordings — only ~H7 of a violin survived. Replaced by a grid-
  relative amplitude gate (`AMP_FLOOR_REL` of the whole grid's peak, with abs
  fallback) + a per-bucket `PHASE_REL` gate that leaves weak harmonics phase-0
  (cosine-aligned) so they don't buzz when phases are on. Hard limit: harmonics
  above the file's Nyquist can't be recovered (a 22050 Hz file ⇒ max ~H18 of a
  596 Hz note; H19+ simply aren't in the signal).

## The pivotal constraint (read this first)

`assemble_buffer_for_key` in `lesynth-fourier/src/engine/synth_compute_engine.rs:197`
shows that **playback is fully re-pitched and period-synchronous**:

```rust
let period = piano_periods[key] as usize;     // samples per period of the PLAYED key
for bucket in 0..num_buckets {
    for t in 0..period {                       // each bucket renders EXACTLY ONE period
        sample += amp[n][bucket] * sin(2π·(n+1)·t/period + phase[n][bucket]);
    }
}
```

Two consequences that dictate the whole design:

1. **The analysis `base_freq` is discarded at playback.** Output pitch is whatever
   key is pressed; a bucket becomes one period of *that* key. We can never reproduce
   the source's *absolute* Hz vibrato — only its *relative* contour, transposed onto
   the played note. **→ Store a per-bucket pitch ratio `r[b] = f_local[b] / base_freq`,
   not Hz.**
2. **One bucket already = one period at playback**, but the analyzer currently buckets
   by uniform time with a fixed `base_period×4` window. That mismatch is the gap.
   Making the analyzer period-synchronous *unifies* the two halves.

## Data model changes

### `AnalysisResult` (lesynth-fourier `engine/analysis.rs:64`)

`bucket_periods: Vec<f32>` already exists but is dead (`vec![base_period; buckets]`).
Repurpose as the real per-bucket period in *source* samples, and add the
playback-facing ratio:

```rust
pub struct AnalysisResult {
    pub amplitude: Vec<Vec<f32>>,   // [harmonic][bucket]  (unchanged)
    pub phase:     Vec<Vec<f32>>,   // [harmonic][bucket]  (unchanged)
    pub bucket_periods: Vec<f32>,   // NOW REAL: local period (source samples) per bucket
    pub pitch_ratio:    Vec<f32>,   // NEW: f_local[b] / base_freq, ~1.0 ± a few %
}
```

`pitch_ratio` survives to playback; `bucket_periods` is for plotting/inspection.

## Stage A — keep the per-frame pitch contour (host side)   ← STARTED HERE

Today `gemstone-daw/src/analysis/segmentation.rs` collapses every frame to one
median (`base_freq`). Stop discarding the contour.

`Subtrack` gains:

```rust
pub base_freq: f32,                  // keep: median, the transpose reference
pub pitch_track: Vec<(usize, f32)>,  // NEW: (sample_pos, freq) per voiced frame in the run
```

The merge loop already computes `run_freqs` per frame — keep their frame positions
alongside and stash them into `pitch_track` at `flush`. Zero new DSP. Add a
`freq_at(sample_pos) -> f32` interpolation helper for Stages B/C to consume.

## Stage B — period-synchronous bucket boundaries

Replace uniform `bucket_hop = len/buckets` + fixed window with boundaries walked by
the local period:

```
pos = 0
while pos < len:
    p_local = sample_rate / f0(pos)        // from the contour
    next = pos + round(p_local * PERIODS_PER_BUCKET)
    push boundary; pos = next
```

`PERIODS_PER_BUCKET` (≈1–4) replaces the global bucket *count*. Each bucket's window
is centred on its span and sized to its own `p_local`. Set
`bucket_periods[b]`/`pitch_ratio[b]` from the local period.

## Stage C — per-bucket frequency in the DFT

In `analyze_subtrack`, change the harmonic frequency from global to per-bucket:

```rust
let f = (h + 1) as f32 * base_freq;                   // before
let f = (h + 1) as f32 * base_freq * pitch_ratio[b];  // after
```

Window/DFT/phase-reference unchanged.

## Bridge (C ABI) changes

Both FFI entry points in `lesynth-fourier/src/lib.rs` carry the contour.
**Option 1 (recommended): host passes the ratio array in.**

```rust
unsafe extern "C" fn lesynth_fourier_analyze(
    samples, len, sample_rate, base_freq,
    pitch_ratio: *const f32, pitch_ratio_len: usize,   // NEW; null ⇒ flat (legacy)
    num_buckets, num_harmonics,
    out_amp, out_phase,
    out_periods: *mut f32,                              // NEW optional: per-bucket period out
) -> i64
```

`push_analysis` similarly gains `pitch_ratio`. `null` ⇒ current behaviour exactly →
backward compatible. Option 2 (plugin re-derives the tracker) duplicates work; prefer
Option 1.

## Playback — turning the ratio into audible vibrato

Only non-trivial new synthesis code, in `assemble_buffer_for_key`. Scale per-bucket
period by the ratio and stitch phase across boundaries:

```rust
let base_period = piano_periods[key] as f32;
let mut carry_phase = 0.0f32;
for bucket in 0..num_buckets {
    let p_b = (base_period / pitch_ratio[bucket]).round().max(2.0) as usize;
    for t in 0..p_b {
        for n in 0..max_harmonic {
            sample += amp[n][bucket]
                * (carry_phase*(n+1) + 2π*(n+1)*t/p_b + phase[n][bucket]).sin();
        }
    }
    carry_phase += 2π * (p_b as f32 / base_period);   // keep fundamental phase continuous
}
```

- `period_b = base_period / r[b]` → the audible, transposed vibrato.
- **Phase continuity** is the crux: varying `p_b` clicks per bucket unless a running
  phase is carried. Highest-risk part.
- Anti-aliasing: recompute `max_harmonic_for_key` against the *local* (sharpened)
  pitch at vibrato peaks.

## Backward compatibility & fallback

- `pitch_ratio == null` / all-ones ⇒ byte-identical to today. Ship A–C behind that.
- Add a `vibrato_resynth` toggle (host UI + param) to A/B flat vs. contoured by ear.
- The stale doc comment at `analysis.rs:25-28` (claims per-bucket local periods)
  becomes true — update it and the `resynth-architecture` memory.

## Testing

1. Synthetic vibrato `f0 = 220·(1 + 0.03·sin(2π·5t))`: recovered `pitch_ratio` tracks
   `1 ± 0.03` at ~5 Hz; H1 amplitude stays flat (no vibrato→amplitude leak).
2. Phase continuity: held note, assert no sample-to-sample jump > threshold at bucket
   boundaries.
3. Legacy parity: `pitch_ratio = None` reproduces current grids bit-for-bit.
4. Existing `segmentation.rs` tests stay green.

## Effort / risk

| Part | Effort | Risk |
|------|--------|------|
| A — keep contour | low | low (data already computed) |
| B — period-sync buckets | medium | medium (boundary edge cases at run ends) |
| C — per-bucket DFT freq | low | low (one-line freq change) |
| Bridge ABI | low–med | low (additive, null-safe) |
| Playback phase-continuous render | medium | **high** (click-free stitching) |
