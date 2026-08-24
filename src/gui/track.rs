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
//! Instrument tracks — a hosted VST3 plugin with an open/close editor, but no
//! audio-file analysis. Two flavours are offered:
//!
//!   * **LeSynth Fourier** — the embedded internal plugin, opened in its plain
//!     (non-analysis, empty) synth mode. No `push_analysis`, so no bucket grid.
//!   * **Custom VST** — any third-party VST3, picked in [`PluginBrowser`] from
//!     the plugins installed on this machine or browsed for by hand.
//!
//! A track is lightweight metadata (name + plugin path); the heavy plugin
//! instance, its audio stream and its editor window live in [`EditorInstance`]
//! and exist only while the editor is open — closing the window tears them all
//! down, matching the resynth subtrack editors.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use eframe::egui;

use super::editor_window::{open_editor_in_thread, EditorHandle};
use super::registry::TrackRegistry;
use crate::audio::AudioEngine;
use crate::midi::{MidiEventQueue, MidiFeed, MidiRouter};
use crate::track_format::TrackState;
use crate::midi::plays_a_drum_kit;
use crate::vst::{class_ids, next_instance_token, scan_classes, validate_module, PluginInstance};

/// A live plugin editor: the loaded instance, its editor-window thread and an
/// audio stream driving `process()` so the plugin's in-GUI piano is audible.
///
/// Dropping this asks the window thread to close and joins it (so the plugin
/// view is detached before the library unloads), then stops the audio stream.
/// Conversely, when the user closes the window directly the thread sets
/// `closed`; the owner polls [`EditorInstance::is_closed`] each frame and drops
/// this to reclaim the resources.
pub struct EditorInstance {
    // Teardown order is everything here, and `Drop` below does it explicitly
    // rather than leaning on the order these fields happen to be declared in:
    // the audio stream and the editor window both hold pointers into the plugin,
    // so both must be gone before `_plugin` terminates it and unloads its
    // library.
    handle: Option<JoinHandle<()>>,
    engine: Option<AudioEngine>,
    _plugin: Arc<PluginInstance>,
    close_flag: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
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

        let engine = match AudioEngine::start(plugin.clone(), midi_queue) {
            Ok(e) => Some(e),
            Err(e) => {
                log::warn!("Track audio start failed: {e:#}");
                None
            }
        };

