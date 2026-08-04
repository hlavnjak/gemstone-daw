// Copyright 2025 Jakub Hlavnicka
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
//! Does the *source material* decide how good resynthesis sounds — and is the
//! container/codec (`.wav` vs `.m4a`) any part of it?
//!
//! Speech resynthesised audibly worse than a violin note, and lossy compression
//! was the obvious suspect. These files move one variable at a time:
//!
//! * `D5.wav` — 22.05 kHz mono PCM, the reference.
//! * `D5` at 48 kb/s AAC — *identical content*, lossy container, so any damage
//!   is the codec's. Supply via `D5_AAC` (skipped when absent):
//!   `ffmpeg -i D5.wav -c:a aac -b:a 48k -ar 48000 -ac 2 /tmp/d5.m4a`
//! * `my_voice.m4a` — different content *and* lossy.
//!
//! # The answer
//!
//! **Neither the codec nor the material.** Both were symptoms of the transform
//! not being invertible; it is now, so all three files reconstruct to float
//! rounding (~-127 dB, asserted in `resynth_exact.rs`) and these tables are
//! flat. The numbers are kept as the record of what a lossy transform did, and
//! as a cheap guard — a regression shows up as a spread between the files.
//!
//! | measured on `my_voice.m4a` | before | after |
//! |---|---|---|
//! | reconstruction's harmonic share | 54.9% | 97.7% (source 98.0%) |
//! | worst per-band error | -10.5 dB | -0.32 dB |
//! | bucket duration | 36.9 ms | 9.3 ms (one period) |
//! | phase slip within a bucket, top harmonic | 8.23 rad | 0.51 rad |
//! | harmonic-to-residual | 0.9 dB | 16.2 dB (source 16.9) |
//!
//! The old diagnosis — "a bucket is a fixed number of rendered cycles, so low
//! `f0` gets 5× coarser resolution exactly where speech needs it finest" —
//! described the defect correctly and pointed at the wrong fix, per-material
//! special-casing. One bucket per period with absolute phase removed the
//! trade-off for every source at once.
//!
//! Run with `--nocapture`; each test prints its table.

use std::path::{Path, PathBuf};

use gemstone_daw::analysis;
use gemstone_daw::audio::{decode_audio_file, DecodedAudio};
use gemstone_daw::vst::{class_ids, PluginInstance};

const NUM_HARMONICS: usize = 256;

fn internal_plugin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("internal_plugins")
        .join("liblesynth_fourier.so")
}

fn load_plugin() -> PluginInstance {
    let path = internal_plugin_path();
    assert!(path.exists(), "internal plugin not built: {path:?}");
    PluginInstance::load(&path, Some(&class_ids::FOURIER_SYNTH), None)
        .expect("load internal LeSynth Fourier")
}

fn db_power(x: f64) -> f64 {
    10.0 * x.max(1e-20).log10()
}

/// Amplitudes at `k · f0` and the window's total power, in matching
/// (amplitude²) units. Same single-bin DFT the plugin's analyser uses, applied
/// by the test to both signals so the comparison is symmetric, and measured at
/// whatever pitch each signal actually has at that instant (see
/// `resynth_roundtrip_d5.rs` — a fixed-frequency FFT smears vibrato and lies).
fn harmonic_spectrum(x: &[f32], sr: f32, center: usize, f0: f32, nh: usize) -> Option<(Vec<f64>, f64)> {
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

    let mut mags = vec![0.0f64; nh];
    for h in 1..=nh {
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
    Some((mags, 2.0 * total / w2))
}

/// What a harmonic model can and cannot take from this recording, plus what it
/// gave back.
struct Report {
    label: String,
    sample_rate: f32,
    secs: f32,
    /// Subtracks found / of those, usable for analysis.
    subtracks: (usize, usize),
    /// Share of the analysed span that any usable subtrack covers at all.
    coverage: f32,
    base_freq: f32,
    /// Vibrato/drift span of the analysed subtrack, in cents.
    pitch_span: f32,
    /// Autocorrelation confidence — how periodic the source is to begin with.
    confidence: f32,
    /// Harmonic-to-residual of the *source*: the ceiling additive synthesis can
    /// reach, before the pipeline does anything at all.
    src_hnr: f64,
    /// Same for the reconstruction.
    rec_hnr: f64,
    /// Reconstruction level relative to source, dB.
    level_db: f64,
    /// Median per-period correlation, and the source's own period-to-period
    /// self-similarity (the ceiling that number is measured against).
    corr: f32,
    ceiling: f32,
}

fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        ab += a[i] as f64 * b[i] as f64;
        aa += a[i] as f64 * a[i] as f64;
        bb += b[i] as f64 * b[i] as f64;
    }
    let denom = (aa * bb).sqrt();
    if denom < 1e-20 { 0.0 } else { (ab / denom) as f32 }
}

