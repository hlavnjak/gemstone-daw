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
use std::sync::atomic::Ordering;

use anyhow::Context;
use eframe::egui;

use crate::midi::{self, MidiRouter, OctaveShift, MAX_OCTAVE_SHIFT};

use super::composer::project::{self, Project, TrackSource};
use super::composer::{ComposerPanel, ProjectRequest};
use super::registry::TrackRegistry;
use super::resynth::ResynthPanel;
use super::track::TracksPanel;

pub struct DawApp {
    midi_status: String,
    midi_ports: Vec<String>,
    selected_midi_port: Option<String>,
    usb_keyboards: Vec<String>,
    selected_usb_keyboard: Option<String>,

    // Runtime state
    /// One keyboard per plugin instance: which port feeds what, and the
    /// connections behind them. Replaces the single shared queue every editor
    /// used to drain — two editors on it each heard half of what was played.
    midi_router: MidiRouter,
    /// How far the keyboard is transposed on the way in. Shared with the input
    /// thread, so the picker moves a connection that is already open.
    octave_shift: OctaveShift,

    // Instrument tracks (LeSynth Fourier / custom VST), each with its own editor.
    tracks: TracksPanel,
    // Arrange the registered tracks on a timeline and play them.
    composer: ComposerPanel,
    // Resynthesis (.wav/.mp3/.m4a → LeSynth Fourier analysis)
    resynth: ResynthPanel,
    // The shared track list, kept here too so saving a project can read every
    // track — including subtracks published straight from Resynthesis, which
    // never pass through the Tracks panel.
    registry: TrackRegistry,
    /// Wav tracks a loaded project asked for. They play a file, so they have no
    /// plugin, no editor and no place in the Tracks panel — but something has to
    /// take them back out of the registry when the next project replaces them,
    /// the way [`TracksPanel::clear`] does for its own. A wav track published in
    /// this session from Resynthesis is *not* in here: that panel owns it, and
    /// loading a project leaves it alone.
    adopted_wavs: Vec<u64>,
}

impl Default for DawApp {
    fn default() -> Self {
        let midi_taps = midi::new_midi_taps();
        let octave_shift = midi::new_octave_shift();
        let midi_router = MidiRouter::new(octave_shift.clone(), midi_taps.clone());
        // The one track list the panels share: Tracks and Resynthesis publish
        // into it, the Composer builds its rows from it.
        let registry = TrackRegistry::default();
        Self {
            midi_status: "Disconnected".to_string(),
            midi_ports: Vec::new(),
            selected_midi_port: None,
            usb_keyboards: Vec::new(),
            selected_usb_keyboard: None,
            tracks: TracksPanel::new(midi_router.clone(), registry.clone()),
            composer: ComposerPanel::new(registry.clone(), midi_taps.clone()),
            midi_router,
            octave_shift,
            resynth: ResynthPanel::new(registry.clone()),
            registry,
            adopted_wavs: Vec::new(),
        }
    }
}

