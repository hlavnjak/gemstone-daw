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
//! Integration test for the resynthesis pipeline on a real recording.
//!
//! Decodes `D5.wav`, segments it into pitch-stable subtracks, loads a real
//! LeSynth Fourier instance (the internal plugin shared object) and, for each
//! reasonable subtrack, runs the plugin's harmonic analysis — the exact data
//! the plugin's amplitude/phase charts display. Verifies that non-trivial
//! amp/phase curves are produced per subtrack and per bucket.

use std::path::{Path, PathBuf};

use gemstone_daw::analysis;
use gemstone_daw::audio::decode_audio_file;
use gemstone_daw::vst::{class_ids, PluginInstance};

const NUM_HARMONICS: usize = 32;
const NUM_BUCKETS: usize = 96;

fn internal_plugin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("internal_plugins")
        .join("liblesynth_fourier.so")
}

#[test]
fn d5_wav_produces_amp_and_phase_curves() {
    let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("D5.wav");
    if !wav.exists() {
        eprintln!("skipping: {:?} not present", wav);
        return;
    }
    let plugin_path = internal_plugin_path();
    assert!(
        plugin_path.exists(),
        "internal plugin not built: {:?} (run `make build`)",
        plugin_path
    );

    // 1) Decode.
    let audio = decode_audio_file(&wav).expect("decode D5.wav");
    println!(
        "decoded {:.2}s, {} samples @ {} Hz",
        audio.duration_secs(),
        audio.samples.len(),
        audio.sample_rate
    );
    assert!(!audio.samples.is_empty());

    // 2) Segment.
    let subs = analysis::segment(&audio.samples, audio.sample_rate);
    println!("found {} raw subtrack(s)", subs.len());
    let reasonable: Vec<_> = subs
        .iter()
        .filter(|s| s.is_reasonable(audio.sample_rate))
        .collect();
    println!("{} reasonable subtrack(s)", reasonable.len());
    assert!(
        !reasonable.is_empty(),
        "expected at least one analysable subtrack in D5.wav"
    );

    // 3) Load a real LeSynth Fourier instance (this opens the plugin's shared
    //    object — the same one whose editor would display these curves).
    let plugin = PluginInstance::load(&plugin_path, Some(&class_ids::FOURIER_SYNTH))
        .expect("load internal LeSynth Fourier");

    // 4) Analyse each subtrack and validate the produced curves.
    let mut any_strong_subtrack = false;
    for (i, sub) in reasonable.iter().enumerate() {
        let end = sub.end.min(audio.samples.len());
        let samples = &audio.samples[sub.start..end];
        // Build the pitch contour (absolute Hz) the bridge carries, exercising
        // the vibrato-aware path end to end.
        let len = sub.len().max(1);
        let contour: Vec<f32> = (0..256)
            .map(|k| {
                let off = (k as f32 + 0.5) / 256.0 * len as f32;
                sub.freq_at(sub.start + off as usize)
            })
            .collect();
        let (amp, phase) = plugin
            .analyze(
                samples,
                audio.sample_rate,
                sub.base_freq,
                &contour,
                NUM_BUCKETS,
                NUM_HARMONICS,
            )
            .expect("plugin analyze");

        // Shape: one curve per harmonic, one value per bucket.
        assert_eq!(amp.len(), NUM_HARMONICS);
        assert_eq!(phase.len(), NUM_HARMONICS);
        assert_eq!(amp[0].len(), NUM_BUCKETS);
        assert_eq!(phase[0].len(), NUM_BUCKETS);

        // Peak amplitude across the whole grid + how many harmonics are active.
        let mut peak = 0.0f32;
        let mut active_harmonics = 0;
        for h in 0..NUM_HARMONICS {
            let hmax = amp[h].iter().copied().fold(0.0f32, f32::max);
            if hmax > 0.01 {
                active_harmonics += 1;
            }
            peak = peak.max(hmax);
        }
        // Count buckets along the fundamental that carry content — confirms the
        // curve is generated *per bucket*, not just a single point.
        let fundamental_buckets = amp[0].iter().filter(|&&v| v > 0.01).count();
        // A phase value should exist (non-zero) somewhere the amplitude is set.
        let phase_present = (0..NUM_HARMONICS)
            .any(|h| phase[h].iter().any(|&p| p != 0.0));

        println!(
            "subtrack {}: {:.0} Hz, {:.2}s, conf {:.2} -> peak amp {:.3}, {} active harmonics, {}/{} fundamental buckets filled, phase_present={}",
            i,
            sub.base_freq,
            sub.duration_secs(audio.sample_rate),
            sub.confidence,
            peak,
            active_harmonics,
            fundamental_buckets,
            NUM_BUCKETS,
            phase_present
        );

        // The strong (tonal) subtrack must yield clearly-visible curves:
        // a peak well up the 0..1 axis, several harmonics, content across many
        // buckets, and real phase data.
        if peak >= 0.5 && active_harmonics >= 2 && fundamental_buckets >= NUM_BUCKETS / 2 && phase_present {
            any_strong_subtrack = true;
        }
    }

    assert!(
        any_strong_subtrack,
        "no subtrack produced a clearly-visible amplitude/phase curve — charts would look empty"
    );
}