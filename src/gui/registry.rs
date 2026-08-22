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
//! Entries are *recipes*, not live plugins: a library path, a class id, and (for
//! LeSynth) the grid to import, so a composition plays whether or not the
//! track's editor is open. When it is, [`TrackEntry::live`] points at that
//! instance, so the Composer can snapshot the grid being edited right now.
//!
//! GUI-thread only (`Rc<RefCell<_>>`) — every user of it is an egui panel.

use std::cell::{Ref, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Weak};

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
    /// Grid to import into a freshly loaded instance. `None` = the plugin's own
    /// default state (a plain synth-mode LeSynth, or any custom VST).
    pub state: Option<TrackState>,
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
            state,
            live: None,
        });
        id
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
        let live_state = entry
            .live
            .as_ref()
            .and_then(Weak::upgrade)
            .and_then(|p: Arc<PluginInstance>| match p.export_state() {
                Ok(s) => Some(s),
                Err(e) => {
                    log::debug!("live grid unavailable for '{}': {e}", entry.name);
                    None
                }
            });
        Some(PlaybackSource {
            name: entry.name.clone(),
            plugin_path: entry.plugin_path.clone(),
            class_id: entry.class_id,
            is_lesynth: entry.is_lesynth,
            state: live_state.or_else(|| entry.state.clone()),
        })
    }
}
