//! The audition plays into the device's stream, not at the rate the file was
//! analysed at. `resynth_exact` proves the inverse is exact at the *analysis*
//! rate and that pitch and duration survive any other — neither says the audio
//! is still faithful once the rate changed. That gap is where "bit-exact" and
//! "sounds fuzzy" were both true: the rate change rendered each bucket's
//! spectrum into a different number of points, ringing against the wrap-around
//! step of a period that does not repeat, once per period. −27.7 dB peak /
//! −50 dB rms, against −128 dB for the same grid at its own rate.
//!
//! Measuring it needs a reference that is not another resampler: a signal whose
//! samples are known at *both* rates — harmonics under a slow envelope.

use std::f64::consts::PI;
use std::path::PathBuf;

use gemstone_daw::vst::{class_ids, PluginInstance};

const NUM_HARMONICS: usize = 256;
const F0: f64 = 590.0;
const SECS: f64 = 0.5;

/// Samples at each end the kernel fills partly from the silence outside the
/// subtrack — correct for a finite note (the reference tone just carries on),
/// and shorter than the voice's own 128-sample fade. Bounded separately below
/// rather than ignored.
const EDGE: usize = 256;

fn load_plugin() -> PluginInstance {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("internal_plugins")
        .join("liblesynth_fourier.so");
    PluginInstance::load(&path, Some(&class_ids::FOURIER_SYNTH), None).expect("load plugin")
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-12).log10()
}

/// The continuous signal, sampled at whatever rate is asked for. Twelve
/// harmonics (top at 7.1 kHz, well inside every Nyquist used here) under a slow
/// envelope, so every rate sees the same band-limited waveform.
fn truth(rate: f64, secs: f64) -> Vec<f32> {
    let n = (rate * secs) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let env = 0.6 + 0.3 * (2.0 * PI * 3.0 * t).sin();
            let mut v = 0.0;
            for k in 1..=12u32 {
                let a = 1.0 / k as f64;
                let ph = 0.7 * k as f64;
                v += a * (2.0 * PI * k as f64 * F0 * t + ph).sin();
            }
            (v * env / 3.2) as f32
        })
        .collect()
}

/// Peak and rms error, both as dB relative to the reference's peak.
fn err_db(reference: &[f32], got: &[f32]) -> (f32, f32) {
    let n = reference.len().min(got.len());
    let peak = reference.iter().fold(0.0f32, |m, &v| m.max(v.abs())).max(1e-9);
    let worst = (0..n).fold(0.0f32, |m, i| m.max((reference[i] - got[i]).abs())) / peak;
    let rms = ((0..n)
        .map(|i| {
            let d = (reference[i] - got[i]) as f64;
            d * d
        })
        .sum::<f64>()
        / n as f64)
        .sqrt() as f32
        / peak;
    (db(worst), db(rms))
}