        Ok(EditorInstance {
            handle: Some(handle),
            engine,
            _plugin: plugin,
            close_flag,
            closed,
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

    /// Snapshot the live LeSynth grid into a [`TrackState`] for saving. Errors if
    /// the instance wasn't tagged (non-LeSynth) or has no grid.
    pub fn export_state(&self) -> Result<TrackState> {
        self._plugin.export_state()
    }

    /// The instance behind this editor, for the track registry to hold weakly —
    /// that is how the Composer plays the grid as it is being edited rather than
    /// the one the track was registered with.
    pub fn plugin(&self) -> &Arc<PluginInstance> {
        &self._plugin
    }
}

impl Drop for EditorInstance {
    fn drop(&mut self) {
        // 1) Stop the audio stream. Dropping it stops and joins the device
        //    callback, and the callback calls `process()` on this very plugin —
        //    so anything that tears the plugin down while the stream is alive is
        //    a crash on the audio thread, not a leak.
        self.engine = None;

        // 2) Ask the window thread to exit, then wait for it: it detaches the
        //    plugin view (`view.removed()`) as it unwinds, which must happen
        //    before the `_plugin` Arc unloads the library the view points into.
        //    If the user already closed the window, the thread has finished and
        //    this joins instantly.
        self.close_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }

        // 3) `_plugin` now drops with nothing else pointing into it, terminating
        //    the plugin and unloading its library.
    }
}

/// Track flavour. Only LeSynth tracks carry the harmonic grid that can be
/// exported/imported; custom VST tracks are create-only.
#[derive(Clone, Copy, PartialEq)]
enum TrackKind {
    LeSynth,
    CustomVst,
}

/// One instrument track: persistent metadata plus its editor while open.
struct PluginTrack {
    /// Stable id, used to key per-track egui widget state across frames.
    id: u64,
    /// This track's id in the shared [`TrackRegistry`], where the Composer finds
    /// it. Distinct from `id`, which is local to this panel.
    registry_id: u64,
    /// Display name (the plugin kind, or the chosen `.so` file name).
    name: String,
    kind: TrackKind,
    /// Library to (re)load whenever the editor is opened.
    plugin_path: PathBuf,
    /// Class ID to select from the factory; `None` takes the first class.
    class_id: Option<[i8; 16]>,
    /// A saved grid to push into the instance when its editor is opened (set for
    /// tracks created via "Load LeSynth Fourier Track").
    import_state: Option<TrackState>,
    /// The same for a custom VST3: its own `IComponent` state, which is where it
    /// keeps the knobs the user set. Captured when the editor closes and restored
    /// when it opens, so the sound survives the window and reaches the Composer.
    vst_state: Option<Vec<u8>>,
    /// Which keyboard plays this track, by port name. `None` follows whatever
    /// the MIDI panel is connected to, which is what every track does until it
    /// is pointed somewhere else.
    midi_source: Option<String>,
    /// This track's own feed from the router, made when its editor first opens
    /// and kept afterwards so changing the source does not mean restarting the
    /// audio engine draining it.
    feed: Option<MidiFeed>,
    editor: Option<EditorInstance>,
}

impl PluginTrack {
    /// Load the plugin, initialise it at the output device's format and open its
    /// editor. Idempotent: does nothing if the editor is already open. LeSynth
    /// tracks are tagged with a token (so their grid can be exported), and if the
    /// track carries an `import_state` it is pushed in before the window opens —
    /// otherwise LeSynth stays in its plain synth mode (no `push_analysis`).
    fn open_editor(&mut self, router: &MidiRouter) -> Result<()> {
        if self.editor.is_some() {
            return Ok(());
        }
        // Only LeSynth instances support the state export/import C ABI.
        let token = (self.kind == TrackKind::LeSynth).then(next_instance_token);
        let inst = Arc::new(PluginInstance::load(
            &self.plugin_path,
            self.class_id.as_ref(),
            token,
        )?);

        // A custom VST3's own state goes in before activation, as the spec has
        // it — and before the window opens, so the editor draws the knobs the
        // user left rather than the plugin's defaults.
        if let Some(bytes) = &self.vst_state {
            if let Err(e) = inst.set_component_state(bytes) {
                log::warn!("Track state restore failed: {e:#}");
            }
        }

        let (sr, block) = AudioEngine::query_device_config()
            .map(|c| (c.sample_rate, c.max_buffer_size as i32))
            .unwrap_or((44_100.0, 512));
        let _ = inst.initialize_audio(sr, block);

        // Push a loaded grid (Analysis mode) before the window renders it.
        if let Some(state) = &self.import_state {
            if let Err(e) = inst.import_state(state) {
                log::warn!("Track import failed: {e:#}");
            }
        }

        // One feed per track, made once and kept: the engine drains the queue it
        // was started with, so a track that changes keyboard changes what the
        // router puts *into* that queue rather than which queue it is.
        let feed = match &self.feed {
            Some(feed) => feed.queue.clone(),
            None => {
                let feed = router.subscribe(self.midi_source.clone())?;
                let queue = feed.queue.clone();
                self.feed = Some(feed);
                queue
            }
        };
        self.editor = Some(EditorInstance::open(inst, feed)?);
        Ok(())
    }

    /// Whether this track's live grid can be exported (LeSynth, editor open).
    fn can_export(&self) -> bool {
        self.kind == TrackKind::LeSynth && self.editor.is_some()
    }

    /// Take what the open editor is playing and keep it on the track, so closing
    /// the window does not throw the user's edits away.
    ///
    /// Without this the Composer would fall back to the state the track was
    /// registered with the moment an editor closed — the plugin's defaults, for
    /// a custom VST3 whose knobs had just been set by hand.
    fn capture_editor_state(&mut self, registry: &TrackRegistry) {
        let Some(editor) = &self.editor else { return };
        match self.kind {
            TrackKind::LeSynth => {
                // An instance in plain synth mode has no grid to export; that is
                // not a failure, it is a track with nothing to remember.
                if let Ok(state) = editor.export_state() {
                    registry.set_state(self.registry_id, Some(state.clone()));
                    self.import_state = Some(state);
                }
            }
            TrackKind::CustomVst => match editor.plugin().component_state() {
                Ok(bytes) if !bytes.is_empty() => {
                    registry.set_vst_state(self.registry_id, Some(bytes.clone()));
                    self.vst_state = Some(bytes);
                }
                Ok(_) => {}
                Err(e) => log::warn!("'{}' state capture failed: {e:#}", self.name),
            },
        }
    }