fn aligned_correlation(src: &[f32], start: usize, chunk: &[f32]) -> f32 {
    let period = chunk.len();
    if start + 2 * period >= src.len() || period < 4 {
        return f32::NAN;
    }
    let at = |shift: i32| {
        let s = shift.rem_euclid(period as i32) as usize;
        correlation(&src[start + s..start + s + period], chunk)
    };
    let mut best = -2.0f32;
    for shift in 0..period as i32 {
        best = best.max(at(shift));
    }
    best
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / x.len() as f64).sqrt() as f32
}

fn median(v: &mut Vec<f32>) -> f32 {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return f32::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn analyse(label: &str, audio: &DecodedAudio, plugin: &PluginInstance) -> Report {
    let sr = audio.sample_rate;
    let subs = analysis::segment(&audio.samples, sr);
    let usable: Vec<_> = subs.iter().filter(|s| s.is_reasonable(sr)).collect();
    let covered: usize = usable.iter().map(|s| s.end.min(audio.samples.len()) - s.start).sum();
    let coverage = covered as f32 / audio.samples.len().max(1) as f32;

    // Judge the longest usable subtrack — the most favourable case the file offers.
    let sub = (*usable
        .iter()
        .max_by_key(|s| s.end - s.start)
        .expect("no usable subtrack"))
    .clone();

    let end = sub.end.min(audio.samples.len());
    let original = &audio.samples[sub.start..end];
    let contour = analysis::build_contour(&sub);
    let grid = plugin
        .analyze_full(original, sr, sub.base_freq, &contour, 0, NUM_HARMONICS)
        .expect("analyze_full");
    let base_period = sr / sub.base_freq;
    let recon = plugin
        .resynthesize(&grid, base_period, 0, original.len(), true)
        .expect("resynthesize");

    // Which period each rendered chunk really used.
    let mut chunks: Vec<(usize, f32)> = Vec::new();
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

    // Harmonics that exist below Nyquist at this pitch, capped like the renderer.
    let nh = NUM_HARMONICS
        .min((sr * 0.5 / sub.base_freq) as usize)
        .min((base_period / 2.0) as usize)
        .max(1);

    let n = original.len().min(recon.len());
    let level_db =
        20.0 * (rms(&recon[..n]).max(1e-9) as f64 / rms(&original[..n]).max(1e-9) as f64).log10();

    let step = (base_period * 8.0) as usize;
    let from = (0.15 * sr) as usize;
    let to = original.len().saturating_sub(step);
    let (mut tot_o, mut tot_r, mut harm_o, mut harm_r) = (0.0, 0.0, 0.0, 0.0);
    let mut p = from;
    while p < to {
        let f_src = sub.freq_at(sub.start + p);
        let f_rec = sr / period_at(p);
        if let (Some((mo, to_)), Some((mr, tr))) = (
            harmonic_spectrum(original, sr, p, f_src, nh),
            harmonic_spectrum(&recon, sr, p, f_rec, nh),
        ) {
            for h in 0..nh {
                harm_o += mo[h] * mo[h];
                harm_r += mr[h] * mr[h];
            }
            tot_o += to_;
            tot_r += tr;
        }
        p += step;
    }
    let hnr = |h: f64, t: f64| db_power(h / (t - h).max(1e-12));

    // Per-period correlation, and the source's own period-to-period similarity.
    let mut corrs = Vec::new();
    let mut selfs = Vec::new();
    let mut pos = from;
    while pos + 3 * (base_period as usize) < to {
        let per = period_at(pos).round() as usize;
        if pos + per <= recon.len() {
            corrs.push(aligned_correlation(original, pos, &recon[pos..pos + per]));
            selfs.push(aligned_correlation(original, pos, &original[pos + per..pos + 2 * per]));
        }
        pos += step;
    }

    let cents = |lo: f32, hi: f32| 1200.0 * (hi / lo).log2();
    let (mut lo, mut hi) = (f32::MAX, 0.0f32);
    for b in 0..grid.num_buckets {
        let f = sub.base_freq * grid.pitch_ratio[b];
        lo = lo.min(f);
        hi = hi.max(f);
    }

    Report {
        label: label.to_string(),
        sample_rate: sr,
        secs: audio.duration_secs(),
        subtracks: (subs.len(), usable.len()),
        coverage,
        base_freq: sub.base_freq,
        pitch_span: cents(lo, hi),
        confidence: sub.confidence,
        src_hnr: hnr(harm_o, tot_o),
        rec_hnr: hnr(harm_r, tot_r),
        level_db,
        corr: median(&mut corrs),
        ceiling: median(&mut selfs),
    }
}

#[test]
fn source_material_not_container_decides_fidelity() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let d5 = root.join("D5.wav");
    let voice = root.join("my_voice.m4a");
    let d5_aac = PathBuf::from(
        std::env::var("D5_AAC").unwrap_or_default(),
    );

    let plugin = load_plugin();
    let mut reports = Vec::new();

    for (label, path) in [
        ("D5.wav (PCM 22k)", d5.clone()),
        ("D5 -> AAC 48kbps", d5_aac.clone()),
        ("my_voice.m4a", voice.clone()),
    ] {
        if !path.exists() {
            eprintln!("skipping {label}: {path:?} not present");
            continue;
        }
        let audio = decode_audio_file(&path).expect("decode");
        reports.push(analyse(label, &audio, &plugin));
    }

    println!(
        "\n{:<20} {:>7} {:>6} {:>8} {:>7} {:>7} {:>8} {:>8} {:>8} {:>7} {:>8}",
        "file", "sr", "secs", "usable", "f0", "cents", "conf", "srcHNR", "recHNR", "corr", "ceiling"
    );
    for r in &reports {
        println!(
            "{:<20} {:>7.0} {:>6.2} {:>4}/{:<3} {:>7.1} {:>7.0} {:>8.2} {:>8.1} {:>8.1} {:>7.3} {:>8.3}",
            r.label,
            r.sample_rate,
            r.secs,
            r.subtracks.1,
            r.subtracks.0,
            r.base_freq,
            r.pitch_span,
            r.confidence,
            r.src_hnr,
            r.rec_hnr,
            r.corr,
            r.ceiling,
        );
    }
    for r in &reports {
        println!(
            "{:<20} coverage {:.0}% of file analysable, level {:+.2} dB",
            r.label, 100.0 * r.coverage, r.level_db
        );
    }

    // The conclusion, pinned: lossy encoding must not meaningfully change how
    // well this pipeline reconstructs the *same* content. If this ever fires,
    // the codec really has become a factor and the advice in the module docs
    // ("accepting .m4a/.mp3 is fine") needs revisiting.
    let pcm = reports.iter().find(|r| r.label.starts_with("D5.wav"));
    let aac = reports.iter().find(|r| r.label.starts_with("D5 ->"));
    if let (Some(pcm), Some(aac)) = (pcm, aac) {
        assert!(
            (aac.src_hnr - pcm.src_hnr).abs() < 4.0,
            "48 kb/s AAC changed the source's harmonic-to-residual by {:.1} dB \
             ({:.1} -> {:.1}); the codec is now doing real damage to the material \
             the analyser sees",
            aac.src_hnr - pcm.src_hnr,
            pcm.src_hnr,
            aac.src_hnr
        );
        assert!(
            aac.corr > pcm.corr - 0.02,
            "the AAC copy of D5 reconstructs at {:.3} against {:.3} for the PCM \
             original — compression, not source material, has become the limit",
            aac.corr,
            pcm.corr
        );
    }
}