/// The same on real audio, where an exact reference still exists: at an integer
/// ratio a correct band-limited interpolation returns the input samples
/// themselves on every n-th output, so the file *is* the reference and the whole
/// pipeline — decode, segment, analyse, invert, rate-convert — is under test.
#[test]
fn real_files_survive_the_device_rate() {
    use gemstone_daw::analysis;
    use gemstone_daw::audio::decode_audio_file;

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugin = load_plugin();
    let mut checked = 0;

    // Each file paired with a device rate at exactly 2× its own.
    for (label, path) in [
        ("D5.wav", root.join("D5.wav")),
        ("my_voice.m4a", root.join("my_voice.m4a")),
    ] {
        if !path.exists() {
            eprintln!("skipping {label}");
            continue;
        }
        let audio = decode_audio_file(&path).expect("decode");
        let sr = audio.sample_rate;
        let sub = analysis::segment(&audio.samples, sr)
            .into_iter()
            .filter(|s| s.is_reasonable(sr))
            .max_by_key(|s| s.end - s.start)
            .expect("usable subtrack");
        let original = &audio.samples[sub.start..sub.end.min(audio.samples.len())];
        let contour = analysis::build_contour(&sub);
        let grid = plugin
            .analyze_full(original, sr, sub.base_freq, &contour, 0, NUM_HARMONICS)
            .expect("analyze_full");
        let out = plugin
            .resynthesize_exact(&grid, true, sr * 2.0)
            .expect("resynthesize_exact");

        let guard = 2 * 32; // the resampling kernel's edge region, in input samples
        let n = original.len().min(out.len() / 2);
        let peak = original.iter().fold(0.0f32, |m, &v| m.max(v.abs())).max(1e-9);
        let worst = (guard..n - guard)
            .fold(0.0f32, |m, i| m.max((out[2 * i] - original[i]).abs()))
            / peak;
        println!(
            "{label:<14} {sr:>6.0} -> {:>6.0} Hz: {:>7.1} dB over {} samples",
            sr * 2.0,
            db(worst),
            n - 2 * guard
        );
        assert!(
            db(worst) < -70.0,
            "{label} at {} Hz is {:.1} dB from the file it reproduces",
            sr * 2.0,
            db(worst)
        );
        checked += 1;
    }
    assert!(checked >= 2, "not enough source files present to prove anything");
}

#[test]
fn the_audition_is_faithful_at_the_device_rate() {
    let plugin = load_plugin();

    for analysis_rate in [22_050.0f64, 48_000.0] {
        let src = truth(analysis_rate, SECS);
        let grid = plugin
            .analyze_full(&src, analysis_rate as f32, F0 as f32, &[], 0, NUM_HARMONICS)
            .expect("analyze_full");

        // At its own rate the inverse is exact: rate handling must cost nothing
        // in the case that needs none.
        let same = plugin
            .resynthesize_exact(&grid, true, analysis_rate as f32)
            .expect("resynthesize_exact");
        let (worst, rms) = err_db(&src, &same);
        println!(
            "{analysis_rate:>7} -> {analysis_rate:>7}: peak {worst:>7.1} dB  rms {rms:>7.1} dB   \
             ({} buckets)",
            grid.num_buckets
        );
        assert!(worst < -80.0, "the inverse is not exact at its own rate: {worst:.1} dB");

        // The audible case, both up (22.05 kHz file) and down (48 kHz file on a
        // 44.1 device).
        for device_rate in [44_100.0f64, 48_000.0] {
            if device_rate == analysis_rate {
                continue;
            }
            let out = plugin
                .resynthesize_exact(&grid, true, device_rate as f32)
                .expect("resynthesize_exact");
            let reference = truth(device_rate, out.len() as f64 / device_rate);
            let n = reference.len().min(out.len());
            let (edge_worst, _) = err_db(&reference[..n], &out[..n]);
            let (worst, rms) = err_db(
                &reference[EDGE..n - EDGE],
                &out[EDGE..n - EDGE],
            );
            println!(
                "{analysis_rate:>7} -> {device_rate:>7}: peak {worst:>7.1} dB  rms {rms:>7.1} dB   \
                 (edges included: peak {edge_worst:>6.1} dB)"
            );
            assert!(
                rms < -80.0,
                "the audition at {device_rate} Hz is {rms:.1} dB rms from the signal it \
                 should reproduce"
            );
            // Measured −112 dB at 2×, −68 dB at 2.177×. Both far under hearing;
            // the threshold catches a structural regression (the per-bucket
            // resampler this replaced measured −27.7 dB).
            assert!(
                worst < -60.0,
                "the audition at {device_rate} Hz peaks {worst:.1} dB from the signal it \
                 should reproduce"
            );
            // The kernel's own edge region must still be a taper, not a click.
            assert!(
                edge_worst < -6.0,
                "the audition's edges are not a taper: {edge_worst:.1} dB"
            );
        }
    }
}
