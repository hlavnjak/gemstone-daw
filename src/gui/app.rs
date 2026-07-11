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
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

use crate::audio::AudioEngine;
use crate::midi::{self, MidiEventQueue};
use crate::vst::{class_ids, PluginInstance};

use super::resynth::ResynthPanel;

const DEFAULT_PLUGIN_PATH: &str =
    "/home/kuba/Programming/Fine_Ware_SW/lesynth-fourier/target/x86_64-unknown-linux-gnu/release/liblesynth_fourier.so";

pub struct DawApp {
    plugin_path: String,
    plugin_status: String,
    midi_status: String,
    midi_ports: Vec<String>,
    selected_midi_port: Option<String>,
    usb_keyboards: Vec<String>,
    selected_usb_keyboard: Option<String>,

    // Runtime state
    plugin: Option<Arc<PluginInstance>>,
    audio_engine: Option<AudioEngine>,
    midi_queue: MidiEventQueue,
    _midi_connection: Option<midir::MidiInputConnection<()>>,

    // Resynthesis (.wav/.mp3/.m4a → LeSynth Fourier analysis)
    resynth: ResynthPanel,
}

impl Default for DawApp {
    fn default() -> Self {
        Self {
            plugin_path: DEFAULT_PLUGIN_PATH.to_string(),
            plugin_status: "No plugin loaded".to_string(),
            midi_status: "Disconnected".to_string(),
            midi_ports: Vec::new(),
            selected_midi_port: None,
            usb_keyboards: Vec::new(),
            selected_usb_keyboard: None,
            plugin: None,
            audio_engine: None,
            midi_queue: midi::new_midi_queue(),
            _midi_connection: None,
            resynth: ResynthPanel::default(),
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
    fn configure_style(ctx: &egui::Context) {
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

    fn do_load_plugin(&mut self, path: &std::path::Path) {
        log::info!("Loading plugin from: {:?}", path);
        match PluginInstance::load(path, Some(&class_ids::FOURIER_SYNTH)) {
            Ok(instance) => {
                log::info!("Plugin loaded OK");
                let plugin = Arc::new(instance);

                match AudioEngine::query_device_config() {
                    Ok(cfg) => {
                        if let Err(e) =
                            plugin.initialize_audio(cfg.sample_rate, cfg.max_buffer_size as i32)
                        {
                            self.plugin_status = format!("Audio init failed: {}", e);
                            self.plugin = Some(plugin);
                            return;
                        }

                        match AudioEngine::start(plugin.processor.clone(), self.midi_queue.clone()) {
                            Ok(engine) => {
                                self.plugin_status = format!(
                                    "Plugin loaded & playing ({}Hz, {} ch)",
                                    engine.config.sample_rate as u32,
                                    engine.config.channels
                                );
                                self.audio_engine = Some(engine);
                            }
                            Err(e) => {
                                self.plugin_status = format!("Audio start failed: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        self.plugin_status = format!("No audio device: {}", e);
                    }
                }

                self.plugin = Some(plugin);
            }
            Err(e) => {
                log::error!("Plugin load FAILED: {}", e);
                self.plugin_status = format!("Load failed: {}", e);
            }
        }
    }

    fn load_internal_plugin(&mut self) {
        #[cfg(target_os = "linux")]
        let lib_name = "liblesynth_fourier.so";
        #[cfg(target_os = "macos")]
        let lib_name = "liblesynth_fourier.dylib";
        #[cfg(target_os = "windows")]
        let lib_name = "lesynth_fourier.dll";

        match std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join("internal_plugins").join(lib_name))
        {
            Some(path) => self.do_load_plugin(&path),
            None => {
                self.plugin_status = "Could not determine working directory".to_string();
            }
        }
    }

    fn unload_plugin(&mut self) {
        self.audio_engine = None;
        self.plugin = None;
        self.plugin_status = "No plugin loaded".to_string();
    }

    fn show_editor(&mut self) {
        if let Some(ref plugin) = self.plugin {
            log::info!("Opening plugin editor...");
            match super::editor_window::open_editor_in_thread(plugin) {
                Ok((_handle, _close_flag)) => {
                    self.plugin_status = "Editor opened".to_string();
                }
                Err(e) => {
                    log::error!("Editor error: {}", e);
                    self.plugin_status = format!("Editor failed: {}", e);
                }
            }
        } else {
            self.plugin_status = "Load a plugin first".to_string();
        }
    }

    fn connect_midi(&mut self) {
        // Prefer USB keyboard selection, fall back to general MIDI port
        let port_filter = self
            .selected_usb_keyboard
            .clone()
            .or_else(|| self.selected_midi_port.clone());
        match midi::spawn_midi_thread(self.midi_queue.clone(), port_filter.as_deref()) {
            Ok(conn) => {
                self.midi_status = format!(
                    "Connected: {}",
                    port_filter.unwrap_or_else(|| "port 0".into())
                );
                self._midi_connection = Some(conn);
            }
            Err(e) => {
                self.midi_status = format!("MIDI error: {}", e);
            }
        }
    }

    fn plugin_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "Plugin", |ui| {
            ui.horizontal(|ui| {
                ui.label("Path:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.plugin_path)
                        .hint_text("Plugin path…")
                        .desired_width(f32::INFINITY),
                );
            });
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Load").clicked() {
                    let path = PathBuf::from(self.plugin_path.clone());
                    self.do_load_plugin(&path);
                }
                if ui.button("Load Internal — LeSynth Fourier").clicked() {
                    self.load_internal_plugin();
                }
                if ui.button("Show Editor").clicked() {
                    self.show_editor();
                }
                if ui.button("Unload").clicked() {
                    self.unload_plugin();
                }
            });
            ui.add_space(2.0);
            Self::status_label(ui, &self.plugin_status);
        });
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
                    egui::ComboBox::from_id_salt("usb_keyboard")
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
                    egui::ComboBox::from_id_salt("midi_port")
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
}

impl eframe::App for DawApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keep repainting so the panels stay live.
        ctx.request_repaint();

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
                    self.plugin_section(ui);
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