impl DawApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::configure_style(&cc.egui_ctx);
        let mut app = Self::default();
        app.refresh_midi_ports();
        app
    }

    /// Apply a consistent, slightly roomier look across the whole app.
    ///
    /// `pub(crate)` so a test that measures a panel's layout can lay it out
    /// under the text sizes and spacing the user actually sees: egui's defaults
    /// are smaller, and a card that fits under them can still overflow here.
    pub(crate) fn configure_style(ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.indent = 16.0;
        // Bump the default body/heading text a touch for legibility.
        use egui::{FontFamily::Proportional, FontId, TextStyle};
        style.text_styles = [
            (TextStyle::Heading, FontId::new(18.0, Proportional)),
            (TextStyle::Body, FontId::new(14.0, Proportional)),
            (TextStyle::Button, FontId::new(14.0, Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, egui::FontFamily::Monospace)),
            (TextStyle::Small, FontId::new(11.0, Proportional)),
        ]
        .into();
        ctx.set_style(style);
    }

    /// Draw a titled "card": a bordered group with a heading and a separator,
    /// used to give every top-level section the same framed look.
    fn section<R>(
        ui: &mut egui::Ui,
        title: &str,
        add_contents: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.heading(title);
                });
                ui.separator();
                ui.add_space(2.0);
                add_contents(ui)
            })
            .inner
    }

    /// Colour a status line by its apparent sentiment (error / success / neutral).
    fn status_color(status: &str) -> egui::Color32 {
        let lower = status.to_ascii_lowercase();
        if ["fail", "error", "unavailable", "could not", "no "]
            .iter()
            .any(|k| lower.contains(k))
        {
            egui::Color32::from_rgb(230, 120, 110)
        } else if ["loaded", "playing", "connected", "opened", "decoded", "removed"]
            .iter()
            .any(|k| lower.contains(k))
        {
            egui::Color32::from_rgb(130, 210, 150)
        } else {
            egui::Color32::from_gray(170)
        }
    }

    /// A status line rendered with sentiment colouring.
    fn status_label(ui: &mut egui::Ui, status: &str) {
        ui.label(egui::RichText::new(status).color(Self::status_color(status)));
    }

    fn refresh_midi_ports(&mut self) {
        self.midi_ports = midi::input::list_midi_ports().unwrap_or_default();
        self.usb_keyboards = midi::list_usb_midi_keyboards().unwrap_or_default();
    }

    fn connect_midi(&mut self) {
        // Prefer USB keyboard selection, fall back to general MIDI port
        let port_filter = self
            .selected_usb_keyboard
            .clone()
            .or_else(|| self.selected_midi_port.clone());
        // The picker offers the ports as the driver names them, so a chosen one
        // is a name to connect by; with nothing chosen, the first port there is.
        let port = match port_filter.or_else(|| self.midi_ports.first().cloned()) {
            Some(p) => p,
            None => {
                self.midi_status = "No MIDI input ports found.".to_string();
                return;
            }
        };
        match self.midi_router.set_default_port(Some(port.clone())) {
            Ok(()) => self.midi_status = format!("Connected: {port}"),
            Err(e) => self.midi_status = format!("MIDI error: {e:#}"),
        }
    }

    fn midi_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "MIDI", |ui| {
            // Lay the two device pickers out in a grid so their labels and
            // combo boxes share aligned columns instead of drifting out of line
            // when placed side by side in a wrapping row.
            egui::Grid::new("midi_devices")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    // USB keyboard picker
                    ui.label("USB keyboard:");
                    let usb_label = self
                        .selected_usb_keyboard
                        .clone()
                        .unwrap_or_else(|| "Select USB keyboard…".to_string());
                    // The list length is part of the id: a `ComboBox` popup is
                    // an `egui::Area`, and an `Area` measures itself only on the
                    // first pass it is shown for a given id, then reuses that
                    // size forever — so a picker first opened with two entries
                    // clips away everything a later rescan finds. See the
                    // Composer's track select box for the long version.
                    egui::ComboBox::from_id_salt(("usb_keyboard", self.usb_keyboards.len()))
                        .width(260.0)
                        .selected_text(usb_label)
                        .show_ui(ui, |ui| {
                            if self.usb_keyboards.is_empty() {
                                ui.label("No USB keyboards detected");
                            }
                            for kb in self.usb_keyboards.clone() {
                                ui.selectable_value(
                                    &mut self.selected_usb_keyboard,
                                    Some(kb.clone()),
                                    kb,
                                );
                            }
                        });
                    ui.end_row();

                    // General MIDI port picker
                    ui.label("MIDI port:");
                    let port_label = self
                        .selected_midi_port
                        .clone()
                        .unwrap_or_else(|| "Select MIDI port…".to_string());
                    egui::ComboBox::from_id_salt(("midi_port", self.midi_ports.len()))
                        .width(260.0)
                        .selected_text(port_label)
                        .show_ui(ui, |ui| {
                            for port in self.midi_ports.clone() {
                                ui.selectable_value(
                                    &mut self.selected_midi_port,
                                    Some(port.clone()),
                                    port,
                                );
                            }
                        });
                    ui.end_row();

                    // Where the keyboard's keys land. A small controller — a
                    // Keystation Mini 32 starts at C3 — has no low notes on it
                    // at all, so a bass part has to be played up here and moved
                    // down. Applied to the incoming MIDI itself, so what the
                    // editors play and what the Composer records are the same
                    // note.
                    ui.label("Octave shift:");
                    let mut shift = self.octave_shift.load(Ordering::Relaxed);
                    egui::ComboBox::from_id_salt("midi_octave")
                        .width(260.0)
                        // Tall enough for all nine steps at this style's row
                        // height: the default caps a popup at 200 px and scrolls
                        // the rest, which hides the very octaves a small
                        // controller is here to reach.
                        .height(9.0 * 44.0)
                        .selected_text(octave_shift_label(shift))
                        .show_ui(ui, |ui| {
                            for step in (-MAX_OCTAVE_SHIFT..=MAX_OCTAVE_SHIFT).rev() {
                                ui.selectable_value(
                                    &mut shift,
                                    step,
                                    octave_shift_label(step),
                                );
                            }
                        })
                        .response
                        .on_hover_text(
                            "Move every note the keyboard sends by whole octaves \
                             before anything plays it.\n\nA key already held keeps \
                             the shift it was pressed with, so moving this mid-note \
                             cannot leave one sounding for good; a note that would \
                             fall off either end of MIDI is not sent.\n\nThe wheel, \
                             the pedal and everything else pass through untouched.",
                        );
                    self.octave_shift.store(shift, Ordering::Relaxed);
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Connect").clicked() {
                    self.connect_midi();
                }
                if ui.button("Refresh").clicked() {
                    self.refresh_midi_ports();
                }
            });
            ui.add_space(2.0);
            Self::status_label(ui, &self.midi_status);
        });
    }
    /// Save or load a project — see [`ProjectRequest`].
    fn perform_project_request(&mut self, request: ProjectRequest) {
        let status = match request {
            ProjectRequest::Save { dir, name } => match self.save_project(&dir, &name) {
                Ok(n) => {
                    self.composer.set_project_dir(dir.clone(), name);
                    format!("Saved to {} ({n} grid file(s)).", crate::file_label(&dir))
                }
                Err(e) => format!("Save failed: {e:#}"),
            },
            ProjectRequest::Load { file } => match self.load_project(&file) {
                Ok(()) => return, // `apply_project` sets its own status.
                Err(e) => format!("Load failed: {e:#}"),
            },
        };
        self.composer.set_status(status);
    }

    /// Write the project folder: the manifest, one `.lsft` for every LeSynth
    /// Fourier track a row plays, and one `.vststate` for every custom VST3 —
    /// the plugin's own state, which is where its knobs live. Returns how many
    /// sound files were written.
    ///
    /// A file is re-written when any row playing that track has autosave on;
    /// otherwise an existing one is left as it is, which is what pins a sound. A
    /// track with nothing to save (a plain synth-mode LeSynth, or a plugin that
    /// gives back no state) writes no file — the manifest records what it is
    /// instead.
    fn save_project(&mut self, dir: &std::path::Path, name: &str) -> anyhow::Result<usize> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create the folder {}", crate::file_label(dir)))?;

        let autosave: Vec<u64> = self.composer.autosave_track_ids();
        let mut sources: std::collections::HashMap<u64, TrackSource> = Default::default();
        let mut taken: Vec<String> = Vec::new();
        let mut written = 0usize;

        // One file per distinct track, however many rows share it. Read through
        // the registry rather than the Tracks panel: a subtrack published from
        // Resynthesis is a real playable track that never appears there, and
        // asking the panel would silently drop its sound from the project.
        for (id, track_name) in self.registry.list() {
            // `playback_source` snapshots the live editor when one is open, so
            // what gets written is what the user can currently hear.
            let Some(src) = self.registry.playback_source(id) else { continue };
            // A wav track is a path. Like a third-party plugin's binary the file
            // stays where it is — it is the user's recording, and usually larger
            // than everything else in the folder together.
            if let Some(path) = src.wav {
                sources.insert(id, TrackSource::Wav { path });
                continue;
            }
            if !src.is_lesynth {
                // A third-party plugin's binary stays where it is — but the state
                // it is playing does not live in the plugin, it lives in the
                // instance, so the project keeps a copy beside the manifest.
                let mut state_file = None;
                if let Some(bytes) = &src.vst_state {
                    let file = project::unique_file_name(&track_name, "vststate", &taken);
                    let full = dir.join(&file);
                    // Same rule as a grid: autosave off with a file already there
                    // means "keep what is pinned".
                    if autosave.contains(&id) || !full.exists() {
                        std::fs::write(&full, bytes)
                            .with_context(|| format!("write {}", crate::file_label(&full)))?;
                        written += 1;
                    }
                    taken.push(file.clone());
                    state_file = Some(file);
                }
                sources.insert(
                    id,
                    TrackSource::Vst {
                        path: src.plugin_path,
                        class_id: src.class_id,
                        state: state_file,
                    },
                );
                continue;
            }
            let Some(state) = src.state else {
                // A LeSynth track in plain synth mode: nothing to save, and the
                // manifest says so rather than pointing at a file that is not there.
                sources.insert(id, TrackSource::LeSynthDefault);
                continue;
            };
            let file = project::grid_file_name(&track_name, &taken);
            let full = dir.join(&file);
            // Autosave off with a file already there means "keep what is pinned".
            if autosave.contains(&id) || !full.exists() {
                state
                    .write(&full)
                    .with_context(|| format!("write {}", crate::file_label(&full)))?;
                written += 1;
            }
            taken.push(file.clone());
            sources.insert(id, TrackSource::LeSynth { file });
        }

        let project = self.composer.to_project(name, |track_id| {
            track_id
                .and_then(|id| sources.get(&id).cloned())
                .unwrap_or(TrackSource::None)
        });
        project.write(&dir.join(format!("{name}.{}", project::EXTENSION)))?;
        Ok(written)
    }

    /// Read a project folder back: adopt every track it names into the Tracks
    /// panel, then hand the composition to the Composer with one resolved
    /// registry id per row (`None` where the source could not be found).
    fn load_project(&mut self, file: &std::path::Path) -> anyhow::Result<()> {
        let project = Project::read(file)?;
        let dir = file
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        // The loaded project replaces what is open, so the tracks it brings are
        // the only ones left — otherwise the previous project's tracks would sit
        // in the list looking like part of this one. The wav tracks the *last*
        // load brought go with them; the ones Resynthesis is holding open stay,
        // because that panel is still showing the file they play.
        self.tracks.clear();
        for id in self.adopted_wavs.drain(..) {
            self.registry.remove(id);
        }

        // Distinct sources first, so rows sharing a track share one instance.
        let mut adopted: Vec<(TrackSource, Option<u64>)> = Vec::new();
        let mut resolved = Vec::with_capacity(project.rows.len());
        for row in &project.rows {
            if let Some((_, id)) = adopted.iter().find(|(src, _)| *src == row.source) {
                resolved.push(*id);
                continue;
            }
            let id = self.adopt_source(&row.source, &dir, &row.track_name);
            adopted.push((row.source.clone(), id));
            resolved.push(id);
        }
        self.composer.apply_project(&project, dir, &resolved);
        Ok(())
    }

    /// Bind one project source to a track, or `None` when it cannot be found —
    /// a deleted `.lsft`, or a VST that has moved. The row then shows what is
    /// missing and the user picks a replacement.
    fn adopt_source(
        &mut self,
        source: &TrackSource,
        dir: &std::path::Path,
        name: &str,
    ) -> Option<u64> {
        let name = if name.is_empty() { "Track" } else { name };
        match source {
            TrackSource::None => None,
            TrackSource::LeSynthDefault => self.tracks.adopt_lesynth(name, None).ok(),
            TrackSource::LeSynth { file } => {
                let state = crate::track_format::TrackState::read(&dir.join(file)).ok()?;
                self.tracks.adopt_lesynth(name, Some(state)).ok()
            }
            TrackSource::Wav { path } => {
                if !path.exists() {
                    log::warn!("'{name}': audio file {} is missing", path.display());
                    return None;
                }
                // One track per file: a file already open in Resynthesis, or
                // named by two rows, is the track that is already registered
                // rather than a second one indistinguishable from it.
                if let Some(id) = self.registry.find_wav(path) {
                    return Some(id);
                }
                let id = self.registry.add_wav(name, path.clone());
                self.adopted_wavs.push(id);
                Some(id)
            }
            TrackSource::Vst { path, class_id, state } => {
                // A state file that has gone missing is not fatal: the track
                // loads, and the plugin comes up on its own defaults.
                let bytes = state.as_ref().and_then(|file| {
                    let full = dir.join(file);
                    match std::fs::read(&full) {
                        Ok(b) => Some(b),
                        Err(e) => {
                            log::warn!("'{name}': cannot read {} ({e})", full.display());
                            None
                        }
                    }
                });
                self.tracks.adopt_vst(name, path.clone(), *class_id, bytes).ok()
            }
        }
    }
}

