// Copyright 2026 Jakub Hlavnicka
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//! Closed-loop resynthesis test: does the audio LeSynth Fourier rebuilds from
//! its own grid match the audio it analysed?
//!
//! The pipeline is the real one end to end — host-side segmentation
//! ([`analysis::segment`]), the plugin's period-synchronous analysis
//! (`lesynth_fourier_analyze_full`) and the plugin's own playback renderer
//! (`lesynth_fourier_resynthesize` → `render_key_buffer`) driven at the
//! source's pitch instead of a key. Nothing here re-implements the DSP.
//!
//! This covers the **transposing** renderer, which resamples and cannot be
//! exact; `resynth_exact.rs` and `resynth_device_rate.rs` cover the exact
//! inverse the audition actually plays.
//!
//! # What correlation cannot see
//!
//! Per-period correlation is energy-weighted, so an artefact 25–30 dB down
//! barely moves it while being plainly audible, and it is scale-invariant, so it
//! cannot see level at all. Two real defects hid behind a 0.98 here: playback
//! inheriting `normalize_for_display`'s gain (**18.9 dB hot**, which shoves the
//! source's own masked bow noise forward — exactly the "noisy background" a
//! listener reports), and a bucket-rate modulation that scored identically
//! stepped or blended (0.9863 vs 0.9868) because correlation judges each period
//! independently. Both are fixed and both now have their own test below.
//!
//! # What remains, quantified so it stays visible
//!
//! * **Noise becomes tone.** ~1.5% of the source's power sits outside the
//!   harmonic series; the reconstruction has ~0.5%. Harmonic-to-residual: source
//!   ~18 dB, reconstruction ~23 dB.
//! * **A duller top end.** H11–H18 sit 2–16 dB low, nothing above ~10.7 kHz.
//!
//! Direction matters: at matched level the reconstruction is duller and more
//! tonal than the source, never brighter or noisier. The onset is reported but
//! not asserted — it is not periodic, so no harmonic model reproduces it.

use std::path::{Path, PathBuf};

use gemstone_daw::analysis;
use gemstone_daw::audio::decode_audio_file;
use gemstone_daw::vst::{class_ids, PluginInstance};

/// The plugin's own harmonic count (`lesynth_fourier::constants::NUM_HARMONICS`).
const NUM_HARMONICS: usize = 256;

/// Lead-in skipped before judging steady-state fidelity. The note's attack is
/// not periodic, so no period-stationary harmonic model can reproduce it; it is
/// still measured and reported, just not asserted on.
const ONSET_SECS: f32 = 0.08;

fn internal_plugin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("internal_plugins")
        .join("liblesynth_fourier.so")
}

fn load_plugin() -> PluginInstance {
    let path = internal_plugin_path();
    assert!(
        path.exists(),
        "internal plugin not built: {:?} (run `make build`)",
        path
    );
    PluginInstance::load(&path, Some(&class_ids::FOURIER_SYNTH), None)
        .expect("load internal LeSynth Fourier")
}

/// Normalised cross-correlation of two equal-length slices, in [-1, 1].
fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        ab += a[i] as f64 * b[i] as f64;
        aa += a[i] as f64 * a[i] as f64;
        bb += b[i] as f64 * b[i] as f64;
    }
    let denom = (aa * bb).sqrt();
    if denom < 1e-20 {
        0.0
    } else {
        (ab / denom) as f32
    }
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / x.len() as f64).sqrt() as f32
}

/// Best correlation of `chunk` against `src` near `start`: every whole-sample
/// shift within a period, then a parabola through the peak. The interpolation is
/// what makes the number meaningful — the optimum falls between samples, and
/// without it even a perfect reconstruction scores ~0.99 from misalignment
/// alone.
fn aligned_correlation(src: &[f32], start: usize, chunk: &[f32]) -> f32 {
    let period = chunk.len();
    let at = |shift: i32| {
        let s = shift.rem_euclid(period as i32) as usize;
        correlation(&src[start + s..start + s + period], chunk)
    };
    let mut best_shift = 0i32;
    let mut best = -2.0f32;
    for shift in 0..period as i32 {
        let c = at(shift);
        if c > best {
            best = c;
            best_shift = shift;
        }
    }
    let (y0, y2) = (at(best_shift - 1), at(best_shift + 1));
    let denom = y0 - 2.0 * best + y2;
    if denom.abs() > 1e-9 {
        let delta = 0.5 * (y0 - y2) / denom;
        (best - 0.25 * (y0 - y2) * delta).clamp(-1.0, 1.0)
    } else {
        best
    }
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    sorted[((sorted.len() - 1) as f32 * p).round() as usize]
}

fn db_power(x: f64) -> f64 {
    10.0 * x.max(1e-20).log10()
}

/// Harmonics measurable below Nyquist at this subtrack's pitch — also the cap
/// `render_key_buffer` applies (`period / 2`).
const MEASURED_HARMONICS: usize = 18;

/// Amplitudes at `k · f0` plus the window's total power, from a 6-period Hann
/// window centred on `center`, applied to *both* signals so the comparison is
/// symmetric.
///
/// Measuring at a supplied `f0` is what keeps it honest: the source's harmonics
/// wander with its vibrato, so a fixed-frequency FFT smears them over dozens of
/// bins and reads them 30+ dB under the reconstruction's static lines — an
/// artefact, not a finding.
fn harmonic_spectrum(x: &[f32], sr: f32, center: usize, f0: f32) -> Option<(Vec<f64>, f64)> {
    let win = (sr / f0 * 6.0).round() as usize;
    if win < 8 || center < win / 2 || center + win / 2 >= x.len() {
        return None;
    }
    let start = center - win / 2;
    let w: Vec<f64> = (0..win)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / win as f64).cos())
        .collect();
    let wsum: f64 = w.iter().sum();
    let w2: f64 = w.iter().map(|v| v * v).sum();
    let total: f64 = (0..win)
        .map(|i| {
            let s = x[start + i] as f64 * w[i];
            s * s
        })
        .sum();

    let mut mags = vec![0.0f64; MEASURED_HARMONICS];
    for h in 1..=MEASURED_HARMONICS {
        let f = h as f64 * f0 as f64;
        if f >= sr as f64 * 0.5 {
            break;
        }
        let om = 2.0 * std::f64::consts::PI * f / sr as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for i in 0..win {
            let s = w[i] * x[start + i] as f64;
            let th = om * (start + i) as f64;
            re += s * th.cos();
            im -= s * th.sin();
        }
        mags[h - 1] = 2.0 / wsum * (re * re + im * im).sqrt();
    }
    // `2·total/Σw²` puts the window's total power in the same units as `mags²`
    // (a unit sine reads 1.0 in both), so harmonic-vs-total is a fair ratio.
    Some((mags, 2.0 * total / w2))
}

