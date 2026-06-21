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
//! Resynthesis panel — the headline feature.
//!
//! Flow:
//!   1. Pick an arbitrary `.wav` / `.mp3` / `.m4a` file.
//!   2. Decode it and segment it into pitch-stable *subtracks*
//!      (host-side, see [`crate::analysis`]).
//!   3. For each *reasonable* subtrack, hand it to LeSynth Fourier, which
//!      subdivides it into per-period *buckets* and extracts an
//!      amplitude/phase value per harmonic per bucket (the FFT step).
//!
//! The analysed grid is previewed inline here (a compact amplitude chart), and
//! can also be opened in a full LeSynth Fourier editor instance running in
//! Analysis mode, where individual harmonics can be toggled.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;

use eframe::egui;

use crate::analysis::{self, Subtrack};
use crate::audio::{decode_audio_file, AudioEngine, DecodedAudio};
use crate::midi::new_midi_queue;
use crate::vst::{class_ids, PluginInstance};

/// Number of harmonics we extract / preview per subtrack.
const PREVIEW_HARMONICS: usize = 16;
/// Bucket count requested for the inline host-side preview.
const PREVIEW_BUCKETS: usize = 128;
/// Resolution of the pitch contour handed to LeSynth Fourier per subtrack.
/// Vibrato is slow, so a few hundred points capture it with room to spare.
const CONTOUR_POINTS: usize = 256;

/// Uniformly-resampled fundamental (absolute Hz) across a subtrack's span, for
/// the analysis bridge. Lets the plugin follow vibrato/drift instead of a single
/// global pitch. Empty when the subtrack carries no pitch track (→ flat).
fn build_contour(sub: &Subtrack) -> Vec<f32> {
    if sub.pitch_track.is_empty() {
        return Vec::new();
    }
    let len = sub.len().max(1);
    (0..CONTOUR_POINTS)
        .map(|i| {
            let off = (i as f32 + 0.5) / CONTOUR_POINTS as f32 * len as f32;
            sub.freq_at(sub.start + off as usize)
        })
        .collect()
}

struct SubtrackView {
    sub: Subtrack,
    /// `[harmonic][bucket]` amplitude grid for the inline preview.
    preview_amp: Option<Vec<Vec<f32>>>,
    analyzed: bool,
}

pub struct ResynthPanel {
    file_path: String,
    status: String,
    decoded: Option<DecodedAudio>,
    subtracks: Vec<SubtrackView>,
    /// Shared library handle used for the stateless analysis FFI calls.
    ffi_plugin: Option<Arc<PluginInstance>>,
    /// Editor instances opened for subtracks, kept alive. The optional
    /// [`AudioEngine`] drives that instance's `process()` so the in-editor piano
    /// is audible — without it the dedicated instance is never pulled and stays
    /// silent.
    editors: Vec<(Arc<PluginInstance>, JoinHandle<()>, Arc<AtomicBool>, Option<AudioEngine>)>,
}

impl Default for ResynthPanel {
    fn default() -> Self {
        Self {
            file_path: String::new(),
            status: "Pick a .wav, .mp3 or .m4a file to begin.".to_string(),
            decoded: None,
            subtracks: Vec::new(),
            ffi_plugin: None,
            editors: Vec::new(),
        }
    }
}