impl eframe::App for DawApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Reactive repaint: nothing in this window changes without user input, so
        // let eframe/winit idle and wake on real events (egui still self-requests
        // repaints for its own hover/tooltip/scroll animations). The Tracks and
        // Resynthesis panels each request a low-frequency repaint while they hold
        // an open editor, so windows the user closes directly are reaped promptly.

        // App title bar.
        egui::TopBottomPanel::top("title_bar")
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("💎 Gemstone DAW")
                            .heading()
                            .strong()
                            .color(egui::Color32::from_rgb(150, 200, 255)),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("additive resynthesis workstation")
                            .italics()
                            .color(egui::Color32::from_gray(150)),
                    );
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    Self::section(ui, "Tracks", |ui| {
                        self.tracks.ui(ui);
                    });
                    ui.add_space(14.0);
                    Self::section(ui, "Track Composer", |ui| {
                        self.composer.ui(ui);
                    });
                    // Saving reads the grids out of the live plugin instances and
                    // loading puts tracks into the Tracks panel, so the request is
                    // performed here rather than inside the Composer, which owns
                    // neither. Deferred past the draw so the panel is not mutated
                    // mid-frame.
                    if let Some(request) = self.composer.take_request() {
                        self.perform_project_request(request);
                    }
                    ui.add_space(14.0);
                    self.midi_section(ui);
                    ui.add_space(14.0);
                    Self::section(ui, "Resynthesis", |ui| {
                        self.resynth.ui(ui);
                    });
                    ui.add_space(10.0);
                });
        });
    }
}

