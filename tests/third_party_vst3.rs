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
//! Hosting **other people's** VST3 plugins, against whatever is installed on
//! this machine. Nothing here is about our own plugin: it is about the parts of
//! a host that only a third-party plugin exercises — a bundle instead of a bare
//! library, `ModuleEntry`, a factory with a class to choose from, and above all a
//! bus layout that is not "no inputs, one stereo output".
//!
//! That last one is why the render half matters: a plugin with an audio input
//! bus reads `ProcessData::inputs` whether or not the host meant to send
//! anything, so a host that hard-codes the layout does not fail politely — it
//! reads pointers nobody provided.
//!
//! With no VST3 installed there is nothing to test and the test says so and
//! passes; run it with `--nocapture` to see what it found.

use gemstone_daw::gui::composer::player::{render_offline, PlannedNote, RowPlan};
use gemstone_daw::gui::registry::PlaybackSource;
use gemstone_daw::gui::track::PluginBrowser;
use gemstone_daw::vst::{validate_module, PluginInstance};

const RATE: f64 = 44_100.0;
const CHANNELS: usize = 2;
/// The release `player` renders past the last note-off.
const TAIL_SECS: f64 = 1.5;
/// Enough to prove the path without walking a large plugin collection.
const MAX_PLUGINS: usize = 8;

#[test]
fn installed_vst3_plugins_load_and_render() {
    let browser = PluginBrowser::scan();
    if browser.found.is_empty() {
        println!(
            "no VST3 plugins installed in {:?} — nothing to test",
            browser.searched
        );
        return;
    }

    for (name, path) in browser.found.iter().take(MAX_PLUGINS) {
        // 1) The picker's own check: a resolvable module with a VST3 entry point.
        let module = validate_module(path)
            .unwrap_or_else(|e| panic!("'{name}' ({}) did not validate: {e:#}", path.display()));
        assert!(module.is_file(), "'{name}' resolved to {}", module.display());

        // 2) A real instance, initialised the way a track's editor does.
        let plugin = PluginInstance::load(path, None, None)
            .unwrap_or_else(|e| panic!("'{name}' failed to load: {e:#}"));
        plugin
            .initialize_audio(RATE, 512)
            .unwrap_or_else(|e| panic!("'{name}' failed to initialize: {e:#}"));
        let io = plugin.io();
        assert!(
            !io.outputs.is_empty(),
            "'{name}' negotiated no audio output bus at all"
        );
        drop(plugin);

        // 3) Two notes through the Composer's offline render — the path that
        //    hands `process()` its buffers, for exactly this bus layout.
        let plan = RowPlan {
            row_id: 0,
            source: PlaybackSource {
                name: name.clone(),
                plugin_path: path.clone(),
                class_id: None,
                is_lesynth: false,
                state: None,
                vst_state: None,
            },
            gain: 1.0,
            notes: vec![
                PlannedNote { at_secs: 0.0, dur_secs: 0.5, pitch: 60 },
                PlannedNote { at_secs: 0.5, dur_secs: 0.5, pitch: 64 },
            ],
        };
        let (samples, loaded, total) = render_offline(vec![plan], RATE, CHANNELS)
            .unwrap_or_else(|e| panic!("'{name}' failed to render: {e:#}"));
        assert_eq!(loaded, total, "'{name}' did not load for the render");
        let expected = ((1.0 + TAIL_SECS) * RATE).round() as usize * CHANNELS;
        assert_eq!(samples.len(), expected, "'{name}' rendered the wrong length");

        // An instrument should make a sound; an effect with no input has nothing
        // to make one from, so the level is reported, not asserted.
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        println!(
            "{name}: in={:?} out={:?} peak={peak:.4}",
            io.inputs, io.outputs
        );
    }
}