    /// Drop the editor if its window was closed directly, freeing the instance —
    /// keeping what it was playing first.
    fn reap_editor(&mut self, registry: &TrackRegistry) {
        if self.editor.as_ref().is_some_and(EditorInstance::is_closed) {
            self.capture_editor_state(registry);
            self.editor = None;
        }
    }
}

/// The "add a custom VST3" picker.
///
/// A VST3 is a *bundle* — a `Foo.vst3` directory with the real library buried at
/// `Contents/x86_64-linux/Foo.so` — and a file dialog cannot select a directory,
/// so "pick the plugin file" alone is a dead end for every plugin a user actually
/// has installed. This lists what is installed instead, and keeps both browse
/// buttons for anything outside the standard locations.
pub struct PluginBrowser {
    /// `(display name, path to the bundle or library)`.
    pub found: Vec<(String, PathBuf)>,
    /// Where the scan looked, shown when it found nothing.
    pub searched: Vec<PathBuf>,
    error: Option<String>,
}

impl PluginBrowser {
    pub fn scan() -> Self {
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        let searched = vst3_search_paths();
        for dir in &searched {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_bundle = path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("vst3"));
                if !is_bundle {
                    continue;
                }
                let name = path
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                found.push((name, path));
            }
        }
        found.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        found.dedup_by(|a, b| a.1 == b.1);
        // The same plugin installed in two places (a user copy and a system one)
        // would otherwise show as two identical rows; name each by its directory.
        let names: Vec<String> = found.iter().map(|(n, _)| n.clone()).collect();
        for (idx, entry) in found.iter_mut().enumerate() {
            if names.iter().enumerate().any(|(i, n)| i != idx && *n == entry.0) {
                if let Some(dir) = entry.1.parent() {
                    entry.0 = format!("{}  —  {}", entry.0, dir.display());
                }
            }
        }
        PluginBrowser {
            found,
            searched,
            error: None,
        }
    }
}

/// Whether this plugin plays a drum kit, so the Composer can name its notes.
///
/// Asks the plugin first — its VST3 subcategories are the precise answer — and
/// falls back to the name, which is what catches the many that declare
/// themselves a plain instrument. The scan opens the module and closes it again;
/// if that fails the track is still perfectly usable, so the name alone decides.
fn is_a_drum_kit(path: &std::path::Path, name: &str) -> bool {
    match scan_classes(path) {
        Ok(classes) => {
            let audio = classes
                .iter()
                .find(|c| c.category == "Audio Module Class")
                .or_else(|| classes.first());
            let by_class = audio
                .is_some_and(|c| plays_a_drum_kit(&c.name, &c.subcategories));
            by_class || plays_a_drum_kit(name, "")
        }
        Err(e) => {
            log::debug!("cannot scan {} for its category ({e:#})", path.display());
            plays_a_drum_kit(name, "")
        }
    }
}

