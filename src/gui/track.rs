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
//! Instrument tracks — a hosted VST3 plugin with an open/close editor, but no
//! audio-file analysis. Two flavours are offered:
//!
//!   * **LeSynth Fourier** — the embedded internal plugin, opened in its plain
//!     (non-analysis, empty) synth mode. No `push_analysis`, so no bucket grid.
//!   * **Custom VST** — an arbitrary VST3 `.so` chosen from a file dialog.
//!
//! A track is lightweight metadata (name + plugin path); the heavy plugin
//! instance, its audio stream and its editor window live in [`EditorInstance`]
//! and exist only while the editor is open — closing the window tears them all
//! down, matching the resynth subtrack editors.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Result;
use eframe::egui;

use super::editor_window::{open_editor_in_thread, EditorHandle};
use crate::audio::AudioEngine;
use crate::midi::MidiEventQueue;
use crate::vst::{class_ids, PluginInstance};

/// A live plugin editor: the loaded instance, its editor-window thread and an
/// audio stream driving `process()` so the plugin's in-GUI piano is audible.
///
/// Dropping this asks the window thread to close and joins it (so the plugin
/// view is detached before the library unloads), then stops the audio stream.
/// Conversely, when the user closes the window directly the thread sets
/// `closed`; the owner polls [`EditorInstance::is_closed`] each frame and drops
/// this to reclaim the resources.
pub struct EditorInstance {
    // Field order matters for `Drop`: after `drop()` joins the window thread,
    // fields drop top-to-bottom, so `_plugin` (which unloads the shared library
    // the thread's view lives in) must come *after* `handle`.
    handle: Option<JoinHandle<()>>,
    _plugin: Arc<PluginInstance>,
    close_flag: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    engine: Option<AudioEngine>,
}

impl EditorInstance {
    /// Open an editor window for `plugin` and start an audio stream (fed by
    /// `midi_queue`) so it is audible. The plugin must already be initialised.
    /// Fails only if the editor window itself cannot be created; an unavailable
    /// audio device merely leaves the instance silent.
    pub fn open(plugin: Arc<PluginInstance>, midi_queue: MidiEventQueue) -> Result<Self> {
        let EditorHandle {
            handle,
            close_flag,
            closed,
        } = open_editor_in_thread(&plugin)?;

        let engine = match AudioEngine::start(plugin.processor.clone(), midi_queue) {
            Ok(e) => Some(e),
            Err(e) => {
                log::warn!("Track audio start failed: {}", e);
                None
            }
        };

        Ok(EditorInstance {
            handle: Some(handle),
            _plugin: plugin,
            close_flag,
            closed,
            engine,
        })
    }

    /// True once the editor window has gone away — whether the user closed it or
    /// we asked it to via `close_flag`.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Whether an audio stream is driving the instance (false if no device).
    pub fn is_audible(&self) -> bool {
        self.engine.is_some()
    }
}

