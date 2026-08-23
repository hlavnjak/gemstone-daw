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
//! Harmonics switched off in the editor must still be off after a save and a
//! load. They are not part of the grid — a disabled harmonic keeps its analysed
//! row and is skipped when the note is rendered — so before `.lsft` version 4
//! every exported track came back with the whole spectrum switched on, and
//! nothing on screen said the sound had changed.

use std::path::PathBuf;

use gemstone_daw::track_format::TrackState;
use gemstone_daw::vst::{class_ids, next_instance_token, PluginInstance};

/// The plugin's grid is this many harmonics tall whatever is imported into it,
/// so the flags it reports are this long too.
const NUM_HARMONICS: usize = 256;

fn load_tagged() -> PluginInstance {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("internal_plugins")
        .join("liblesynth_fourier.so");
    PluginInstance::load(
        &path,
        Some(&class_ids::FOURIER_SYNTH),
        Some(next_instance_token()),
    )
    .expect("load plugin")
}

/// An analysed grid with two harmonics' amplitude and one harmonic's phase
/// switched off — the state the bug report describes.
fn state_with_disabled_harmonics() -> TrackState {
    let (nh, nb) = (NUM_HARMONICS, 4usize);
    let mut amplitude = vec![0.0f32; nh * nb];
    let mut phase = vec![0.0f32; nh * nb];
    for h in 0..8 {
        for b in 0..nb {
            amplitude[h * nb + b] = 0.5 / (h + 1) as f32;
            phase[h * nb + b] = 0.1 * h as f32;
        }
    }
    let mut amp_enabled = vec![true; nh];
    let mut phase_enabled = vec![true; nh];
    amp_enabled[1] = false;
    amp_enabled[7] = false;
    phase_enabled[3] = false;

    TrackState {
        num_harmonics: nh,
        num_buckets: nb,
        base_freq: 220.0,
        duration_secs: 0.5,
        sample_rate: 44_100.0,
        display_gain: 1.0,
        amplitude,
        phase,
        pitch_ratio: vec![1.0, 1.01, 0.99, 1.0],
        bucket_lengths: vec![200, 198, 202, 200],
        dc: vec![0.0; nb],
        nyquist: vec![0.0; nb],
        amp_enabled,
        phase_enabled,
    }
}

/// The whole path a "Save project" / "Export…" takes: the live instance's flags
/// are exported, written to a `.lsft`, read back, and imported into a fresh
/// instance — which must then report the same selection.
#[test]
fn disabled_harmonics_survive_a_save_and_a_load() {
    let state = state_with_disabled_harmonics();

    let source = load_tagged();
    source.import_state(&state).expect("import into source");
    let exported = source.export_state().expect("export from source");
    assert_eq!(
        exported.amp_enabled, state.amp_enabled,
        "the amp checkboxes must come back off the live instance"
    );
    assert_eq!(exported.phase_enabled, state.phase_enabled);

    let path = std::env::temp_dir().join(format!("gmst_flags_{}.lsft", std::process::id()));
    exported.write(&path).expect("write .lsft");
    let reloaded = TrackState::read(&path).expect("read .lsft");
    let _ = std::fs::remove_file(&path);
    assert_eq!(reloaded.amp_enabled, state.amp_enabled, "…and off the file");
    assert_eq!(reloaded.phase_enabled, state.phase_enabled);

    let target = load_tagged();
    target.import_state(&reloaded).expect("import into target");
    let round_tripped = target.export_state().expect("export from target");
    assert_eq!(
        round_tripped.amp_enabled, state.amp_enabled,
        "a loaded track must play with the harmonics it was saved with"
    );
    assert_eq!(round_tripped.phase_enabled, state.phase_enabled);

    an_untouched_track_saves_no_selection();
}

/// A track nobody touched must not be saved as an explicit "all 256 enabled"
/// selection: the default has one representation, and a file that spells it out
/// would claim the user made a choice they never made.
///
/// Part of the test above rather than a `#[test]` of its own: tagging an
/// instance goes through one global pending token, so two tests loading plugins
/// on parallel threads can take each other's.
fn an_untouched_track_saves_no_selection() {
    let state = TrackState {
        amp_enabled: Vec::new(),
        phase_enabled: Vec::new(),
        ..state_with_disabled_harmonics()
    };
    let inst = load_tagged();
    inst.import_state(&state).expect("import");
    let exported = inst.export_state().expect("export");
    assert!(exported.amp_enabled.is_empty());
    assert!(exported.phase_enabled.is_empty());
}