/// How an octave shift reads in its select box: signed, and saying what it does
/// rather than only how far.
fn octave_shift_label(shift: i32) -> String {
    match shift {
        0 => "none (as played)".to_string(),
        1 => "+1 octave up".to_string(),
        -1 => "−1 octave down".to_string(),
        n if n > 0 => format!("+{n} octaves up"),
        n => format!("−{} octaves down", -n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn d5() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("D5.wav")
    }

    /// A wav track survives a project the way every other track does — except
    /// that what is saved is the path to the file, since the recording is not
    /// ours to copy into the folder. Loading it back must bind the row to a
    /// track that plays that file, and saving again must write the same path.
    #[test]
    fn a_wav_row_saves_as_a_path_and_loads_back_onto_the_file() {
        let dir = std::env::temp_dir().join("gemstone-wav-project");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let manifest = dir.join(format!("Take.{}", project::EXTENSION));
        std::fs::write(
            &manifest,
            format!(
                "gemstone-project 1\nname = Take\ntempo = 120\n\n\
                 [row]\ntrack = take.wav\nsource = wav {}\ngain = 1\n\
                 lead = 0 none\nnote = 60 0 1/4 0 1/8\n",
                d5().display()
            ),
        )
        .expect("write the manifest");

        let mut app = DawApp::default();
        app.load_project(&manifest).expect("loads");

        // One track, playing the file, and the row is on it — not left showing a
        // missing source.
        let tracks = app.registry.list();
        assert_eq!(tracks.len(), 1, "{tracks:?}");
        let id = app.registry.find_wav(&d5()).expect("the file is a track");
        assert!(app.registry.is_wav(id));

        // And saving writes the path back out — through the app, which is what
        // decides what a track's source is.
        let written = app.save_project(&dir, "Take").expect("saves");
        assert_eq!(written, 0, "a wav track writes no file into the folder");
        let text = std::fs::read_to_string(&manifest).expect("re-read");
        assert!(
            text.contains(&format!("source = wav {}", d5().display())),
            "the path did not survive the round trip: {text}"
        );

        // Loading again must not leave the first load's track behind: one file
        // is one track, however many times a project names it.
        app.load_project(&manifest).expect("loads again");
        assert_eq!(app.registry.list().len(), 1, "{:?}", app.registry.list());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A project naming a file that is not there must say so rather than bind
    /// the row to nothing quietly: the row keeps what it was looking for, which
    /// is what puts "⚠ missing" in its select box.
    #[test]
    fn a_missing_audio_file_leaves_the_row_asking_for_it() {
        let dir = std::env::temp_dir().join("gemstone-wav-project-missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let manifest = dir.join(format!("Gone.{}", project::EXTENSION));
        std::fs::write(
            &manifest,
            "gemstone-project 1\nname = Gone\n\n\
             [row]\ntrack = gone.wav\nsource = wav /nonexistent/gone.wav\n\
             note = 60 0 1/4 0 none\n",
        )
        .expect("write the manifest");

        let mut app = DawApp::default();
        app.load_project(&manifest).expect("loads");
        assert!(app.registry.list().is_empty(), "nothing to play, so no track");
        let saved = app.composer.to_project("Gone", |_| TrackSource::None);
        assert_eq!(
            saved.rows[0].source,
            TrackSource::Wav { path: PathBuf::from("/nonexistent/gone.wav") },
            "a save after a failed load must not lose the file it was looking for"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