impl ResynthPanel {
    fn internal_plugin_path() -> Option<PathBuf> {
        #[cfg(target_os = "linux")]
        let lib_name = "liblesynth_fourier.so";
        #[cfg(target_os = "macos")]
        let lib_name = "liblesynth_fourier.dylib";
        #[cfg(target_os = "windows")]
        let lib_name = "lesynth_fourier.dll";

        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join("internal_plugins").join(lib_name))
    }

    /// Load (once) a plugin instance whose shared object backs the analysis
    /// FFI. Returns a clone of the loaded instance.
    fn ensure_ffi_plugin(&mut self) -> Option<Arc<PluginInstance>> {
        if let Some(p) = &self.ffi_plugin {
            return Some(p.clone());
        }
        let path = Self::internal_plugin_path()?;
        match PluginInstance::load(&path, Some(&class_ids::FOURIER_SYNTH)) {
            Ok(inst) => {
                let arc = Arc::new(inst);
                self.ffi_plugin = Some(arc.clone());
                Some(arc)
            }
            Err(e) => {
                self.status = format!("Could not load internal plugin for analysis: {}", e);
                None
            }
        }
    }

    /// Open a native file picker and put the chosen path into `file_path`.
    fn browse_for_file(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Audio (.wav, .mp3, .m4a)", &["wav", "mp3", "m4a"])
            .add_filter("All files", &["*"]);
        // Start from the directory of the current entry, if any.
        let current = PathBuf::from(self.file_path.trim());
        if let Some(parent) = current.parent().filter(|p| p.is_dir()) {
            dialog = dialog.set_directory(parent);
        }
        if let Some(path) = dialog.pick_file() {
            self.file_path = path.display().to_string();
            self.status = format!("Selected {}", self.file_path);
        }
    }

    fn decode_and_segment(&mut self) {
        let path = PathBuf::from(self.file_path.trim());
        match decode_audio_file(&path) {
            Ok(audio) => {
                let subs = analysis::segment(&audio.samples, audio.sample_rate);
                let reasonable = subs
                    .iter()
                    .filter(|s| s.is_reasonable(audio.sample_rate))
                    .count();
                self.status = format!(
                    "Decoded {:.1}s @ {} Hz → {} subtrack(s), {} reasonable to analyse.",
                    audio.duration_secs(),
                    audio.sample_rate as u32,
                    subs.len(),
                    reasonable
                );
                self.subtracks = subs
                    .into_iter()
                    .map(|sub| SubtrackView {
                        sub,
                        preview_amp: None,
                        analyzed: false,
                    })
                    .collect();
                self.decoded = Some(audio);
            }
            Err(e) => {
                self.status = format!("Decode failed: {}", e);
                self.decoded = None;
                self.subtracks.clear();
            }
        }
    }

    /// Compute the inline preview grid for subtrack `idx` via the plugin's
    /// stateless analysis FFI.
    fn preview_subtrack(&mut self, idx: usize) {
        let (samples, sr, freq, contour) = {
            let Some(audio) = &self.decoded else { return };
            let Some(view) = self.subtracks.get(idx) else { return };
            let s = &view.sub;
            (
                audio.samples[s.start..s.end.min(audio.samples.len())].to_vec(),
                audio.sample_rate,
                s.base_freq,
                build_contour(s),
            )
        };
        let Some(plugin) = self.ensure_ffi_plugin() else { return };
        match plugin.analyze(&samples, sr, freq, &contour, PREVIEW_BUCKETS, PREVIEW_HARMONICS) {
            Ok((amp, _phase)) => {
                if let Some(view) = self.subtracks.get_mut(idx) {
                    view.preview_amp = Some(amp);
                }
            }
            Err(e) => self.status = format!("Analyze failed: {}", e),
        }
    }

    /// Open a dedicated LeSynth Fourier instance for subtrack `idx`, push the
    /// audio for analysis, and show its editor (Analysis mode).
    fn open_in_lesynth(&mut self, idx: usize) {
        let (samples, sr, freq, contour) = {
            let Some(audio) = &self.decoded else { return };
            let Some(view) = self.subtracks.get(idx) else { return };
            let s = &view.sub;
            (
                audio.samples[s.start..s.end.min(audio.samples.len())].to_vec(),
                audio.sample_rate,
                s.base_freq,
                build_contour(s),
            )
        };

        let Some(path) = Self::internal_plugin_path() else {
            self.status = "Could not locate internal plugin.".to_string();
            return;
        };
        let inst = match PluginInstance::load(&path, Some(&class_ids::FOURIER_SYNTH)) {
            Ok(i) => Arc::new(i),
            Err(e) => {
                self.status = format!("Plugin load failed: {}", e);
                return;
            }
        };
        // Initialise the instance at the *output device* sample rate (not the
        // file's) so its piano renders at the correct pitch for playback. The
        // analysis itself carries the file's `sr` independently via
        // `push_analysis`, so this only affects synthesis/playback.
        let device_cfg = AudioEngine::query_device_config().ok();
        let (init_sr, init_block) = device_cfg
            .as_ref()
            .map(|c| (c.sample_rate, c.max_buffer_size as i32))
            .unwrap_or((sr as f64, 512));
        let _ = inst.initialize_audio(init_sr, init_block);

        // Queue the subtrack, then open the editor which will claim it.
        if let Err(e) = inst.push_analysis(&samples, sr, freq, &contour) {
            self.status = format!("Push analysis failed: {}", e);
            return;
        }

        match super::editor_window::open_editor_in_thread(&inst) {
            Ok((handle, close_flag)) => {
                // Drive this dedicated instance's `process()` with its own audio
                // stream so the in-editor piano is audible. The piano writes
                // voices directly into the instance's shared state, so an empty
                // MIDI queue is fine here.
                let engine = match AudioEngine::start(inst.processor.clone(), new_midi_queue()) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        log::warn!("Resynth instance audio start failed: {}", e);
                        None
                    }
                };
                let audible = engine.is_some();
                self.editors.push((inst, handle, close_flag, engine));
                if let Some(view) = self.subtracks.get_mut(idx) {
                    view.analyzed = true;
                }
                self.status = if audible {
                    format!("Opened subtrack {} in LeSynth (Analysis mode).", idx + 1)
                } else {
                    format!(
                        "Opened subtrack {} in LeSynth (Analysis mode) — audio output unavailable.",
                        idx + 1
                    )
                };
            }
            Err(e) => self.status = format!("Editor failed: {}", e),
        }
    }

    fn draw_amp_preview(ui: &mut egui::Ui, grid: &[Vec<f32>]) {
        let (rect, _resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 90.0), egui::Sense::hover());
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 2.0, egui::Color32::from_gray(20));
        let buckets = grid.first().map(|r| r.len()).unwrap_or(0);
        if buckets < 2 {
            return;
        }
        let palette = [
            egui::Color32::from_rgb(120, 200, 255),
            egui::Color32::from_rgb(255, 170, 120),
            egui::Color32::from_rgb(150, 255, 150),
            egui::Color32::from_rgb(255, 130, 200),
            egui::Color32::from_rgb(230, 230, 130),
        ];
        for (h, row) in grid.iter().enumerate().take(5) {
            let color = palette[h % palette.len()];
            let pts: Vec<egui::Pos2> = row
                .iter()
                .enumerate()
                .map(|(b, &v)| {
                    let x = rect.left() + rect.width() * b as f32 / (buckets - 1) as f32;
                    let y = rect.bottom() - rect.height() * v.clamp(0.0, 1.0);
                    egui::pos2(x, y)
                })
                .collect();
            painter.add(egui::Shape::line(pts, egui::Stroke::new(1.0, color)));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Resynthesis (.wav / .mp3 / .m4a → LeSynth Fourier)");
        ui.horizontal(|ui| {
            ui.label("File:");
            if ui.button("Browse…").clicked() {
                self.browse_for_file();
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.file_path)
                    .hint_text("/path/to/audio.wav")
                    .desired_width(f32::INFINITY),
            );
        });
        ui.horizontal(|ui| {
            if ui.button("Decode & Segment").clicked() {
                self.decode_and_segment();
            }
            if ui.button("Close all editors").clicked() {
                for (_, _, flag, _) in &self.editors {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // Dropping the editors also drops their AudioEngines, stopping
                // each instance's audio stream.
                self.editors.clear();
            }
        });
        ui.label(&self.status);
        ui.add_space(8.0);

        let sr = self.decoded.as_ref().map(|d| d.sample_rate).unwrap_or(44_100.0);
        let count = self.subtracks.len();
        let mut to_preview: Option<usize> = None;
        let mut to_open: Option<usize> = None;

        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for idx in 0..count {
                    let (start, dur, freq, conf, reasonable, analyzed) = {
                        let v = &self.subtracks[idx];
                        (
                            v.sub.start,
                            v.sub.duration_secs(sr),
                            v.sub.base_freq,
                            v.sub.confidence,
                            v.sub.is_reasonable(sr),
                            v.analyzed,
                        )
                    };
                    let fill = if reasonable {
                        egui::Color32::from_rgb(28, 42, 38)
                    } else {
                        egui::Color32::from_gray(34)
                    };
                    egui::Frame::new()
                        .fill(fill)
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("Subtrack {}", idx + 1)).strong(),
                                );
                                ui.label(format!(
                                    "{:.0} Hz · {:.2}s · conf {:.2} · @{:.2}s",
                                    freq,
                                    dur,
                                    conf,
                                    start as f32 / sr
                                ));
                                if !reasonable {
                                    ui.label(
                                        egui::RichText::new("(skipped — not pitched enough)")
                                            .italics()
                                            .color(egui::Color32::from_gray(160)),
                                    );
                                } else if analyzed {
                                    ui.label(
                                        egui::RichText::new("● in LeSynth")
                                            .color(egui::Color32::from_rgb(130, 230, 150)),
                                    );
                                }
                            });
                            if reasonable {
                                ui.horizontal(|ui| {
                                    if ui.button("Preview FFT").clicked() {
                                        to_preview = Some(idx);
                                    }
                                    if ui.button("Open in LeSynth").clicked() {
                                        to_open = Some(idx);
                                    }
                                });
                                if let Some(grid) = &self.subtracks[idx].preview_amp {
                                    Self::draw_amp_preview(ui, grid);
                                }
                            }
                        });
                    ui.add_space(4.0);
                }
            });

        if let Some(idx) = to_preview {
            self.preview_subtrack(idx);
        }
        if let Some(idx) = to_open {
            self.open_in_lesynth(idx);
        }
    }
}