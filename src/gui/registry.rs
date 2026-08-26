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
//! The one list of **Tracks** every panel agrees on.
//!
//! A Track is a playable sound source — an instrument track from the Tracks
//! panel or a subtrack published from Resynthesis. Composer rows map onto one by
//! id, so the registry, not any single panel, is what "all Tracks" means.
//!
//! Entries are *recipes*, not live plugins: a library path, a class id, and the
//! state to load into a fresh instance — LeSynth's harmonic grid, or any other
//! plugin's own `IComponent` state — so a composition plays whether or not the
//! track's editor is open. When it is, [`TrackEntry::live`] points at that
//! instance, so the Composer can snapshot what is being edited right now.
//!
//! One kind of track is not a plugin at all: a **wav track** ([`TrackEntry::wav`])
//! is an audio file, played back whole as one note. Its recipe is the path, and
//! nothing else — there is no instance to load, no state to import and no editor
//! to snapshot.
//!
//! GUI-thread only (`Rc<RefCell<_>>`) — every user of it is an egui panel.

use std::cell::{Ref, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Weak;

use crate::track_format::TrackState;
use crate::vst::PluginInstance;

/// One registered Track.
pub struct TrackEntry {
    /// Stable id. Composer rows store this, so a row survives tracks being added
    /// and removed around it.
    pub id: u64,
    pub name: String,
    /// Library to load for playback.
    pub plugin_path: PathBuf,
    /// Class to select from the factory; `None` takes the first.
    pub class_id: Option<[i8; 16]>,
    /// LeSynth tracks can carry a grid and be tagged for state import/export.
    pub is_lesynth: bool,
    /// This track plays a drum kit, so its notes are named after what they hit
    /// rather than left as pitches. See [`crate::midi::plays_a_drum_kit`].
    pub percussion: bool,
    /// The audio file this track *is*, played whole as one note — a track with
    /// no plugin behind it at all. `Some` makes every plugin field above inert:
    /// nothing is loaded, no state is imported, and the note's pitch is ignored
    /// (a file has the pitch it was recorded at). Published from the
    /// Resynthesis panel, and saved in a project as this path.
    pub wav: Option<PathBuf>,
    /// Grid to import into a freshly loaded instance. `None` = the plugin's own
    /// default state (a plain synth-mode LeSynth, or any custom VST).
    pub state: Option<TrackState>,
    /// The same idea for every other plugin: an opaque `IComponent` state, which
    /// is where a third-party VST3 keeps the knob positions the user set in its
    /// editor. `None` = whatever the plugin starts up with.
    pub vst_state: Option<Vec<u8>>,
    /// The instance behind this track's open editor, if any. Weak so a closed
    /// editor is simply gone; the entry itself outlives it.
    pub live: Option<Weak<PluginInstance>>,
}

/// Everything the Composer needs to load and configure its own instance of a
/// track — a value, so it can be built while the registry is borrowed and used
/// after the borrow ends.
#[derive(Clone)]
pub struct PlaybackSource {
    pub name: String,
    pub plugin_path: PathBuf,
    pub class_id: Option<[i8; 16]>,
    pub is_lesynth: bool,
    pub state: Option<TrackState>,
    pub vst_state: Option<Vec<u8>>,
    /// See [`TrackEntry::wav`]. `Some` means the row plays this file rather than
    /// any of the fields above.
    pub wav: Option<PathBuf>,
}

struct Inner {
    entries: Vec<TrackEntry>,
    next_id: u64,
}

/// Shared handle to the track list. Cloning shares, it does not copy.
#[derive(Clone)]
pub struct TrackRegistry(Rc<RefCell<Inner>>);

impl Default for TrackRegistry {
    fn default() -> Self {
        Self(Rc::new(RefCell::new(Inner {
            entries: Vec::new(),
            next_id: 0,
        })))
    }
}

impl TrackRegistry {
    /// Register a track and return its id.
    pub fn add(
        &self,
        name: impl Into<String>,
        plugin_path: PathBuf,
        class_id: Option<[i8; 16]>,
        is_lesynth: bool,
        state: Option<TrackState>,
    ) -> u64 {
        let mut inner = self.0.borrow_mut();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.entries.push(TrackEntry {
            id,
            name: name.into(),
            plugin_path,
            class_id,
            is_lesynth,
            percussion: false,
            wav: None,
            state,
            vst_state: None,
            live: None,
        });
        id
    }

    /// Register an audio file as a track that plays it whole, as one note, and
    /// return its id. The path is the whole recipe — see [`TrackEntry::wav`].
    pub fn add_wav(&self, name: impl Into<String>, path: PathBuf) -> u64 {
        let mut inner = self.0.borrow_mut();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.entries.push(TrackEntry {
            id,
            name: name.into(),
            // A wav track has no library; the file it plays is what it is, and
            // naming it here is what lets a message about the track say so.
            plugin_path: path.clone(),
            class_id: None,
            is_lesynth: false,
            percussion: false,
            wav: Some(path),
            state: None,
            vst_state: None,
            live: None,
        });
        id
    }

    /// The wav track playing `path`, if one is already registered. A file is one
    /// track however many times it is asked for: opening it again in Resynthesis,
    /// or loading a project that names it, finds the entry rather than adding a
    /// second one whose rows would be indistinguishable from the first's.
    pub fn find_wav(&self, path: &Path) -> Option<u64> {
        self.0
            .borrow()
            .entries
            .iter()
            .find(|e| e.wav.as_deref() == Some(path))
            .map(|e| e.id)
    }

    /// Whether this track plays an audio file rather than a plugin — which is
    /// what takes the pitch box off the Composer's frames. `false` for a track
    /// that is gone.
    pub fn is_wav(&self, id: u64) -> bool {
        self.0
            .borrow()
            .entries
            .iter()
            .find(|e| e.id == id)
            .is_some_and(|e| e.wav.is_some())
    }

    /// Record that this track plays a drum kit, which is what puts drum names in
    /// the Composer's note pickers.
    pub fn set_percussion(&self, id: u64, percussion: bool) {
        if let Some(e) = self.0.borrow_mut().entries.iter_mut().find(|e| e.id == id) {
            e.percussion = percussion;
        }
    }

    /// Whether this track plays a drum kit. `false` for a track that is gone.
    pub fn is_percussion(&self, id: u64) -> bool {
        self.0
            .borrow()
            .entries
            .iter()
            .find(|e| e.id == id)
            .is_some_and(|e| e.percussion)
    }

    /// Remember a plugin's own state for this track — what a freshly loaded
    /// instance is given, and what a project save writes out. Set when a custom
    /// VST3 editor closes, and when a project is loaded.
    pub fn set_vst_state(&self, id: u64, state: Option<Vec<u8>>) {
        if let Some(e) = self.0.borrow_mut().entries.iter_mut().find(|e| e.id == id) {
            e.vst_state = state;
        }
    }

    /// Remember a LeSynth grid for this track, the same way — set when its
    /// editor closes, so the Composer keeps playing what was edited.
    pub fn set_state(&self, id: u64, state: Option<TrackState>) {
        if let Some(e) = self.0.borrow_mut().entries.iter_mut().find(|e| e.id == id) {
            if state.is_some() {
                e.state = state;
            }
        }
    }

    pub fn remove(&self, id: u64) {
        self.0.borrow_mut().entries.retain(|e| e.id != id);
    }

    /// Point this track at the live instance behind its open editor, or clear it
    /// (`None`) when the editor closes.
    pub fn set_live(&self, id: u64, live: Option<Weak<PluginInstance>>) {
        if let Some(e) = self.0.borrow_mut().entries.iter_mut().find(|e| e.id == id) {
            e.live = live;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.borrow().entries.is_empty()
    }

    pub fn contains(&self, id: u64) -> bool {
        self.0.borrow().entries.iter().any(|e| e.id == id)
    }

    pub fn first_id(&self) -> Option<u64> {
        self.0.borrow().entries.first().map(|e| e.id)
    }

    pub fn name_of(&self, id: u64) -> Option<String> {
        self.0
            .borrow()
            .entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.name.clone())
    }

    /// `(id, name)` for every track, for the row select box.
    pub fn list(&self) -> Vec<(u64, String)> {
        self.0
            .borrow()
            .entries
            .iter()
            .map(|e| (e.id, e.name.clone()))
            .collect()
    }

    pub fn entries(&self) -> Ref<'_, [TrackEntry]> {
        Ref::map(self.0.borrow(), |i| i.entries.as_slice())
    }

    /// What to load to play track `id`. The grid comes from the live editor when
    /// one is open — the user is composing with what they can hear there, not
    /// with the state the track was registered in — and from the registered
    /// state otherwise.
    pub fn playback_source(&self, id: u64) -> Option<PlaybackSource> {
        let inner = self.0.borrow();
        let entry = inner.entries.iter().find(|e| e.id == id)?;
        // A wav track is its file. There is no instance to ask what it is
        // playing, and nothing about it changes while a composition is open.
        if let Some(path) = &entry.wav {
            return Some(PlaybackSource {
                name: entry.name.clone(),
                plugin_path: entry.plugin_path.clone(),
                class_id: None,
                is_lesynth: false,
                state: None,
                vst_state: None,
                wav: Some(path.clone()),
            });
        }
        let live = entry.live.as_ref().and_then(Weak::upgrade);

        // Whichever kind of state this track carries, prefer the open editor's:
        // the user is composing with what they can hear there.
        let (mut live_grid, mut live_vst) = (None, None);
        if let Some(plugin) = live {
            if entry.is_lesynth {
                match plugin.export_state() {
                    Ok(s) => live_grid = Some(s),
                    Err(e) => log::debug!("live grid unavailable for '{}': {e}", entry.name),
                }
            } else {
                // Every other VST3 keeps its knobs in its own opaque state.
                match plugin.component_state() {
                    Ok(bytes) if !bytes.is_empty() => live_vst = Some(bytes),
                    Ok(_) => {}
                    Err(e) => log::debug!("live state unavailable for '{}': {e:#}", entry.name),
                }
            }
        }
        Some(PlaybackSource {
            name: entry.name.clone(),
            plugin_path: entry.plugin_path.clone(),
            class_id: entry.class_id,
            is_lesynth: entry.is_lesynth,
            state: live_grid.or_else(|| entry.state.clone()),
            vst_state: live_vst.or_else(|| entry.vst_state.clone()),
            wav: None,
        })
    }
}