impl Drop for EditorInstance {
    fn drop(&mut self) {
        // Ask the window thread to exit, then wait for it: it detaches the plugin
        // view (`view.removed()`) as it unwinds, which must happen before the
        // `_plugin` Arc below unloads the library the view points into. If the
        // user already closed the window, the thread has finished and this joins
        // instantly.
        self.close_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One instrument track: persistent metadata plus its editor while open.
struct PluginTrack {
    /// Stable id, used to key per-track egui widget state across frames.
    id: u64,
    /// Display name (the plugin kind, or the chosen `.so` file name).
    name: String,
    /// Library to (re)load whenever the editor is opened.
    plugin_path: PathBuf,
    /// Class ID to select from the factory; `None` takes the first class.
    class_id: Option<[i8; 16]>,
    editor: Option<EditorInstance>,
}

impl PluginTrack {
    /// Load the plugin, initialise it at the output device's format and open its
    /// editor. Idempotent: does nothing if the editor is already open. LeSynth
    /// stays in its plain synth mode here — no `push_analysis` is issued.
    fn open_editor(&mut self, midi_queue: &MidiEventQueue) -> Result<()> {
        if self.editor.is_some() {
            return Ok(());
        }
        let inst = Arc::new(PluginInstance::load(&self.plugin_path, self.class_id.as_ref())?);

        let (sr, block) = AudioEngine::query_device_config()
            .map(|c| (c.sample_rate, c.max_buffer_size as i32))
            .unwrap_or((44_100.0, 512));
        let _ = inst.initialize_audio(sr, block);

        self.editor = Some(EditorInstance::open(inst, midi_queue.clone())?);
        Ok(())
    }

    /// Drop the editor if its window was closed directly, freeing the instance.
    fn reap_editor(&mut self) {
        if self.editor.as_ref().is_some_and(EditorInstance::is_closed) {
            self.editor = None;
        }
    }
}

/// The Tracks panel: the two "add" buttons and the list of instrument tracks.
pub struct TracksPanel {
    tracks: Vec<PluginTrack>,
    next_id: u64,
    status: String,
    /// Shared MIDI queue, so a connected keyboard plays the open track editors.
    midi_queue: MidiEventQueue,
}

impl TracksPanel {
    pub fn new(midi_queue: MidiEventQueue) -> Self {
        Self {
            tracks: Vec::new(),
            next_id: 0,
            status: "Add a LeSynth Fourier or custom VST track.".to_string(),
            midi_queue,
        }
    }

    #[cfg(target_os = "linux")]
    const INTERNAL_LIB: &'static str = "liblesynth_fourier.so";
    #[cfg(target_os = "macos")]
    const INTERNAL_LIB: &'static str = "liblesynth_fourier.dylib";
    #[cfg(target_os = "windows")]
    const INTERNAL_LIB: &'static str = "lesynth_fourier.dll";

    fn internal_plugin_path() -> Option<PathBuf> {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join("internal_plugins").join(Self::INTERNAL_LIB))
    }

    /// Add a LeSynth Fourier track (internal plugin, plain synth mode).
    fn add_lesynth_track(&mut self) {
        let Some(path) = Self::internal_plugin_path() else {
            self.status = "Could not locate the internal plugin.".to_string();
            return;
        };
        if !path.exists() {
            self.status = format!("Internal plugin not found at {}", path.display());
            return;
        }
        let track = PluginTrack {
            id: self.take_id(),
            name: "LeSynth Fourier".to_string(),
            plugin_path: path,
            class_id: Some(class_ids::FOURIER_SYNTH),
            editor: None,
        };
        self.tracks.push(track);
        self.status = "Added LeSynth Fourier track.".to_string();
    }

    /// Add a custom VST track from a `.so` chosen in a file dialog.
    fn add_custom_vst_track(&mut self) {
        let dialog = rfd::FileDialog::new()
            .add_filter("VST3 plugin (.so)", &["so"])
            .add_filter("All files", &["*"]);
        let Some(path) = dialog.pick_file() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let track = PluginTrack {
            id: self.take_id(),
            name,
            plugin_path: path,
            // Take the first class in the factory — we don't know the plugin's ID.
            class_id: None,
            editor: None,
        };
        self.tracks.push(track);
        self.status = "Added custom VST track.".to_string();
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("➕ Add LeSynth Fourier Track").clicked() {
                self.add_lesynth_track();
            }
            if ui.button("➕ Add Custom VST Track").clicked() {
                self.add_custom_vst_track();
            }
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new(&self.status).color(egui::Color32::from_gray(170)));

        if self.tracks.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);

        // Reap editors the user closed directly, so button state and resources
        // stay honest.
        for track in &mut self.tracks {
            track.reap_editor();
        }

        // Deferred actions, so we don't mutate a track while iterating.
        enum Action {
            Open(usize),
            Close(usize),
            Remove(usize),
        }
        let mut action: Option<Action> = None;

        for (idx, track) in self.tracks.iter().enumerate() {
            // Scope widget ids by the stable track id, so buttons keep their
            // identity when tracks above them are removed and indices shift.
            ui.push_id(track.id, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_gray(34))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&track.name).strong());
                            if track.editor.is_some() {
                                ui.label(
                                    egui::RichText::new("● editor open")
                                        .color(egui::Color32::from_rgb(130, 230, 150)),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button("🗑 Remove")
                                        .on_hover_text("Remove this track (closes its editor)")
                                        .clicked()
                                    {
                                        action = Some(Action::Remove(idx));
                                    }
                                    if track.editor.is_some() {
                                        if ui.button("✖ Close editor").clicked() {
                                            action = Some(Action::Close(idx));
                                        }
                                    } else if ui.button("Open editor").clicked() {
                                        action = Some(Action::Open(idx));
                                    }
                                },
                            );
                        });
                    });
            });
            ui.add_space(6.0);
        }

        match action {
            Some(Action::Open(idx)) => {
                let queue = self.midi_queue.clone();
                if let Some(track) = self.tracks.get_mut(idx) {
                    if let Err(e) = track.open_editor(&queue) {
                        self.status = format!("Open editor failed: {}", e);
                    } else if track.editor.as_ref().is_some_and(|e| !e.is_audible()) {
                        self.status =
                            format!("Opened {} — audio output unavailable.", track.name);
                    } else {
                        self.status = String::new();
                    }
                }
            }
            Some(Action::Close(idx)) => {
                if let Some(track) = self.tracks.get_mut(idx) {
                    track.editor = None;
                }
            }
            Some(Action::Remove(idx)) => {
                if idx < self.tracks.len() {
                    let removed = self.tracks.remove(idx);
                    self.status = format!("Removed {}.", removed.name);
                }
            }
            None => {}
        }

        // While any editor is open, poll a few times a second so a window the
        // user closes directly is reaped promptly; otherwise stay idle.
        if self.tracks.iter().any(|t| t.editor.is_some()) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}