/// The differences correlation is blind to: absolute level, per-harmonic
/// spectrum, harmonic-vs-noise balance, and how much of the vibrato the renderer
/// can actually reproduce. See the module docs — several of these are known
/// limitations, asserted at their measured values so they stay visible and any
/// change (improvement or regression) forces this test to be revisited.
#[test]
fn d5_resynthesis_spectral_fidelity() {
    let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("D5.wav");
    if !wav.exists() {
        eprintln!("skipping: {wav:?} not present");
        return;
    }
    let plugin = load_plugin();
    let audio = decode_audio_file(&wav).expect("decode D5.wav");
    let sub = analysis::segment(&audio.samples, audio.sample_rate)
        .into_iter()
        .find(|s| s.is_reasonable(audio.sample_rate))
        .expect("one reasonable subtrack");
    let end = sub.end.min(audio.samples.len());
    let original = &audio.samples[sub.start..end];
    let contour = analysis::build_contour(&sub);
    let grid = plugin
        .analyze_full(
            original,
            audio.sample_rate,
            sub.base_freq,
            &contour,
            0,
            NUM_HARMONICS,
        )
        .expect("analyze_full");
    let sr = audio.sample_rate;
    let base_period = sr / sub.base_freq;
    let recon = plugin
        .resynthesize(&grid, base_period, 0, original.len(), true)
        .expect("resynthesize");

    // Which period each rendered chunk actually used, so the reconstruction is
    // measured at the pitch it was really rendered at, chunk by chunk.
    let mut chunks: Vec<(usize, f32)> = Vec::new(); // (start sample, period)
    let mut at = 0.0f64;
    for b in 0..grid.num_buckets {
        let period = grid.rendered_period(base_period, b);
        chunks.push((at as usize, period));
        at += period as f64;
    }
    let period_at = |p: usize| {
        let i = chunks.partition_point(|&(s, _)| s <= p).saturating_sub(1);
        chunks[i].1
    };

    // ── Level ───────────────────────────────────────────────────────────────
    // The one number correlation can never report, and the one that was wrong:
    // the reconstruction must come back at the source's own absolute level, so a
    // listener can A/B it against the file without matching gains by hand.
    let n = original.len().min(recon.len());
    let level_db = 20.0
        * (rms(&recon[..n]).max(1e-9) as f64 / rms(&original[..n]).max(1e-9) as f64).log10();
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    println!(
        "level: reconstruction is {level_db:+.2} dB relative to the source \
         (peaks {:.4} vs {:.4}, display gain {:.2}x)",
        peak(&recon[..n]),
        peak(&original[..n]),
        grid.display_gain
    );
    assert!(
        level_db.abs() < 0.5,
        "reconstruction plays {level_db:+.2} dB off the source. It used to be \
         +18.9 dB: `normalize_for_display(0.9)` scales the grid for chart \
         legibility and the render inherited it. `analyze_full` reports that \
         gain and `resynthesize(.., restore_source_level = true)` divides it \
         back out — check that path before relaxing this bound."
    );
    // The display gain has to be a real, non-trivial number here, or the
    // assertion above would pass for the wrong reason (nothing to undo).
    assert!(
        grid.display_gain > 5.0,
        "display gain {} — this quiet source should need a large one",
        grid.display_gain
    );
    // And the un-restored render must still be the hot one: this is what a piano
    // key plays, and it is what the audition used to play.
    let raw = plugin
        .resynthesize(&grid, base_period, 0, original.len(), false)
        .expect("resynthesize");
    let raw_db =
        20.0 * (rms(&raw[..n]).max(1e-9) as f64 / rms(&original[..n]).max(1e-9) as f64).log10();
    println!("(un-restored render, for reference: {raw_db:+.1} dB)");
    assert!(
        raw_db > 6.0,
        "un-restored render is only {raw_db:+.1} dB hot — the level restore is \
         no longer doing anything and this test has stopped proving it"
    );

    // ── Per-harmonic spectrum, vibrato-aware ────────────────────────────────
    let step = (base_period * 8.0) as usize;
    let from = (0.15 * sr) as usize;
    let to = original.len().saturating_sub(step);
    let mut acc_o = vec![0.0f64; MEASURED_HARMONICS];
    let mut acc_r = vec![0.0f64; MEASURED_HARMONICS];
    let (mut tot_o, mut tot_r) = (0.0f64, 0.0f64);
    let (mut harm_o, mut harm_r) = (0.0f64, 0.0f64);
    let mut frames = 0usize;
    let mut p = from;
    while p < to {
        let f_src = sub.freq_at(sub.start + p);
        let f_rec = sr / period_at(p);
        if let (Some((mo, to_)), Some((mr, tr))) = (
            harmonic_spectrum(original, sr, p, f_src),
            harmonic_spectrum(&recon, sr, p, f_rec),
        ) {
            for h in 0..MEASURED_HARMONICS {
                acc_o[h] += mo[h] * mo[h];
                acc_r[h] += mr[h] * mr[h];
                harm_o += mo[h] * mo[h];
                harm_r += mr[h] * mr[h];
            }
            tot_o += to_;
            tot_r += tr;
            frames += 1;
        }
        p += step;
    }
    assert!(frames > 50, "too few frames to average: {frames}");

    // Compare shape after matching the fundamental, so a level error and a
    // timbre error can't cancel or masquerade as each other. That match is now
    // nearly a no-op — assert it, or this table would keep looking perfect even
    // if the absolute level drifted again.
    let gain = acc_o[0] / acc_r[0];
    assert!(
        (10.0 * gain.log10()).abs() < 1.0,
        "matching H1 needed {:+.2} dB — the render is no longer at the source's \
         level even though the fundamental's shape matches",
        10.0 * gain.log10()
    );
    println!("\n{frames} frames averaged; harmonic levels after matching H1:");
    println!("   h     source dB   recon dB    delta");
    let mut worst_low = 0.0f64;
    for h in 0..MEASURED_HARMONICS {
        let (o, r) = (acc_o[h] / frames as f64, acc_r[h] / frames as f64 * gain);
        if o <= 0.0 && r <= 0.0 {
            continue;
        }
        let delta = db_power(r) - db_power(o);
        println!(
            "  {:2}    {:9.2}   {:8.2}   {:+7.2}",
            h + 1,
            db_power(o),
            db_power(r),
            delta
        );
        // The harmonics that carry the timbre must be reproduced at the right
        // level; the top few are allowed to roll off (and are reported).
        if h < 10 {
            assert!(
                delta.abs() < 2.0,
                "H{} is {:+.2} dB off — the reconstruction's spectrum no longer \
                 matches the source's",
                h + 1,
                delta
            );
        }
        worst_low = worst_low.min(delta);
    }
    println!("worst roll-off across H1..H18: {worst_low:.2} dB");

    // ── Harmonic vs noise: the "sines only" limitation, quantified ───────────
    let hnr = |h: f64, t: f64| db_power(h / (t - h).max(1e-12));
    let (hnr_o, hnr_r) = (hnr(harm_o, tot_o), hnr(harm_r, tot_r));
    println!(
        "\nharmonic share of power: source {:.1}%, recon {:.1}%",
        100.0 * (harm_o / tot_o).min(1.0),
        100.0 * (harm_r / tot_r).min(1.0)
    );
    println!("harmonic-to-residual: source {hnr_o:.1} dB, recon {hnr_r:.1} dB");
    // A sines-only model can only ever be *more* tonal than its source. The bar
    // is that it must not invent broadband content — i.e. never be less tonal.
    assert!(
        hnr_r > hnr_o - 1.0,
        "the reconstruction is less tonal than the source ({hnr_r:.1} vs \
         {hnr_o:.1} dB) — it is adding broadband noise, which additive \
         synthesis cannot legitimately do"
    );
    // And it must still be recognisably the same signal, not a bare sine stack:
    // the source's own residual is what it is, but the harmonic series must
    // account for the bulk of both.
    assert!(
        harm_o / tot_o > 0.9 && harm_r / tot_r > 0.9,
        "harmonics 1..{MEASURED_HARMONICS} should explain most of both signals"
    );

    // ── Vibrato the renderer actually reproduces ────────────────────────────
    let cents = |lo: f32, hi: f32| 1200.0 * (hi / lo).log2();
    let (mut src_lo, mut src_hi) = (f32::MAX, 0.0f32);
    let (mut ren_lo, mut ren_hi) = (f32::MAX, 0.0f32);
    let mut worst_cents = 0.0f32;
    for b in 0..grid.num_buckets {
        let want = sub.base_freq * grid.pitch_ratio[b];
        let got = sr / grid.rendered_period(base_period, b);
        src_lo = src_lo.min(want);
        src_hi = src_hi.max(want);
        ren_lo = ren_lo.min(got);
        ren_hi = ren_hi.max(got);
        let e = cents(want, got);
        if e.abs() > worst_cents.abs() {
            worst_cents = e;
        }
    }
    println!(
        "\nsource pitch   {:.1}..{:.1} Hz ({:.0} cents of vibrato)",
        src_lo,
        src_hi,
        cents(src_lo, src_hi)
    );
    println!(
        "rendered pitch {:.1}..{:.1} Hz ({:.0} cents); worst error {:+.2} cents",
        ren_lo,
        ren_hi,
        cents(ren_lo, ren_hi),
        worst_cents
    );
    // The contour must be continuous — the sub-sample autocorrelation refinement
    // doing its job. Without it the tracker could only report whole-sample lags
    // (46 cents apart at this pitch) and the contour would collapse onto two values.
    let distinct_src = grid
        .pitch_ratio
        .iter()
        .map(|r| (r * 10_000.0) as i32)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    assert!(
        distinct_src > 20,
        "the pitch contour has only {distinct_src} distinct values — sub-sample \
         autocorrelation refinement has regressed and vibrato is being quantised \
         at the tracker"
    );
    // And the renderer must follow it. This is what the fractional phase
    // accumulator bought: the vibrato reaching playback used to collapse onto two
    // whole-sample periods 46 cents apart, with 97% of buckets on one of them.
    assert!(
        cents(ren_lo, ren_hi) > 0.8 * cents(src_lo, src_hi),
        "the render spans only {:.0} cents where the source spans {:.0} — vibrato \
         is being flattened",
        cents(ren_lo, ren_hi),
        cents(src_lo, src_hi)
    );
}

