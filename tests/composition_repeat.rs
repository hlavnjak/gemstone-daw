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
//! **Repeat**: the transport loops the composition instead of stopping at the
//! end, it picks up edits made while it is looping, and unticking it lets the
//! pass in flight finish.
//!
//! The loop lives inside the audio callback, so this drives a real output
//! stream. With no audio device there is nothing to drive and the test says so
//! and passes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gemstone_daw::gui::composer::player::{CompositionPlayer, PlannedNote, RowEdit, RowPlan};
use gemstone_daw::gui::registry::PlaybackSource;
use gemstone_daw::track_format::TrackState;
use gemstone_daw::vst::class_ids;

/// The composition loops on this, not on the last note plus its release.
const LOOP_SECS: f64 = 1.0;

/// A grid a key can actually be rendered from: a decaying harmonic series over
/// four buckets. A track row carries one of these; an instance without one has
/// an empty grid and every key renders silence, so a test that skipped it would
/// be measuring nothing.
fn grid() -> TrackState {
    let (nh, nb) = (256usize, 4usize);
    let mut amplitude = vec![0.0f32; nh * nb];
    for h in 0..8 {
        for b in 0..nb {
            amplitude[h * nb + b] = 0.4 / (h + 1) as f32;
        }
    }
    TrackState {
        num_harmonics: nh,
        num_buckets: nb,
        base_freq: 220.0,
        duration_secs: 0.5,
        sample_rate: 44_100.0,
        display_gain: 1.0,
        amplitude,
        phase: vec![0.0; nh * nb],
        pitch_ratio: vec![1.0; nb],
        bucket_lengths: vec![200; nb],
        dc: vec![0.0; nb],
        nyquist: vec![0.0; nb],
        amp_enabled: Vec::new(),
        phase_enabled: Vec::new(),
    }
}

fn lesynth_source() -> PlaybackSource {
    PlaybackSource {
        name: "LeSynth".to_string(),
        plugin_path: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("internal_plugins")
            .join("liblesynth_fourier.so"),
        class_id: Some(class_ids::FOURIER_SYNTH),
        is_lesynth: true,
        state: Some(grid()),
        vst_state: None,
    }
}


#[test]
fn repeat_loops_the_composition_until_it_is_switched_off() {
    let plans = vec![RowPlan {
        row_id: 0,
        source: lesynth_source(),
        gain: 1.0,
        notes: vec![
            PlannedNote { at_secs: 0.0, dur_secs: 0.4, pitch: 60 },
            PlannedNote { at_secs: 0.5, dur_secs: 0.4, pitch: 64 },
        ],
    }];

    let repeat = Arc::new(AtomicBool::new(true));
    let player = match CompositionPlayer::start(plans, LOOP_SECS, repeat.clone()) {
        Ok(p) => p,
        Err(e) => {
            println!("no audio device to play through ({e:#}) — nothing to test");
            return;
        }
    };
    assert_eq!(player.loaded_rows, 1, "the row must have loaded");
    // The loop is the written length; playback without a repeat would run on to
    // the last note-off plus the release tail, which is longer.
    assert!(
        player.total_secs > player.loop_secs(),
        "loop {:.2}s vs total {:.2}s",
        player.loop_secs(),
        player.total_secs
    );

    // Watch it round the loop at least once, and never past the loop point.
    let mut wrapped = false;
    let mut previous = 0.0;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        let position = player.position_secs();
        // A tolerance of one device block: the position is read between blocks.
        assert!(
            position <= player.loop_secs() + 0.25,
            "position {position:.2}s ran past the {:.2}s loop",
            player.loop_secs()
        );
        assert!(!player.is_finished(), "a looping transport reported finished");
        wrapped |= position < previous;
        previous = position;
        if wrapped && Instant::now() > deadline - Duration::from_secs(3) {
            break;
        }
    }
    assert!(wrapped, "never wrapped in 6s of a {LOOP_SECS}s loop");

    // An edit made mid-loop is taken up at the next loop point — here a longer
    // composition, which moves the loop length the transport reports.
    let edited = LOOP_SECS * 2.0;
    player.update_live(
        &[RowEdit {
            row_id: 0,
            gain: 1.0,
            notes: vec![
                PlannedNote { at_secs: 0.0, dur_secs: 0.4, pitch: 67 },
                PlannedNote { at_secs: 1.2, dur_secs: 0.4, pitch: 72 },
            ],
        }],
        edited,
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline && (player.loop_secs() - edited).abs() > 0.01 {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        (player.loop_secs() - edited).abs() <= 0.01,
        "the live edit never landed: still looping on {:.2}s",
        player.loop_secs()
    );

    // Unticking lets the pass in flight play out, then stops.
    repeat.store(false, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && !player.is_finished() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        player.is_finished(),
        "unticking Repeat left the transport running"
    );
}