/// Where does the voice reconstruction's energy go? `src_hnr` vs `rec_hnr`
/// says it is *less* tonal than its source, which additive synthesis should be
/// incapable of — a real defect, or a smearing measurement. Octave bands, both
/// signals, band by band.
#[test]
fn voice_reconstruction_energy_by_band() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugin = load_plugin();

    for (label, path) in [
        ("D5.wav", root.join("D5.wav")),
        ("D5 -> AAC 48kbps", PathBuf::from(std::env::var("D5_AAC").unwrap_or_default())),
        ("my_voice.m4a", root.join("my_voice.m4a")),
    ] {
        if !path.exists() {
            continue;
        }
        let audio = decode_audio_file(&path).expect("decode");
        let sr = audio.sample_rate;
        let subs = analysis::segment(&audio.samples, sr);
        let sub = subs
            .iter()
            .filter(|s| s.is_reasonable(sr))
            .max_by_key(|s| s.end - s.start)
            .expect("usable subtrack")
            .clone();
        let end = sub.end.min(audio.samples.len());
        let original = &audio.samples[sub.start..end];
        let contour = analysis::build_contour(&sub);
        let grid = plugin
            .analyze_full(original, sr, sub.base_freq, &contour, 0, NUM_HARMONICS)
            .expect("analyze_full");
        let base_period = sr / sub.base_freq;
        let recon = plugin
            .resynthesize(&grid, base_period, 0, original.len(), true)
            .expect("resynthesize");

        let mut chunks: Vec<(usize, f32)> = Vec::new();
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

        let nh = NUM_HARMONICS
            .min((sr * 0.5 / sub.base_freq) as usize)
            .min((base_period / 2.0) as usize)
            .max(1);

        let step = (base_period * 8.0) as usize;
        let from = (0.15 * sr) as usize;
        let to = original.len().saturating_sub(step);
        let mut acc_o = vec![0.0f64; nh];
        let mut acc_r = vec![0.0f64; nh];
        let (mut tot_o, mut tot_r) = (0.0f64, 0.0f64);
        let mut frames = 0usize;
        let mut p = from;
        while p < to {
            let f_src = sub.freq_at(sub.start + p);
            let f_rec = sr / period_at(p);
            if let (Some((mo, to_)), Some((mr, tr))) = (
                harmonic_spectrum(original, sr, p, f_src, nh),
                harmonic_spectrum(&recon, sr, p, f_rec, nh),
            ) {
                for h in 0..nh {
                    acc_o[h] += mo[h] * mo[h];
                    acc_r[h] += mr[h] * mr[h];
                }
                tot_o += to_;
                tot_r += tr;
                frames += 1;
            }
            p += step;
        }

        println!(
            "\n=== {label}: f0 {:.1} Hz, {nh} harmonics modelled, {frames} frames ===",
            sub.base_freq
        );
        println!(" harmonics      kHz      source dB    recon dB      delta   share src  share rec");
        let mut lo = 1usize;
        while lo <= nh {
            let hi = (lo * 2 - 1).min(nh);
            let so: f64 = acc_o[lo - 1..hi].iter().sum::<f64>() / frames as f64;
            let sr_: f64 = acc_r[lo - 1..hi].iter().sum::<f64>() / frames as f64;
            println!(
                " H{:<3}-H{:<4}  {:>7.1}   {:>10.2}  {:>10.2}  {:>+9.2}   {:>8.2}%  {:>8.2}%",
                lo,
                hi,
                hi as f32 * sub.base_freq / 1000.0,
                db_power(so),
                db_power(sr_),
                db_power(sr_) - db_power(so),
                100.0 * so / (tot_o / frames as f64),
                100.0 * sr_ / (tot_r / frames as f64),
            );
            lo = hi + 1;
        }
        let harm_o: f64 = acc_o.iter().sum::<f64>() / frames as f64;
        let harm_r: f64 = acc_r.iter().sum::<f64>() / frames as f64;
        let (to_, tr_) = (tot_o / frames as f64, tot_r / frames as f64);
        println!(
            " total power: source {:.2} dB, recon {:.2} dB",
            db_power(to_),
            db_power(tr_)
        );
        println!(
            " harmonic share: source {:.1}%, recon {:.1}%  (recon >100% would mean smearing)",
            100.0 * harm_o / to_,
            100.0 * harm_r / tr_
        );
    }
}