/// A signal the model represents exactly — a stationary harmonic stack whose
/// period is a whole number of samples — must survive analysis + resynthesis
/// unchanged. This isolates the DSP logic (phase convention, inverse-FFT render
/// path, per-bucket normalisation) from anything to do with real, noisy source
/// material: if this drops, the round trip is genuinely broken.
#[test]
fn exactly_representable_signal_round_trips() {
    let plugin = load_plugin();

    let sr = 22_050.0f32;
    let period = 37usize;
    let f0 = sr / period as f32;
    let n = sr as usize;
    let amps = [1.0f32, 0.5, 0.3, 0.2, 0.12, 0.07];
    let phases = [0.3f32, 1.1, 2.7, 0.0, 4.2, 5.5];
    let sig: Vec<f32> = (0..n)
        .map(|i| {
            let x = 2.0 * std::f32::consts::PI * f0 * i as f32 / sr;
            0.3 * (0..amps.len())
                .map(|h| amps[h] * ((h as f32 + 1.0) * x + phases[h]).sin())
                .sum::<f32>()
        })
        .collect();

    let grid = plugin
        .analyze_full(&sig, sr, f0, &[], 0, NUM_HARMONICS)
        .expect("analyze_full");
    let mid = grid.num_buckets / 2;

    // The analysis must recover the harmonic *ratios* exactly (the grid as a
    // whole is scaled for the charts) and each harmonic's **absolute** phase —
    // the angle at the bucket's own first sample. Phase used to be stored
    // relative to the fundamental (`ψ_k − k·ψ_1`), which discarded ψ₁ and with
    // it the time origin the inverse transform needs; a one-period bucket makes
    // absolute phase continuous across buckets on its own, so nothing is gained
    // by encoding it relatively and everything is lost.
    let h1 = grid.amp(0, mid);
    assert!(h1 > 0.1, "fundamental not detected: {h1}");
    let two_pi = 2.0 * std::f32::consts::PI;
    for (h, &a) in amps.iter().enumerate() {
        let got = grid.amp(h, mid) / h1;
        assert!(
            (got - a / amps[0]).abs() < 0.01,
            "H{} amplitude ratio {:.4}, want {:.4}",
            h + 1,
            got,
            a / amps[0]
        );
        let want = phases[h].rem_euclid(two_pi);
        let d = (grid.phase(h, mid) - want).rem_euclid(two_pi);
        assert!(
            d.min(two_pi - d) < 0.01,
            "H{} phase {:.4}, want {:.4}",
            h + 1,
            grid.phase(h, mid),
            want
        );
    }
    // Harmonics that aren't in the source read at the transform's noise floor.
    // (This asserted an exact 0.0 while an amplitude gate rounded them down; the
    // gate is gone, because rounding is what made the transform non-invertible.)
    assert!(
        grid.amp(amps.len(), mid) < 1e-3 * h1,
        "H{} leaked: {}",
        amps.len() + 1,
        grid.amp(amps.len(), mid)
    );

    let recon = plugin
        .resynthesize(&grid, period as f32, 0, sig.len(), true)
        .expect("resynthesize");
    assert!(recon.len() >= sig.len());

    // Compare a long stretch out of the middle, on the same time base.
    let start = n / 2;
    let span = period * 100;
    let corr = aligned_correlation(&sig, start, &recon[start..start + span]);
    // Correlation is scale invariant, so check the amplitude separately: on a
    // signal the model represents exactly, the round trip must return the same
    // *numbers*, not merely the same shape.
    let level_db = 20.0
        * (rms(&recon[start..start + span]).max(1e-9) as f64
            / rms(&sig[start..start + span]).max(1e-9) as f64)
            .log10();
    println!(
        "exactly representable: aligned correlation {corr:.6}, level {level_db:+.3} dB"
    );
    assert!(
        level_db.abs() < 0.2,
        "exact round trip came back {level_db:+.3} dB off — the level restore is \
         wrong even where the model is lossless"
    );
    assert!(
        corr > 0.999,
        "a signal the model represents exactly must round-trip; got {corr}"
    );
}