/// The directories a VST3 is installed into, most specific first. `VST3_PATH`
/// overrides nothing — it adds to the list, as it does for other hosts.
pub fn vst3_search_paths() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(extra) = std::env::var_os("VST3_PATH") {
        dirs.extend(std::env::split_paths(&extra));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".vst3"));
    }
    #[cfg(target_os = "linux")]
    {
        dirs.push(PathBuf::from("/usr/lib/vst3"));
        dirs.push(PathBuf::from("/usr/local/lib/vst3"));
        dirs.push(PathBuf::from("/usr/lib64/vst3"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(pf) = std::env::var_os("CommonProgramFiles") {
            dirs.push(PathBuf::from(pf).join("VST3"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3"));
        }
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
    }
    dirs.retain(|d| d.is_dir());
    dirs.dedup();
    dirs
}

/// The Tracks panel: the "add" buttons and the list of instrument tracks.
pub struct TracksPanel {
    tracks: Vec<PluginTrack>,
    next_id: u64,
    status: String,
    /// The custom-VST picker, while it is open.
    browser: Option<PluginBrowser>,
    /// Where a track's keyboard feed comes from — one queue per instance, so two
    /// open editors both hear everything instead of splitting the stream.
    midi_router: MidiRouter,
    /// The app-wide track list the Composer draws its rows from. Every track
    /// created here is registered, and removed from it when it goes.
    registry: TrackRegistry,
}

impl TracksPanel {
    pub fn new(midi_router: MidiRouter, registry: TrackRegistry) -> Self {
        Self {
            tracks: Vec::new(),
            next_id: 0,
            status: "Add a LeSynth Fourier or custom VST track.".to_string(),
            browser: None,
            midi_router,
            registry,
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
        let id = self.take_id();
        let name = unique_track_name(&format!("LeSynth Fourier {}", id + 1));
        let registry_id =
            self.registry
                .add(&name, path.clone(), Some(class_ids::FOURIER_SYNTH), true, None);
        self.tracks.push(PluginTrack {
            id,
            registry_id,
            name,
            kind: TrackKind::LeSynth,
            plugin_path: path,
            class_id: Some(class_ids::FOURIER_SYNTH),
            import_state: None,
            vst_state: None,
            midi_source: None,
            feed: None,
            editor: None,
        });
        self.status = "Created LeSynth Fourier track.".to_string();
    }

    /// Add a custom VST3 track for `path` — a `.vst3` bundle or a bare library.
    ///
    /// The plugin is checked here rather than when its editor is opened: a file
    /// that is not a VST3, or one whose dependencies the loader cannot satisfy,
    /// should say so while the user is still looking at the picker.
    fn add_custom_vst_track(&mut self, path: PathBuf) -> Result<()> {
        let resolved = validate_module(&path)?;
        // Name the track after the bundle, not the library inside it: "Dexed"
        // rather than "Dexed.so", and never "libsomething.so".
        let name = path
            .file_stem()
            .or_else(|| resolved.file_stem())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // Take the first audio-module class in the factory — we don't know the
        // plugin's own class id, and a bundle may hold several classes.
        let registry_id = self.registry.add(&name, path.clone(), None, false, None);
        self.registry
            .set_percussion(registry_id, is_a_drum_kit(&path, &name));
        let id = self.take_id();
        self.tracks.push(PluginTrack {
            id,
            registry_id,
            name: name.clone(),
            kind: TrackKind::CustomVst,
            plugin_path: path,
            class_id: None,
            import_state: None,
            vst_state: None,
            midi_source: None,
            feed: None,
            editor: None,
        });
        self.status = format!("Created custom VST track '{name}'.");
        Ok(())
    }

    /// The picker window: installed plugins, plus the two browse buttons for
    /// anything that lives elsewhere.
    fn browser_ui(&mut self, ctx: &egui::Context) {
        let Some(browser) = &mut self.browser else {
            return;
        };
        let mut open = true;
        let mut chosen: Option<PathBuf> = None;
        let mut close = false;

        egui::Window::new("Add a custom VST3")
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                if browser.found.is_empty() {
                    ui.label("No VST3 plugins found in:");
                    for dir in &browser.searched {
                        ui.label(
                            egui::RichText::new(format!("  {}", dir.display()))
                                .color(egui::Color32::from_gray(160)),
                        );
                    }
                    if browser.searched.is_empty() {
                        ui.label(
                            egui::RichText::new("  (none of the standard plugin directories exist)")
                                .color(egui::Color32::from_gray(160)),
                        );
                    }
                    ui.add_space(4.0);
                    ui.label("Use the browse buttons below.");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for (name, path) in &browser.found {
                                ui.horizontal(|ui| {
                                    if ui.button("Add").clicked() {
                                        chosen = Some(path.clone());
                                    }
                                    ui.label(egui::RichText::new(name).strong())
                                        .on_hover_text(path.display().to_string());
                                });
                            }
                        });
                }

                ui.add_space(6.0);
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("📁 Browse bundle…")
                        .on_hover_text("Pick a .vst3 bundle directory")
                        .clicked()
                    {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            chosen = Some(dir);
                        }
                    }
                    if ui
                        .button("📄 Browse library…")
                        .on_hover_text("Pick a plugin library file directly")
                        .clicked()
                    {
                        if let Some(file) = rfd::FileDialog::new()
                            .add_filter("VST3 plugin", &["so", "vst3", "dll", "dylib"])
                            .add_filter("All files", &["*"])
                            .pick_file()
                        {
                            chosen = Some(file);
                        }
                    }
                    if ui.button("↻ Rescan").clicked() {
                        let refreshed = PluginBrowser::scan();
                        browser.found = refreshed.found;
                        browser.searched = refreshed.searched;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });

                if let Some(err) = &browser.error {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(err).color(egui::Color32::from_rgb(240, 140, 140)),
                    );
                }
            });

        if let Some(path) = chosen {
            match self.add_custom_vst_track(path) {
                Ok(()) => close = true,
                // Stay open with the reason on screen — the user is most likely
                // to want to pick something else straight away.
                Err(e) => {
                    if let Some(browser) = &mut self.browser {
                        browser.error = Some(format!("{e:#}"));
                    }
                }
            }
        }
        if close || !open {
            self.browser = None;
        }
    }

    /// Adopt a LeSynth Fourier track from an already-parsed grid — what loading a
    /// project does for every row that plays one. Returns its registry id.
    ///
    /// `state` is `None` for a plain synth-mode track (no grid to import).
    pub fn adopt_lesynth(&mut self, name: &str, state: Option<TrackState>) -> Result<u64> {
        let path = Self::internal_plugin_path()
            .filter(|p| p.exists())
            .context("internal LeSynth Fourier plugin not found")?;
        let id = self.take_id();
        let registry_id = self.registry.add(
            name,
            path.clone(),
            Some(class_ids::FOURIER_SYNTH),
            true,
            state.clone(),
        );
        self.tracks.push(PluginTrack {
            id,
            registry_id,
            name: name.to_string(),
            kind: TrackKind::LeSynth,
            plugin_path: path,
            class_id: Some(class_ids::FOURIER_SYNTH),
            import_state: state,
            vst_state: None,
            midi_source: None,
            feed: None,
            editor: None,
        });
        Ok(registry_id)
    }

    /// Adopt a custom VST3 track by path — the other half of loading a project.
    /// `vst_state` is the plugin's own saved state from the project, restored
    /// into every instance the track loads.
    pub fn adopt_vst(
        &mut self,
        name: &str,
        path: PathBuf,
        class_id: Option<[i8; 16]>,
        vst_state: Option<Vec<u8>>,
    ) -> Result<u64> {
        if !path.exists() {
            anyhow::bail!("plugin not found at {}", path.display());
        }
        let id = self.take_id();
        let registry_id = self.registry.add(name, path.clone(), class_id, false, None);
        self.registry.set_vst_state(registry_id, vst_state.clone());
        self.registry
            .set_percussion(registry_id, is_a_drum_kit(&path, name));
        self.tracks.push(PluginTrack {
            id,
            registry_id,
            name: name.to_string(),
            kind: TrackKind::CustomVst,
            plugin_path: path,
            class_id,
            import_state: None,
            vst_state,
            midi_source: None,
            feed: None,
            editor: None,
        });
        Ok(registry_id)
    }

    /// Drop every track and its editor — what loading a project does before it
    /// adopts the project's own.
    pub fn clear(&mut self) {
        for track in self.tracks.drain(..) {
            self.registry.remove(track.registry_id);
        }
        self.status = "Cleared for a loaded project.".to_string();
    }

    /// Load a saved `.lsft` LeSynth track: pick the file, parse it, and add a
    /// LeSynth track whose grid is pushed into the instance when its editor opens.
    fn load_lesynth_track(&mut self) {
        let Some(file) = rfd::FileDialog::new()
            .add_filter("LeSynth Fourier track (.lsft)", &["lsft"])
            .add_filter("All files", &["*"])
            .pick_file()
        else {
            return;
        };
        let state = match TrackState::read(&file) {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("Load failed: {e:#}");
                return;
            }
        };
        let Some(plugin_path) = Self::internal_plugin_path() else {
            self.status = "Could not locate the internal plugin.".to_string();
            return;
        };
        if !plugin_path.exists() {
            self.status = format!("Internal plugin not found at {}", plugin_path.display());
            return;
        }
        let name = unique_track_name(
            &file
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "LeSynth track".to_string()),
        );
        let id = self.take_id();
        // The saved grid is registered with the track, so the Composer can play
        // it without the editor ever being opened.
        let registry_id = self.registry.add(
            &name,
            plugin_path.clone(),
            Some(class_ids::FOURIER_SYNTH),
            true,
            Some(state.clone()),
        );
        self.tracks.push(PluginTrack {
            id,
            registry_id,
            name,
            kind: TrackKind::LeSynth,
            plugin_path,
            class_id: Some(class_ids::FOURIER_SYNTH),
            import_state: Some(state),
            vst_state: None,
            midi_source: None,
            feed: None,
            editor: None,
        });
        self.status = "Loaded LeSynth track — open its editor to view.".to_string();
    }

    /// Save a track's live grid to a chosen `.lsft` file.
    fn export_track(&mut self, idx: usize) {
        // Snapshot first (ends the borrow of `self.tracks` before the dialog).
        let snapshot = self
            .tracks
            .get(idx)
            .filter(|t| t.can_export())
            .map(|t| (t.name.clone(), t.editor.as_ref().unwrap().export_state()));
        let Some((name, result)) = snapshot else {
            return;
        };
        let state = match result {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("Export failed: {e:#}");
                return;
            }
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("LeSynth Fourier track (.lsft)", &["lsft"])
            .set_file_name(format!("{name}.lsft"))
            .save_file()
        else {
            return;
        };
        self.status = match state.write(&path) {
            Ok(()) => format!("Exported {name}."),
            Err(e) => format!("Export failed: {e:#}"),
        };
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("➕ Create LeSynth Fourier Track").clicked() {
                self.add_lesynth_track();
            }
            if ui
                .button("➕ Create Custom VST Track")
                .on_hover_text("Pick an installed VST3, or browse for one")
                .clicked()
            {
                self.browser = Some(PluginBrowser::scan());
            }
            if ui
                .button("➕ Load LeSynth Fourier Track")
                .on_hover_text("Open a saved .lsft track")
                .clicked()
            {
                self.load_lesynth_track();
            }
        });
        // Before the early return below: the picker is a window of its own and
        // has to be drawn whether or not there are any tracks yet.
        self.browser_ui(ui.ctx());

        ui.add_space(4.0);
        ui.label(egui::RichText::new(&self.status).color(egui::Color32::from_gray(170)));

        if self.tracks.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);

        // Reap editors the user closed directly, so button state and resources
        // stay honest, and keep the registry's view of which instances are live
        // in step with them — that is what lets the Composer play a grid the
        // user is editing right now.
        for track in &mut self.tracks {
            track.reap_editor(&self.registry);
            self.registry.set_live(
                track.registry_id,
                track.editor.as_ref().map(|e| Arc::downgrade(e.plugin())),
            );
        }

        // Deferred actions, so we don't mutate a track while iterating.
        enum Action {
            Open(usize),
            Close(usize),
            Export(usize),
            Remove(usize),
        }
        let mut action: Option<Action> = None;

        // The ports on offer, read once for the whole list rather than per row:
        // asking the driver is not free, and every row offers the same answer.
        let ports = crate::midi::list_midi_ports().unwrap_or_default();
        let default_port = self.midi_router.default_port();
        // Mutable: a row's keyboard picker writes straight into its track.
        let mut retarget: Option<(usize, Option<String>)> = None;
        for (idx, track) in self.tracks.iter_mut().enumerate() {
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
                                    // Export the live grid (LeSynth only, editor open).
                                    if track.can_export()
                                        && ui
                                            .button("💾 Export…")
                                            .on_hover_text("Save this track's grid to a .lsft file")
                                            .clicked()
                                    {
                                        action = Some(Action::Export(idx));
                                    }
                                },
                            );
                        });

                        // Which keyboard plays *this* instance. Each track has a
                        // feed of its own, so two open editors both hear
                        // everything their keyboard sends instead of taking
                        // turns at one queue — and a machine with two keyboards
                        // on it can play them into two instruments at once.
                        ui.horizontal(|ui| {
                            ui.label("MIDI in:");
                            let mut want = track.midi_source.clone();
                            let label = match &want {
                                Some(port) => port.clone(),
                                None => match &default_port {
                                    Some(p) => format!("default ({p})"),
                                    None => "default (not connected)".to_string(),
                                },
                            };
                            egui::ComboBox::from_id_salt(("midi_in", track.id, ports.len()))
                                .width(300.0)
                                .height(9.0 * 44.0)
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut want,
                                        None,
                                        "— default (the MIDI panel's port) —",
                                    );
                                    for port in &ports {
                                        ui.selectable_value(
                                            &mut want,
                                            Some(port.clone()),
                                            port,
                                        );
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "The keyboard that plays this track's instance.\n\n\
                                     Every instance has a feed of its own, so two open \
                                     editors both hear everything rather than splitting \
                                     one stream between them, and two keyboards can play \
                                     two instruments at once.\n\n\
                                     “Default” follows whatever the MIDI panel is \
                                     connected to. Takes effect on the next note — \
                                     nothing is reopened, and the audio keeps running.",
                                );
                            if want != track.midi_source {
                                retarget = Some((idx, want));
                            }
                        });
                    });
            });
            ui.add_space(6.0);
        }

        // Applied after the loop: pointing a feed somewhere else talks to the
        // router, which the rows above are still borrowing.
        if let Some((idx, want)) = retarget {
            if let Some(track) = self.tracks.get_mut(idx) {
                let feed_id = track.feed.as_ref().map(|f| f.id);
                match feed_id.map_or(Ok(()), |id| self.midi_router.set_source(id, want.clone())) {
                    Ok(()) => {
                        track.midi_source = want;
                        // A track whose editor has never been opened has no feed
                        // yet; the choice is remembered and used when it is.
                        self.status = match &track.midi_source {
                            Some(port) => format!("{} now plays from {port}.", track.name),
                            None => format!("{} follows the MIDI panel's port.", track.name),
                        };
                    }
                    Err(e) => self.status = format!("Could not switch MIDI source: {e:#}"),
                }
            }
        }
        // An editor that has been closed since the last frame may have been the
        // last listener on its keyboard; this is what hands the device back.
        self.midi_router.release_unused();

        match action {
            Some(Action::Open(idx)) => {
                let router = self.midi_router.clone();
                if let Some(track) = self.tracks.get_mut(idx) {
                    if let Err(e) = track.open_editor(&router) {
                        self.status = format!("Open editor failed: {e:#}");
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
                    track.capture_editor_state(&self.registry);
                    track.editor = None;
                }
            }
            Some(Action::Export(idx)) => self.export_track(idx),
            Some(Action::Remove(idx)) => {
                if idx < self.tracks.len() {
                    // Nothing to capture: the track is going, and with it the
                    // registry entry any state would have been kept in.
                    let removed = self.tracks.remove(idx);
                    // Composer rows pointing here fall back to another track on
                    // their next frame.
                    self.registry.remove(removed.registry_id);
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

/// A track name nothing else can already be called: the readable part, then a
/// fresh UUID.
///
/// Two LeSynth tracks with the same name are not a cosmetic problem. A saved
/// project writes one `.lsft` per track named after the track, a row records
/// which track it plays by that name, and the Composer's select boxes are read
/// by it — so a duplicate means a grid file quietly overwritten by another
/// track's, and a row that cannot say which of the two it wanted. The counter in
/// front of the UUID is what a person reads; the UUID is what makes the
/// guarantee, including across projects and across machines.
pub fn unique_track_name(stem: &str) -> String {
    format!("{stem} · {}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two tracks made the same way must never be called the same thing: a
    /// saved project names each grid file after its track, so a duplicate is a
    /// grid overwritten by another track's.
    #[test]
    fn a_fresh_track_name_is_never_a_repeat() {
        let names: std::collections::HashSet<String> =
            (0..64).map(|_| unique_track_name("LeSynth Fourier 1")).collect();
        assert_eq!(names.len(), 64);
        // The readable part still leads, so a select box shows what the track is
        // before it shows which one it is.
        assert!(
            unique_track_name("LeSynth Fourier 1").starts_with("LeSynth Fourier 1 · "),
            "the name no longer reads as a name"
        );
        // And it survives being turned into a file name.
        let name = unique_track_name("Voice");
        assert_eq!(
            crate::gui::composer::project::sanitize_name(&name).matches('-').count(),
            4,
            "the UUID did not survive sanitising: {name}"
        );
    }
}