/// Is the missing harmonic energy displaced (wrong rendered pitch) or spread
/// (modulation sidebands)? A fine spectrum around a strong harmonic tells them
/// apart: a pitch error moves the peak, modulation keeps the peak and grows
/// symmetric skirts around it.
#[test]
fn voice_fine_spectrum_around_a_harmonic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugin = load_plugin();
    for (label, path, probe) in [
        ("D5.wav", root.join("D5.wav"), 4usize),
        ("my_voice.m4a", root.join("my_voice.m4a"), 8usize),
    ] {
        if !path.exists() {
            continue;
        }
        let audio = decode_audio_file(&path).expect("decode");
        let sr = audio.sample_rate;
        let sub = analysis::segment(&audio.samples, sr)
            .into_iter()
            .filter(|s| s.is_reasonable(sr))
            .max_by_key(|s| s.end - s.start)
            .expect("usable subtrack");
        let end = sub.end.min(audio.samples.len());
        let original = &audio.samples[sub.start..end];
        let contour = analysis::build_contour(&sub);
        let grid = plugin
            .analyze_full(original, sr, sub.base_freq, &contour, 0, NUM_HARMONICS)
            .expect("analyze_full");
        let base_period = sr / sub.base_freq;
        let recon = plugin
            .resynthesize(&grid, base_period, 0, original.len(), true)
            .expect("resynthesize");

        let cycles = original.len() as f32 / base_period;
        let cycles_per_bucket = cycles / grid.num_buckets as f32;
        let bucket_rate = sub.base_freq / cycles_per_bucket;

        // A window short enough that the source's own pitch is roughly steady,
        // long enough to resolve the bucket rate.
        let len = (0.20 * sr) as usize;
        let start = original.len() / 3;
        if start + len >= recon.len() {
            continue;
        }
        let power = |x: &[f32], f: f32| -> f64 {
            let w = 2.0 * std::f64::consts::PI * f as f64 / sr as f64;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for i in 0..len {
                let hann =
                    0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / len as f64).cos();
                let s = x[start + i] as f64 * hann;
                re += s * (w * i as f64).cos();
                im -= s * (w * i as f64).sin();
            }
            re * re + im * im
        };

        let f0 = sub.freq_at(sub.start + start + len / 2);
        let fh = f0 * probe as f32;
        println!(
            "\n=== {label}: H{probe} at {fh:.1} Hz (f0 {f0:.1}), bucket rate \
             {bucket_rate:.1} Hz, {:.2} cycles/bucket ===",
            cycles_per_bucket
        );
        let peak_o = power(original, fh).max(1e-30);
        let peak_r = power(&recon, fh).max(1e-30);
        println!("  offset Hz    source dB    recon dB   (each relative to its own peak at H{probe})");
        let span = (bucket_rate * 1.6) as i32;
        let mut off = -span;
        while off <= span {
            let f = fh + off as f32;
            println!(
                "  {:>+9}    {:>9.1}    {:>8.1}",
                off,
                db_power(power(original, f) / peak_o),
                db_power(power(&recon, f) / peak_r),
            );
            off += (span / 8).max(1);
        }
        // Where the recon's peak actually is, in cents.
        let mut best = (fh, f64::NEG_INFINITY);
        let mut f = fh * 0.97;
        while f < fh * 1.03 {
            let p = power(&recon, f);
            if p > best.1 {
                best = (f, p);
            }
            f += fh * 0.0002;
        }
        println!(
            "  recon peak sits {:+.1} cents from k*f0 ({:.1} Hz vs {:.1} Hz)",
            1200.0 * (best.0 / fh).log2(),
            best.0,
            fh
        );
    }
}

