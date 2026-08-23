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
//! `vst3_probe <path>` — load a VST3 the way the DAW does and report what the
//! host sees: the resolved module, the factory's classes, the bus layout, and
//! whether the plugin offers an editor.
//!
//! This is the tool to reach for when a third-party plugin "does not open": it
//! prints the same error the Tracks panel would show, with nothing else in the
//! way. With `--editor [seconds]` it also opens the plugin's editor window, the
//! same way the Tracks panel does, and closes it again.

use std::path::PathBuf;

use std::sync::atomic::Ordering;

use gemstone_daw::audio::AudioEngine;
use gemstone_daw::gui::editor_window::open_editor_in_thread;
use gemstone_daw::gui::track::EditorInstance;
use gemstone_daw::midi::new_midi_queue;
use gemstone_daw::vst::{resolve_module_path, PluginInstance};

fn main() {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("usage: vst3_probe <plugin.vst3 | plugin.so>");
        std::process::exit(2);
    };
    env_logger_fallback();

    let path = PathBuf::from(arg);
    match resolve_module_path(&path) {
        Ok(resolved) => println!("module:   {}", resolved.display()),
        Err(e) => {
            println!("module:   unresolved — {e:#}");
        }
    }

    let plugin = match PluginInstance::load(&path, None, None) {
        Ok(p) => p,
        Err(e) => {
            println!("load:     FAILED — {e:#}");
            std::process::exit(1);
        }
    };
    println!("loaded:   '{}'", plugin.name());

    // The device's own format, exactly as the Tracks panel does it: a plugin set
    // up for a smaller block than the stream later hands it writes past the
    // buffers it sized, which looks like a crash in the host.
    let (rate, block) = AudioEngine::query_device_config()
        .map(|c| (c.sample_rate, c.max_buffer_size as i32))
        .unwrap_or((48_000.0, 512));
    if let Err(e) = plugin.initialize_audio(rate, block) {
        println!("audio:    FAILED — {e:#}");
    } else {
        let io = plugin.io();
        println!(
            "audio:    {rate} Hz, up to {block} frames, inputs {:?}, outputs {:?}",
            io.inputs, io.outputs
        );
    }

    match plugin.create_view() {
        Some(_) => println!("editor:   yes"),
        None => println!("editor:   none (the plugin has no GUI)"),
    }

    // `--editor [seconds]`: open the real window, so an editor that attaches but
    // never draws (the classic missing-run-loop symptom) is visible here too.
    // `--window` and `--audio` open only half of that, which is how a crash in
    // teardown is narrowed down to one of them.
    let mut args = std::env::args().skip(2);
    let mode = args.next().unwrap_or_default();
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let hold = |what: &str, closed: &dyn Fn() -> bool| {
        println!("{what}: holding for {secs}s");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline && !closed() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        println!("{what}: tearing down");
    };

    match mode.as_str() {
        "--editor" => {
            let plugin = std::sync::Arc::new(plugin);
            match EditorInstance::open(plugin, new_midi_queue()) {
                Ok(editor) => {
                    println!("window:   open (audible: {})", editor.is_audible());
                    hold("window", &|| editor.is_closed());
                }
                Err(e) => println!("window:   FAILED — {e:#}"),
            }
        }
        // Editor window, no audio stream.
        "--window" => match open_editor_in_thread(&plugin) {
            Ok(handle) => {
                hold("window", &|| handle.closed.load(Ordering::Relaxed));
                handle.close_flag.store(true, Ordering::Relaxed);
                let _ = handle.handle.join();
            }
            Err(e) => println!("window:   FAILED — {e:#}"),
        },
        // Audio stream, no editor window.
        "--audio" => match AudioEngine::start(std::sync::Arc::new(plugin), new_midi_queue()) {
            Ok(engine) => {
                hold("audio", &|| false);
                drop(engine);
            }
            Err(e) => println!("audio:    FAILED — {e:#}"),
        },
        _ => {}
    }
}

/// Route the crate's `log` output to stderr, so the class list shows up.
fn env_logger_fallback() {
    let _ = fern::Dispatch::new()
        .level(log::LevelFilter::Info)
        .chain(std::io::stderr())
        .apply();
}
