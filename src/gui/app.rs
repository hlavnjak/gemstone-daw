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
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        app.refresh_midi_ports();
        app
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
        ui.heading("Plugin");
        ui.horizontal(|ui| {
            ui.label("Path:");
            ui.add(
                egui::TextEdit::singleline(&mut self.plugin_path)
                    .hint_text("Plugin path...")
                    .desired_width(f32::INFINITY),
            );
        });
        ui.horizontal(|ui| {
            if ui.button("Load").clicked() {
                let path = PathBuf::from(self.plugin_path.clone());
                self.do_load_plugin(&path);
            }
            if ui.button("Load Internal - lesynth fourier").clicked() {
                self.load_internal_plugin();
            }
            if ui.button("Show Editor").clicked() {
                self.show_editor();
            }
            if ui.button("Unload").clicked() {
                self.unload_plugin();
            }
        });
        ui.label(&self.plugin_status);
    }

    fn midi_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("MIDI");
        ui.horizontal(|ui| {
            // USB keyboard picker
            let usb_label = self
                .selected_usb_keyboard
                .clone()
                .unwrap_or_else(|| "Select USB keyboard...".to_string());
            egui::ComboBox::from_id_salt("usb_keyboard")
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

            // General MIDI port picker
            let port_label = self
                .selected_midi_port
                .clone()
                .unwrap_or_else(|| "Select MIDI port...".to_string());
            egui::ComboBox::from_id_salt("midi_port")
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

            if ui.button("Connect").clicked() {
                self.connect_midi();
            }
            if ui.button("Refresh").clicked() {
                self.refresh_midi_ports();
            }
        });
        ui.label(&self.midi_status);
    }
}

impl eframe::App for DawApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keep repainting so the panels stay live.
        ctx.request_repaint();

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.plugin_section(ui);
                ui.add_space(20.0);
                self.midi_section(ui);
                ui.add_space(20.0);
                self.resynth.ui(ui);
            });
        });
    }
}