/// Guard against the measurement trap: `harmonic_spectrum` reads at one
/// supplied `f0`, so a slightly wrong estimate drops high harmonics (at `k·f0`,
/// with `k`× the error) out of their bins and reads as lost energy. The
/// reconstruction is a sum of partials, so its harmonic share *must* be ~100% at
/// the right pitch; scanning `f0` per frame separates deficit from estimate.
#[test]
fn harmonic_share_is_not_an_f0_estimation_artifact() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugin = load_plugin();
    for (label, path) in [
        ("D5.wav", root.join("D5.wav")),
        ("my_voice.m4a", root.join("my_voice.m4a")),
    ] {
        if !path.exists() {
            continue;
        }
        let audio = decode_audio_file(&path).expect("decode");
        let sr = audio.sample_rate;
        let sub = analysis::segment(&audio.samples, sr)
            .into_iter()
            .filter(|s| s.is_reasonable(sr))
            .max_by_key(|s| s.end - s.start)
            .expect("usable subtrack");
        let end = sub.end.min(audio.samples.len());
        let original = &audio.samples[sub.start..end];
        let contour = analysis::build_contour(&sub);
        let grid = plugin
            .analyze_full(original, sr, sub.base_freq, &contour, 0, NUM_HARMONICS)
            .expect("analyze_full");
        let base_period = sr / sub.base_freq;
        let recon = plugin
            .resynthesize(&grid, base_period, 0, original.len(), true)
            .expect("resynthesize");

        let mut chunks: Vec<(usize, f32)> = Vec::new();
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
        let nh = NUM_HARMONICS
            .min((sr * 0.5 / sub.base_freq) as usize)
            .min((base_period / 2.0) as usize)
            .max(1);

        // Harmonic share of one frame at a given f0.
        let share = |x: &[f32], p: usize, f0: f32| -> Option<f64> {
            let (m, t) = harmonic_spectrum(x, sr, p, f0, nh)?;
            Some(m.iter().map(|v| v * v).sum::<f64>() / t.max(1e-20))
        };

        let step = (base_period * 8.0) as usize;
        let from = (0.15 * sr) as usize;
        let to = original.len().saturating_sub(step);
        let (mut fixed_o, mut fixed_r, mut best_o, mut best_r) = (0.0, 0.0, 0.0, 0.0);
        let (mut cents_o, mut cents_r) = (0.0f64, 0.0f64);
        let mut frames = 0usize;
        let mut p = from;
        while p < to {
            let f_src = sub.freq_at(sub.start + p);
            let f_rec = sr / period_at(p);
            let (Some(so), Some(sr_)) = (share(original, p, f_src), share(&recon, p, f_rec))
            else {
                p += step;
                continue;
            };
            // Scan ±2% around each signal's nominal f0.
            let scan = |x: &[f32], f_nom: f32| -> (f64, f64) {
                let mut best = (0.0f64, 0.0f64);
                let mut k = -40i32;
                while k <= 40 {
                    let f = f_nom * (1.0 + k as f32 * 0.0005);
                    if let Some(s) = share(x, p, f) {
                        if s > best.0 {
                            best = (s, 1200.0 * (f / f_nom).log2() as f64);
                        }
                    }
                    k += 1;
                }
                best
            };
            let (bo, co) = scan(original, f_src);
            let (br, cr) = scan(&recon, f_rec);
            fixed_o += so;
            fixed_r += sr_;
            best_o += bo;
            best_r += br;
            cents_o += co;
            cents_r += cr;
            frames += 1;
            p += step;
        }
        let f = frames as f64;
        println!("\n=== {label} ({frames} frames, {nh} harmonics) ===");
        println!(
            "  source: {:.1}% at the tracked f0  ->  {:.1}% at best-fit f0 ({:+.1} cents mean)",
            100.0 * fixed_o / f,
            100.0 * best_o / f,
            cents_o / f
        );
        println!(
            "  recon : {:.1}% at the rendered f0 ->  {:.1}% at best-fit f0 ({:+.1} cents mean)",
            100.0 * fixed_r / f,
            100.0 * best_r / f,
            cents_r / f
        );
    }
}