#[test]
fn d5_resynthesis_reproduces_the_analysed_subtrack() {
    let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("D5.wav");
    if !wav.exists() {
        eprintln!("skipping: {wav:?} not present");
        return;
    }
    let plugin = load_plugin();

    // ── 1) Autocorrelation segmentation → exactly one analysable subtrack ────
    let audio = decode_audio_file(&wav).expect("decode D5.wav");
    let subs = analysis::segment(&audio.samples, audio.sample_rate);
    let reasonable: Vec<_> = subs
        .iter()
        .filter(|s| s.is_reasonable(audio.sample_rate))
        .collect();
    println!(
        "D5.wav: {:.3}s @ {} Hz -> {} subtrack(s), {} reasonable",
        audio.duration_secs(),
        audio.sample_rate,
        subs.len(),
        reasonable.len()
    );
    assert_eq!(
        reasonable.len(),
        1,
        "D5.wav must segment into exactly one reasonable subtrack"
    );
    let sub = reasonable[0];
    let end = sub.end.min(audio.samples.len());
    let original = &audio.samples[sub.start..end];
    println!(
        "subtrack: samples {}..{} ({} = {:.3}s), base {:.2} Hz, conf {:.3}",
        sub.start,
        end,
        original.len(),
        sub.duration_secs(audio.sample_rate),
        sub.base_freq,
        sub.confidence
    );
    // It really is a D5 (587.33 Hz), within the ~semitone the merge tolerance
    // allows the median to wander.
    assert!(
        (sub.base_freq / 587.33).log2().abs() < 1.0 / 12.0,
        "detected fundamental {:.2} Hz is not a D5",
        sub.base_freq
    );

    // ── 2) Amps, phases and per-bucket pitch, from the plugin ────────────────
    let contour = analysis::build_contour(sub);
    let grid = plugin
        .analyze_full(
            original,
            audio.sample_rate,
            sub.base_freq,
            &contour,
            0, // period-synchronous — exactly how the plugin analyses a pushed subtrack
            NUM_HARMONICS,
        )
        .expect("plugin analyze_full");
    println!(
        "grid: {} harmonics x {} buckets, bucket period {:.2}..{:.2} samples",
        grid.num_harmonics,
        grid.num_buckets,
        grid.bucket_periods.iter().cloned().fold(f32::MAX, f32::min),
        grid.bucket_periods.iter().cloned().fold(0.0, f32::max),
    );
    assert!(grid.num_buckets > 100, "grid too coarse: {}", grid.num_buckets);
    assert!(
        grid.pitch_ratio.iter().all(|&r| (0.5..2.0).contains(&r)),
        "pitch contour left the subtrack's own octave"
    );

    // ── 3) Rebuild at the *original* pitch through the plugin's playback path ─
    let base_period = audio.sample_rate / sub.base_freq;
    let recon = plugin
        .resynthesize(&grid, base_period, 0, original.len(), true)
        .expect("plugin resynthesize");
    println!(
        "reconstruction: {} samples (source {}), base_period {} = {:.2} Hz",
        recon.len(),
        original.len(),
        base_period,
        audio.sample_rate / base_period as f32
    );
    // "Preserve seconds": the note lasts the source's duration, to within the
    // final period the renderer cannot cut short.
    assert!(
        recon.len() >= original.len()
            && recon.len() < original.len() + 2 * base_period.ceil() as usize,
        "reconstruction length {} does not match source {}",
        recon.len(),
        original.len()
    );

    // ── 4) Compare, sample by sample ────────────────────────────────────────
    let n = original.len().min(recon.len());
    let raw = correlation(&original[..n], &recon[..n]);
    println!(
        "whole-buffer: orig rms {:.5}, recon rms {:.5}, raw correlation {:.4}",
        rms(&original[..n]),
        rms(&recon[..n]),
        raw
    );
    // This used to be ~0: with the rendered period rounded to whole samples the
    // reconstruction drifted out of phase with the source within a few cycles, so
    // an un-aligned whole-buffer correlation carried no information at all. The
    // fractional phase accumulator tracks the source's pitch closely enough that
    // the *whole 3-second note* now stays substantially phase-locked with no
    // alignment whatsoever — which is the strongest single piece of evidence that
    // the render is in tune. (It cannot reach 1.0: the format stores phase
    // relative to the fundamental, so the note's overall time origin is still
    // free, and the contour is an estimate.)
    assert!(
        raw > 0.5,
        "whole-buffer correlation is {raw} — the reconstruction has come unstuck \
         from the source's phase, which means the rendered pitch is drifting again"
    );

    let mut steady = Vec::new(); // per-period correlation, steady state
    let mut steady_selfsim = Vec::new(); // the source's own predictability there
    let mut onset = Vec::new();
    let mut env_orig = Vec::new();
    let mut env_recon = Vec::new();
    let mut worse_than_source = 0usize;

    let mut pos_f = 0.0f64;
    for b in 0..grid.num_buckets {
        let period_f = grid.rendered_period(base_period, b);
        let pos = pos_f as usize;
        let period = period_f.round() as usize;
        // The render is time-driven across exactly `original.len()` samples, so
        // the cycle written at `pos` reproduces the source at `pos`. (Anchoring on
        // the *bucket's* centre instead would be off by up to half a bucket — one
        // bucket feeds several rendered cycles.) `pos_f` accumulates the
        // fractional period, since cycles no longer land on whole samples.
        if pos + period > recon.len() || pos + 2 * period >= original.len() {
            pos_f += period_f as f64;
            continue;
        }
        let chunk = &recon[pos..pos + period];
        let corr = aligned_correlation(original, pos, chunk);
        // Ceiling: how well the source predicts *itself* one period on. No
        // period-stationary harmonic model can do better than this.
        let selfsim = correlation(
            &original[pos..pos + period],
            &original[pos + period..pos + 2 * period],
        );

        if (pos as f32) < ONSET_SECS * audio.sample_rate {
            onset.push(corr);
        } else {
            steady.push(corr);
            steady_selfsim.push(selfsim);
            if corr < selfsim - 0.15 {
                worse_than_source += 1;
            }
            env_orig.push(rms(&original[pos..pos + period]));
            env_recon.push(rms(chunk));
        }
        pos_f += period_f as f64;
    }
    assert!(steady.len() > 100, "too few steady-state periods to judge");

    steady.sort_by(|a, b| a.partial_cmp(b).unwrap());
    steady_selfsim.sort_by(|a, b| a.partial_cmp(b).unwrap());
    onset.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let envelope = correlation(&env_orig, &env_recon);

    println!(
        "steady-state per-period correlation (n={}): p05 {:.4} p25 {:.4} median {:.4} mean {:.4}",
        steady.len(),
        percentile(&steady, 0.05),
        percentile(&steady, 0.25),
        percentile(&steady, 0.50),
        steady.iter().sum::<f32>() / steady.len() as f32
    );
    println!(
        "source self-similarity there  : p05 {:.4} p25 {:.4} median {:.4}",
        percentile(&steady_selfsim, 0.05),
        percentile(&steady_selfsim, 0.25),
        percentile(&steady_selfsim, 0.50),
    );
    println!(
        "onset (<{:.0} ms, reported only): median {:.4}",
        ONSET_SECS * 1000.0,
        percentile(&onset, 0.50)
    );
    println!("envelope correlation: {envelope:.4}");
    println!(
        "periods materially worse than the source's own predictability: {}/{}",
        worse_than_source,
        steady.len()
    );

    // The waveform itself: each rendered period must reproduce the source period
    // it stands for.
    let median = percentile(&steady, 0.50);
    let p25 = percentile(&steady, 0.25);
    assert!(
        median > 0.95,
        "median per-period correlation {median:.4} — the reconstructed waveform no \
         longer matches the source"
    );
    assert!(p25 > 0.90, "lower-quartile per-period correlation {p25:.4}");
    // The real bar: the model must not lose anything the source actually
    // carries. It may only fail where the source itself stops being periodic.
    let ceiling = percentile(&steady_selfsim, 0.50);
    assert!(
        median >= ceiling - 0.01,
        "reconstruction ({median:.4}) is worse than the source's own period-to-period \
         predictability ({ceiling:.4}) — the pipeline is discarding real content"
    );
    assert!(
        (worse_than_source as f32) < 0.05 * steady.len() as f32,
        "{worse_than_source} of {} periods are materially worse than the source's own \
         predictability",
        steady.len()
    );
    // The amplitude envelope must survive the per-bucket normalisation.
    assert!(
        envelope > 0.98,
        "envelope correlation {envelope:.4} — loudness contour was not preserved"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Strict sample-by-sample diff
//
// The question this section answers is "if the amps and phases are right, why
// don't the samples match?" — by rebuilding the signal four ways and diffing
// each against the source, so the cost of every lossy stage is a number.
// ───────────────────────────────────────────────────────────────────────────

const PI64: f64 = std::f64::consts::PI;

/// Single-bin DFT at `k · f0` over a Hann window of 6 periods centred on
/// `center`, keeping the **absolute** phase (sin convention, referenced to the
/// absolute sample index) rather than the fundamental-relative phase the plugin
/// stores. Returns `(amp, phase)` per harmonic.
fn analyse_absolute(x: &[f32], sr: f64, center: usize, f0: f64) -> Option<(Vec<f64>, Vec<f64>)> {
    let win = (sr / f0 * 6.0).round() as usize;
    if win < 8 || center < win / 2 || center + win / 2 >= x.len() {
        return None;
    }
    let start = center - win / 2;
    let w: Vec<f64> = (0..win)
        .map(|i| 0.5 - 0.5 * (2.0 * PI64 * i as f64 / win as f64).cos())
        .collect();
    let wsum: f64 = w.iter().sum();
    let mut amp = vec![0.0; MEASURED_HARMONICS];
    let mut ph = vec![0.0; MEASURED_HARMONICS];
    for h in 1..=MEASURED_HARMONICS {
        let f = h as f64 * f0;
        if f >= sr * 0.5 {
            break;
        }
        let om = 2.0 * PI64 * f / sr;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for i in 0..win {
            let s = w[i] * x[start + i] as f64;
            let th = om * (start + i) as f64;
            re += s * th.cos();
            im -= s * th.sin();
        }
        amp[h - 1] = 2.0 / wsum * (re * re + im * im).sqrt();
        ph[h - 1] = im.atan2(re) + 0.5 * PI64;
    }
    Some((amp, ph))
}

/// Largest absolute sample deviation over `[from, to)`, as a percentage of
/// `peak`. `shift` slides the reconstruction against the source.
fn max_dev_pct(src: &[f32], rec: &[f64], from: usize, to: usize, shift: usize, peak: f64) -> f64 {
    let mut m = 0.0f64;
    for i in from..to {
        let j = i - from + shift;
        if j >= rec.len() {
            break;
        }
        m = m.max((src[i] as f64 - rec[j]).abs());
    }
    100.0 * m / peak
}

/// Sample-by-sample diff, stage by stage, so the flaw is localised rather than
/// averaged away:
///
/// * **1** — absolute phase at the true fractional pitch: ~7% of peak, and that
///   is a floor, not numerical error. ~1.3% of the source's power lies *between*
///   the harmonics as bow noise, which an 18-harmonic model cannot represent.
/// * **2** — round the period to whole samples, as `bucket_period` once did:
///   ~7% → ~72%. **The defect**, and a tuning error (595.9 Hz played against a
///   590.7 Hz source, with 46-cent jumps).
/// * **3** — phase relative to the fundamental, each period restarted at t = 0.
///   Cheap on top of stage 2, but it is why no global alignment can make the
///   whole note match: the time origin is given away once per period.
/// * **4** — the plugin's real output, per period with local alignment.
#[test]
fn d5_strict_sample_diff_localises_the_flaw() {
    let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("D5.wav");
    if !wav.exists() {
        eprintln!("skipping: {wav:?} not present");
        return;
    }
    let plugin = load_plugin();
    let audio = decode_audio_file(&wav).expect("decode D5.wav");
    let sub = analysis::segment(&audio.samples, audio.sample_rate)
        .into_iter()
        .find(|s| s.is_reasonable(audio.sample_rate))
        .expect("one reasonable subtrack");
    let end = sub.end.min(audio.samples.len());
    let original = &audio.samples[sub.start..end];
    let sr = audio.sample_rate as f64;
    let peak = original.iter().fold(0.0f32, |m, &v| m.max(v.abs())) as f64;
    assert!(peak > 0.01, "source is silent");

    let step = (sr / sub.base_freq as f64 * 4.0) as usize;
    let from = (0.15 * sr) as usize;
    let (mut s1, mut s2, mut s3) = (Vec::new(), Vec::new(), Vec::new());

    let mut pos = from;
    while pos + 2 * step < original.len() {
        let f0 = sub.freq_at(sub.start + pos) as f64;
        let p = (sr / f0) as usize;
        if let Some((amp, ph)) = analyse_absolute(original, sr, pos, f0) {
            let (lo, hi) = (pos - p / 2, pos + p / 2);
            let render = |freq: f64| -> Vec<f64> {
                (lo..hi)
                    .map(|n| {
                        (0..MEASURED_HARMONICS)
                            .map(|h| {
                                let om = 2.0 * PI64 * (h as f64 + 1.0) * freq / sr;
                                amp[h] * (om * n as f64 + ph[h]).sin()
                            })
                            .sum()
                    })
                    .collect()
            };
            // 1) True fractional pitch, absolute phase.
            s1.push(max_dev_pct(original, &render(f0), lo, hi, 0, peak));
            // 2) Only change: period rounded to whole samples.
            s2.push(max_dev_pct(original, &render(sr / (sr / f0).round()), lo, hi, 0, peak));
            // 3) Also: relative phase, each period restarted at t = 0.
            let rel: Vec<f64> = (0..MEASURED_HARMONICS)
                .map(|h| (ph[h] - (h as f64 + 1.0) * ph[0]).rem_euclid(2.0 * PI64))
                .collect();
            let restarted: Vec<f64> = (0..hi - lo)
                .map(|t| {
                    (0..MEASURED_HARMONICS)
                        .map(|h| {
                            let om = 2.0 * PI64 * (h as f64 + 1.0) / p as f64;
                            amp[h] * (om * t as f64 + rel[h]).sin()
                        })
                        .sum()
                })
                .collect();
            s3.push(max_dev_pct(original, &restarted, lo, hi, 0, peak));
        }
        pos += step;
    }
    for v in [&mut s1, &mut s2, &mut s3] {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    }
    let med = |v: &Vec<f64>| v[v.len() / 2];
    assert!(s1.len() > 100, "too few frames: {}", s1.len());

    println!("max sample deviation, % of source peak (median over {} periods):", s1.len());
    println!("  1  absolute phase, fractional pitch : {:6.1}%   <- model floor", med(&s1));
    println!("  2  + period rounded to whole samples: {:6.1}%   <- THE DEFECT", med(&s2));
    println!("  3  + relative phase, restart at t=0 : {:6.1}%", med(&s3));

    // The information really is in the amps and phases: rebuilt at the true
    // fractional pitch with absolute phase, the samples track the source closely.
    assert!(
        med(&s1) < 12.0,
        "even an ideal reconstruction deviates by {:.1}% — the analysis itself is \
         losing the signal, not just the render",
        med(&s1)
    );
    // Rounding the period is what destroys it — by an order of magnitude.
    assert!(
        med(&s2) > 4.0 * med(&s1),
        "whole-sample period rounding now costs only {:.1}% vs {:.1}% ideal; if \
         fractional-period rendering has landed, retune this test",
        med(&s2),
        med(&s1)
    );

    // Stage 4: the plugin's real output, per period with local alignment.
    let contour = analysis::build_contour(&sub);
    let grid = plugin
        .analyze_full(original, audio.sample_rate, sub.base_freq, &contour, 0, NUM_HARMONICS)
        .expect("analyze_full");
    let base_period = audio.sample_rate / sub.base_freq;
    let recon = plugin
        .resynthesize(&grid, base_period, 0, original.len(), true)
        .expect("resynthesize");
    let n = original.len().min(recon.len());
    // No gain fitting. Stage 4 is a raw sample difference against the source, in
    // absolute units, so a level error shows up as deviation instead of being
    // divided out — fitting a best gain here is precisely how an 18.9 dB offset
    // stayed invisible to a "sample-by-sample" test.
    let fitted_gain = {
        let (mut a, mut b) = (0.0f64, 0.0f64);
        for i in 0..n {
            a += (original[i] as f64).powi(2);
            b += (recon[i] as f64).powi(2);
        }
        (a / b).sqrt()
    };
    println!(
        "     (gain that would best-fit the level: {:.4} = {:+.2} dB)",
        fitted_gain,
        20.0 * fitted_gain.log10()
    );
    assert!(
        (20.0 * fitted_gain.log10()).abs() < 0.5,
        "the render needs a {:+.2} dB correction to sit on the source — it is \
         not at the source's level",
        20.0 * fitted_gain.log10()
    );
    let scaled: Vec<f64> = recon.iter().map(|&v| v as f64).collect();
    let mut s4 = Vec::new();
    let mut pos = from;
    while pos + 2 * step < n {
        let p = (sr / sub.freq_at(sub.start + pos) as f64) as usize;
        if pos + 2 * p >= n {
            break;
        }
        let best = (0..p)
            .map(|sh| max_dev_pct(original, &scaled[pos..], pos, pos + p, sh, peak))
            .fold(f64::MAX, f64::min);
        s4.push(best);
        pos += step;
    }
    s4.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("  4  plugin output, per period, aligned: {:6.1}%", med(&s4));
    assert!(
        med(&s4) < 25.0,
        "plugin per-period deviation {:.1}% — worse than the format's own limit",
        med(&s4)
    );
}

/// Every harmonic must come back at the right **phase**, not just the right
/// level — the third thing this file could not see, and the one that mattered
/// most. Magnitudes, correlation and the per-period diff are all dominated by
/// the strong low harmonics, so a harmonic at exactly the right amplitude and a
/// half-turn out of position passed every check.
///
/// `PHASE_REL = 0.05` did exactly that: it discarded the phase of any harmonic
/// under 5% of its bucket's peak (−26 dB, not remotely silent). H5, H8 and H10
/// came back 71.6°, 116.5° and 125.2° out with amplitudes within 0.5 dB. A
/// harmonic misplaced by 116° contributes error ~1.7× its own amplitude — no
/// click, no change in crest factor, just the wrong waveform every period.
///
/// Relative phase is what the format stores and playback can reproduce; each
/// period's absolute phase is given away by design.
#[test]
fn d5_relative_phase_matches_the_source() {
    let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("D5.wav");
    if !wav.exists() {
        eprintln!("skipping: {wav:?} not present");
        return;
    }
    let plugin = load_plugin();
    let audio = decode_audio_file(&wav).expect("decode D5.wav");
    let sub = analysis::segment(&audio.samples, audio.sample_rate)
        .into_iter()
        .find(|s| s.is_reasonable(audio.sample_rate))
        .expect("one reasonable subtrack");
    let end = sub.end.min(audio.samples.len());
    let original = &audio.samples[sub.start..end];
    let contour = analysis::build_contour(&sub);
    let grid = plugin
        .analyze_full(
            original,
            audio.sample_rate,
            sub.base_freq,
            &contour,
            0,
            NUM_HARMONICS,
        )
        .expect("analyze_full");
    let sr = audio.sample_rate as f64;
    let recon = plugin
        .resynthesize(&grid, audio.sample_rate / sub.base_freq, 0, original.len(), true)
        .expect("resynthesize");

    let step = (sr / sub.base_freq as f64 * 4.0) as usize;
    let from = (0.15 * sr) as usize;
    let to = original.len().saturating_sub(2 * step);
    // How many harmonics carry enough level for a phase error to matter. H11+ on
    // this file sit 39-61 dB under the fundamental and are also amplitude-limited
    // by the analysis window; they are reported, not asserted.
    const JUDGED: usize = 10;
    let mut err = vec![Vec::<f64>::new(); MEASURED_HARMONICS];

    let mut p = from;
    while p < to {
        let f0 = sub.freq_at(sub.start + p) as f64;
        if let (Some((ao, po)), Some((ar, pr))) = (
            analyse_absolute(original, sr, p, f0),
            analyse_absolute(&recon, sr, p, f0),
        ) {
            for k in 0..MEASURED_HARMONICS {
                if ao[k] <= 0.0 || ar[k] <= 0.0 {
                    continue;
                }
                let rel = |ph: &Vec<f64>| ph[k] - (k as f64 + 1.0) * ph[0];
                let mut d = (rel(&pr) - rel(&po)).rem_euclid(2.0 * PI64);
                if d > PI64 {
                    d -= 2.0 * PI64;
                }
                err[k].push(d.abs().to_degrees());
            }
        }
        p += step;
    }

    println!("median relative-phase error per harmonic (degrees):");
    let mut worst = 0.0f64;
    for k in 0..MEASURED_HARMONICS {
        if err[k].len() < 50 {
            continue;
        }
        err[k].sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = err[k][err[k].len() / 2];
        println!(
            "  H{:<2} {:6.1}°{}",
            k + 1,
            med,
            if k < JUDGED { "" } else { "   (reported only)" }
        );
        if k < JUDGED {
            worst = worst.max(med);
        }
    }
    // Gated, H5/H8/H10 sat at 72-125°. Ungated they are all inside 9°.
    assert!(
        worst < 20.0,
        "a harmonic in H1..H{JUDGED} is {worst:.1}° out of phase. Amplitude checks \
         cannot see this — look at the phase gate (`PHASE_REL` in the plugin's \
         analysis) before anything else."
    );
}

/// The reconstruction must not be modulated at the **bucket rate** — the
/// artefact class per-period correlation cannot see, and the reason a "0.98
/// median" kept passing over an audible defect. A bucket used to span ~4 cycles,
/// held flat and stepped at the boundary: a 147.6 Hz staircase, 6% in amplitude
/// typically. Correlation judges each period independently, so it scored the
/// same either way (0.9863 vs 0.9868).
///
/// The artefact is coherent sidebands at `f_h ± bucket_rate`, heard as
/// roughness. The trap: the source has *more* energy at those offsets (−22 to
/// −40 dB), but that is broadband bow noise, not a discrete tone pair, and they
/// sound nothing alike. So this bounds the reconstruction's own sidebands
/// absolutely rather than comparing. A bucket is now one period, so there is no
/// separate rate left to modulate at.
#[test]
fn bucket_rate_modulation_is_no_longer_a_thing() {
    let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("D5.wav");
    if !wav.exists() {
        eprintln!("skipping: {wav:?} not present");
        return;
    }
    let plugin = load_plugin();
    let audio = decode_audio_file(&wav).expect("decode D5.wav");
    let sub = analysis::segment(&audio.samples, audio.sample_rate)
        .into_iter()
        .find(|s| s.is_reasonable(audio.sample_rate))
        .expect("one reasonable subtrack");
    let end = sub.end.min(audio.samples.len());
    let original = &audio.samples[sub.start..end];
    let contour = analysis::build_contour(&sub);
    let grid = plugin
        .analyze_full(
            original,
            audio.sample_rate,
            sub.base_freq,
            &contour,
            0,
            NUM_HARMONICS,
        )
        .expect("analyze_full");

    // One bucket per rendered cycle: the grid changes at exactly the rate the
    // waveform does, so there is no sub-audio rate for it to modulate at. The
    // old test measured sidebands at +/- the bucket rate; with a bucket rate of
    // f0 those "sidebands" are the neighbouring harmonics, and it read the
    // source and the reconstruction as identical - which is the real result.
    let base_period = audio.sample_rate / sub.base_freq;
    let cycles = original.len() as f32 / base_period;
    let cycles_per_bucket = cycles / grid.num_buckets as f32;
    println!(
        "{} buckets over {:.0} cycles = {cycles_per_bucket:.3} cycles/bucket",
        grid.num_buckets, cycles
    );
    assert!(
        (cycles_per_bucket - 1.0).abs() < 0.02,
        "a bucket must be one period, got {cycles_per_bucket:.3} cycles - if that \
         changes, bucket-rate modulation becomes possible again and this test has \
         to go back to measuring sidebands"
    );

    // The stronger statement the sideband measurement was reaching for: the
    // reconstruction *is* the source, so it cannot carry modulation the source
    // does not.
    let rec = plugin
        .resynthesize_exact(&grid, true, grid.sample_rate)
        .expect("resynthesize_exact");
    let n = original.len().min(rec.len());
    let peak = original.iter().fold(0.0f32, |m, &v| m.max(v.abs())).max(1e-9);
    let worst = (0..n).fold(0.0f32, |m, i| m.max((original[i] - rec[i]).abs())) / peak;
    let db = 20.0 * worst.max(1e-12).log10();
    println!("peak reconstruction error: {db:.1} dB");
    assert!(db < -80.0, "reconstruction is not exact: {db:.1} dB");
}

/// The rendered note must be in tune with the source, bucket by bucket. This
/// failed by up to **+36.5 cents** when `bucket_period` rounded to whole
/// samples: at 590.7 Hz only 595.9 (period 37) and 580.3 (38) were reachable,
/// 46 cents apart, and 97% of buckets took 37 — sharp and warbling instead of
/// carrying the vibrato. The fractional phase accumulator removed it.
#[test]
fn d5_rendered_pitch_matches_the_source() {
    let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("D5.wav");
    if !wav.exists() {
        return;
    }
    let plugin = load_plugin();
    let audio = decode_audio_file(&wav).expect("decode D5.wav");
    let sub = analysis::segment(&audio.samples, audio.sample_rate)
        .into_iter()
        .find(|s| s.is_reasonable(audio.sample_rate))
        .expect("one reasonable subtrack");
    let end = sub.end.min(audio.samples.len());
    let original = &audio.samples[sub.start..end];
    let contour = analysis::build_contour(&sub);
    let grid = plugin
        .analyze_full(
            original,
            audio.sample_rate,
            sub.base_freq,
            &contour,
            0,
            NUM_HARMONICS,
        )
        .expect("analyze_full");
    let sr = audio.sample_rate;
    let base_period = sr / sub.base_freq;

    let mut worst = 0.0f32;
    let mut worst_at = (0.0f32, 0.0f32);
    for b in 0..grid.num_buckets {
        let want = sub.base_freq * grid.pitch_ratio[b];
        let got = sr / grid.rendered_period(base_period, b);
        let cents = 1200.0 * (got / want).log2();
        if cents.abs() > worst.abs() {
            worst = cents;
            worst_at = (want, got);
        }
    }
    println!(
        "worst rendered pitch error: {:+.1} cents (wanted {:.2} Hz, rendered {:.2} Hz)",
        worst, worst_at.0, worst_at.1
    );
    assert!(
        worst.abs() < 5.0,
        "rendered pitch is {:+.1} cents off (wanted {:.2} Hz, got {:.2} Hz) — \
         whole-sample period rounding detunes the resynthesis",
        worst,
        worst_at.0,
        worst_at.1
    );
}