/// Independent check on the pitch finding above, using the project's own
/// tracker instead of the test's DFT: segment the *reconstruction* and compare
/// the pitch it actually carries against the contour it was rendered from.
#[test]
fn rendered_pitch_tracks_the_contour_for_both_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugin = load_plugin();
    for (label, path) in [
        ("D5.wav", root.join("D5.wav")),
        ("my_voice.m4a", root.join("my_voice.m4a")),
    ] {
        if !path.exists() {
            continue;
        }
        let audio = decode_audio_file(&path).expect("decode");
        let sr = audio.sample_rate;
        let sub = analysis::segment(&audio.samples, sr)
            .into_iter()
            .filter(|s| s.is_reasonable(sr))
            .max_by_key(|s| s.end - s.start)
            .expect("usable subtrack");
        let end = sub.end.min(audio.samples.len());
        let original = &audio.samples[sub.start..end];
        let contour = analysis::build_contour(&sub);
        let grid = plugin
            .analyze_full(original, sr, sub.base_freq, &contour, 0, NUM_HARMONICS)
            .expect("analyze_full");
        let base_period = sr / sub.base_freq;
        let recon = plugin
            .resynthesize(&grid, base_period, 0, original.len(), true)
            .expect("resynthesize");

        // Re-track the reconstruction with the same segmenter.
        let back = analysis::segment(&recon, sr);
        let rsub = back
            .iter()
            .filter(|s| s.is_reasonable(sr))
            .max_by_key(|s| s.end - s.start);
        let cents = |a: f32, b: f32| 1200.0 * (b / a).log2();
        match rsub {
            Some(r) => println!(
                "\n{label}: source f0 {:.2} Hz -> reconstruction re-tracked at {:.2} Hz \
                 ({:+.1} cents), conf {:.2} vs {:.2}",
                sub.base_freq,
                r.base_freq,
                cents(sub.base_freq, r.base_freq),
                sub.confidence,
                r.confidence
            ),
            None => println!("\n{label}: reconstruction has no trackable subtrack"),
        }

        // And bucket by bucket: the pitch the grid asked for vs the pitch rendered.
        let mut worst = 0.0f32;
        let mut sum = 0.0f64;
        for b in 0..grid.num_buckets {
            let want = sub.base_freq * grid.pitch_ratio[b];
            let got = sr / grid.rendered_period(base_period, b);
            let e = cents(want, got);
            sum += e as f64;
            if e.abs() > worst.abs() {
                worst = e;
            }
        }
        println!(
            "  per-bucket rendered-vs-requested: mean {:+.2} cents, worst {:+.2} cents",
            sum / grid.num_buckets as f64,
            worst
        );
    }
}

/// How much does each source move *inside* one bucket? That is content the
/// model cannot represent however good the DSP is, and it scales with harmonic
/// index, so a low-f0 source paid for it far more than a high-f0 one.
#[test]
fn how_much_the_source_moves_inside_one_bucket() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugin = load_plugin();
    for (label, path) in [
        ("D5.wav", root.join("D5.wav")),
        ("D5 -> AAC 48kbps", PathBuf::from(std::env::var("D5_AAC").unwrap_or_default())),
        ("my_voice.m4a", root.join("my_voice.m4a")),
    ] {
        if !path.exists() {
            continue;
        }
        let audio = decode_audio_file(&path).expect("decode");
        let sr = audio.sample_rate;
        let sub = analysis::segment(&audio.samples, sr)
            .into_iter()
            .filter(|s| s.is_reasonable(sr))
            .max_by_key(|s| s.end - s.start)
            .expect("usable subtrack");
        let end = sub.end.min(audio.samples.len());
        let original = &audio.samples[sub.start..end];
        let contour = analysis::build_contour(&sub);
        let grid = plugin
            .analyze_full(original, sr, sub.base_freq, &contour, 0, NUM_HARMONICS)
            .expect("analyze_full");
        let base_period = sr / sub.base_freq;
        let nh = NUM_HARMONICS
            .min((sr * 0.5 / sub.base_freq) as usize)
            .min((base_period / 2.0) as usize)
            .max(1);
        let bucket_ms = 1000.0 * original.len() as f32 / sr / grid.num_buckets as f32;

        // Pitch step between consecutive buckets, in cents.
        let mut steps: Vec<f32> = (1..grid.num_buckets)
            .map(|b| 1200.0 * (grid.pitch_ratio[b] / grid.pitch_ratio[b - 1]).log2())
            .collect();
        steps.sort_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap());
        let med = steps[steps.len() / 2].abs();
        let p95 = steps[(steps.len() as f32 * 0.95) as usize].abs();

        // Amplitude step of a mid harmonic, in dB, bucket to bucket.
        let mid = (nh / 4).max(1);
        let mut amp_steps: Vec<f64> = (1..grid.num_buckets)
            .map(|b| {
                let (a, c) = (grid.amp(mid, b) as f64, grid.amp(mid, b - 1) as f64);
                20.0 * (a.max(1e-9) / c.max(1e-9)).log10()
            })
            .collect();
        amp_steps.sort_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap());

        println!(
            "\n{label}: f0 {:.1} Hz, {nh} harmonics, {} buckets of {bucket_ms:.1} ms",
            sub.base_freq, grid.num_buckets
        );
        println!(
            "  pitch step per bucket: median {med:.2} cents, p95 {p95:.2} cents \
             -> at H{nh} that is {:.2} / {:.2} cents of phase error frozen per bucket",
            med, p95
        );
        // A frozen pitch error of `c` cents held for `bucket_ms` slips harmonic k
        // by this many radians by the end of the bucket.
        let slip = |c: f32, k: usize| {
            let df = sub.base_freq * k as f32 * ((c / 1200.0).exp2() - 1.0);
            2.0 * std::f32::consts::PI * df * bucket_ms / 1000.0
        };
        println!(
            "  phase slip by end of bucket at median step: H1 {:.2} rad, H{} {:.2} rad, H{nh} {:.2} rad",
            slip(med, 1),
            nh / 4,
            slip(med, nh / 4),
            slip(med, nh)
        );
        println!(
            "  H{mid} amplitude step per bucket: median {:.2} dB, p95 {:.2} dB",
            amp_steps[amp_steps.len() / 2].abs(),
            amp_steps[(amp_steps.len() as f32 * 0.95) as usize].abs()
        );
    }
}
