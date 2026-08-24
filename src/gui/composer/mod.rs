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
//! Track Composer — arrange the registered Tracks in rows of frames and play them.
//!
//! **A row is a sequence, not a canvas.** It plays one Track (several rows may
//! share it) as a chain of frames laid left to right, each simply played after
//! the one before: nothing is positioned, dragged, or overlapping. Rows sound
//! together exactly to the extent that the lengths before a frame add up the
//! same — that is what makes a chord.
//!
//! **Frames come in pairs.** A *note* frame carries a pitch and a length; the
//! *space* behind it carries the silence that follows. They are one [`Item`], so
//! no code path can orphan a space or leave a note without one, and the space
//! has no delete button of its own. Zero length is normal — a placeholder ready
//! to be given one.
//!
//! **Every row also opens with a space** ([`Row::lead`]), which is how one row is
//! offset against another. It belongs to the row, not to a note, so deleting the
//! first note leaves it in place to lead the next; it starts at zero.
//!
//! **Length is two select boxes**: whole notes plus a fraction down to 1/256.
//! Time is counted in [`UNITS_PER_WHOLE`]ths, so every length is a whole number
//! of units and the arithmetic stays exact. Playback lives in [`player`].
//!
//! **Rows can also be played in rather than written.** "Record & Play Once"
//! plays the composition through once and captures a MIDI keyboard against it,
//! rounding what was played onto a chosen note value and appending it as new
//! rows — see [`record`].

pub mod player;
pub mod project;
pub mod record;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;

use self::player::{CompositionPlayer, PlannedNote, PreparedComposition, RowEdit, RowPlan};
use crate::midi::{add_midi_tap, gm_percussion_name, MidiTap, MidiTaps};
use super::registry::TrackRegistry;

/// Time resolution: a whole note is this many units. 256 makes every fraction in
/// [`Fraction`] — down to a 1/256 — a whole number of them, so lengths and the
/// positions they add up to are exact integer arithmetic.
const UNITS_PER_WHOLE: i64 = 256;
/// A beat is a quarter note.
const UNITS_PER_BEAT: i64 = UNITS_PER_WHOLE / 4;

/// On-screen width of one frame. Fixed: a frame carries up to three select boxes
/// and stops being usable below about this width, and a width proportional to
/// the length would make a 1/256 invisible.
const CARD_W: f32 = 108.0;
/// Height of one frame — a header line plus up to three select boxes.
const CARD_H: f32 = 112.0;
/// Height of one row's lane. The frames plus a little air around them.
const ROW_H: f32 = CARD_H + 10.0;
/// Width of the fixed row head (track select, add-note, gain).
const HEAD_W: f32 = 284.0;

/// Lowest and highest note offered, C0..B8.
const PITCH_MIN: u8 = 12;
const PITCH_MAX: u8 = 119;
/// A new note starts at middle C.
const DEFAULT_PITCH: u8 = 60;
/// …unless the row plays a drum kit, where middle C is a bongo and the note
/// anyone wants first is the kick.
const DEFAULT_DRUM_PITCH: u8 = 36;
/// Largest whole-note count a length may carry.
const MAX_WHOLES: u8 = 16;

/// The fractional part of a length, as the fraction of a whole note it is named
/// for. `None` is a length that is a whole number of whole notes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fraction {
    None,
    Half,
    Quarter,
    Eighth,
    Sixteenth,
    ThirtySecond,
    SixtyFourth,
    HundredTwentyEighth,
    TwoHundredFiftySixth,
}

impl Fraction {
    const ALL: [Fraction; 9] = [
        Fraction::None,
        Fraction::Half,
        Fraction::Quarter,
        Fraction::Eighth,
        Fraction::Sixteenth,
        Fraction::ThirtySecond,
        Fraction::SixtyFourth,
        Fraction::HundredTwentyEighth,
        Fraction::TwoHundredFiftySixth,
    ];

    /// Length in grid units.
    fn units(self) -> i64 {
        match self {
            Fraction::None => 0,
            Fraction::Half => UNITS_PER_WHOLE / 2,
            Fraction::Quarter => UNITS_PER_WHOLE / 4,
            Fraction::Eighth => UNITS_PER_WHOLE / 8,
            Fraction::Sixteenth => UNITS_PER_WHOLE / 16,
            Fraction::ThirtySecond => UNITS_PER_WHOLE / 32,
            Fraction::SixtyFourth => UNITS_PER_WHOLE / 64,
            Fraction::HundredTwentyEighth => UNITS_PER_WHOLE / 128,
            Fraction::TwoHundredFiftySixth => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Fraction::None => "—",
            Fraction::Half => "1/2",
            Fraction::Quarter => "1/4",
            Fraction::Eighth => "1/8",
            Fraction::Sixteenth => "1/16",
            Fraction::ThirtySecond => "1/32",
            Fraction::SixtyFourth => "1/64",
            Fraction::HundredTwentyEighth => "1/128",
            Fraction::TwoHundredFiftySixth => "1/256",
        }
    }
}

/// A length: a whole-note count plus a fraction, added together. Both parts are
/// picked in their own select box, which is why they are stored apart rather
/// than as a single unit count.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Duration {
    /// Whole notes, `0..=`[`MAX_WHOLES`].
    wholes: u8,
    frac: Fraction,
}

impl Duration {
    const fn new(wholes: u8, frac: Fraction) -> Self {
        Self { wholes, frac }
    }

    fn units(self) -> i64 {
        self.wholes as i64 * UNITS_PER_WHOLE + self.frac.units()
    }

    /// How the length reads in the frame's header, e.g. `1 + 1/8`.
    fn label(self) -> String {
        match (self.wholes, self.frac) {
            (0, Fraction::None) => "0".to_string(),
            (0, f) => f.label().to_string(),
            (w, Fraction::None) => w.to_string(),
            (w, f) => format!("{w} + {}", f.label()),
        }
    }
}

/// Scientific pitch name of a MIDI note number (`60` → `C4`).
fn pitch_name(pitch: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", NAMES[(pitch % 12) as usize], pitch as i32 / 12 - 1)
}

/// What a note is called on a row, which depends on what the row plays: a pitch
/// on an instrument, and the drum it hits on a kit.
///
/// The drum name is a General MIDI one, and it is shown *beside* the pitch, not
/// instead of it: on a kit whose pads are not GM the note still plays whatever
/// is on the pad, and the pitch — the thing actually sent — stays visible.
fn note_label(pitch: u8, percussion: bool) -> String {
    match percussion.then(|| gm_percussion_name(pitch)).flatten() {
        Some(drum) => format!("{} · {drum}", pitch_name(pitch)),
        None => pitch_name(pitch),
    }
}

/// A note and the space tied behind it — two frames on screen, one thing in the
/// model. They are stored together rather than as two entries in the row
/// because the space is not independently removable: it exists only as the
/// silence after *this* note, and goes when the note goes. Making that
/// structural means no code path can leave a space orphaned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    /// Stable within its row, so the egui widget ids survive frames being
    /// deleted around it.
    pub(crate) id: u64,
    pub(crate) pitch: u8,
    /// How long the note sounds.
    pub(crate) dur: Duration,
    /// The silence after it. Zero is legal and useful: the frame stays on
    /// screen as a placeholder, ready to be given a length, while the next note
    /// follows on immediately.
    pub(crate) space: Duration,
}

impl Item {
    /// The note and its space together — what the next note waits for.
    fn total_units(&self) -> i64 {
        self.dur.units() + self.space.units()
    }
}

/// One lane: a Track and the chain of frames played on it.
struct Row {
    /// Stable id, so egui widget state survives rows being removed above them.
    id: u64,
    /// The Track this row plays, by registry id. `None` only while the registry
    /// is empty.
    track_id: Option<u64>,
    gain: f32,
    /// The silence before the row's first note. Held by the row, not an item, so
    /// deleting the first note cannot take it along — it stays at the head and
    /// leads whichever note is first afterwards. Zero by default.
    lead: Duration,
    /// Re-export this row's LeSynth grid into the project folder on every save.
    /// On by default, so a project saved after an edit carries the edit; off
    /// pins whatever `.lsft` is already there.
    autosave: bool,
    /// The source a loaded project asked for and the app could not find. Kept
    /// whole, not just as a message, so saving the project again preserves the
    /// reference rather than quietly replacing it with "no track" — a save after
    /// a load must not be the thing that loses the file name.
    ///
    /// A row in this state is **not** auto-adopted onto another track by
    /// [`ComposerPanel::reconcile_tracks`]: the point is that the user is told
    /// what is missing and picks the replacement.
    missing: Option<project::TrackSource>,
    /// In play order: item `n` starts where item `n - 1`'s space ended.
    items: Vec<Item>,
    next_item_id: u64,
}

impl Row {
    fn new(id: u64, track_id: Option<u64>) -> Self {
        Self {
            id,
            track_id,
            gain: 1.0,
            lead: Duration::new(0, Fraction::None),
            autosave: true,
            missing: None,
            items: Vec::new(),
            next_item_id: 0,
        }
    }

    /// Where the row's first note starts: the lead space, which counts only
    /// while there is a note for it to lead. An empty row has no first note and
    /// so no lead — and draws none.
    fn lead_units(&self) -> i64 {
        if self.items.is_empty() {
            0
        } else {
            self.lead.units()
        }
    }

    /// Total length of the row in units — the lead space plus the items, which
    /// simply add up.
    fn end_units(&self) -> i64 {
        self.lead_units() + self.items.iter().map(Item::total_units).sum::<i64>()
    }

    /// Append a note and, tied behind it, the space that separates it from
    /// whatever comes next.
    ///
    /// The new frame **repeats the last one whole** — its pitch, its note length
    /// and its space, whole notes and fraction alike. A row is nearly always
    /// built by repeating something: a run on one note, the same drum over and
    /// over, a rhythm of one length. What was chosen a moment ago is a far better
    /// guess than any fixed default, and carrying only *part* of it is the worst
    /// of both — five select boxes to check every time, three of which have
    /// silently gone back to a default.
    ///
    /// `default_pitch` is what the *first* note of a row starts on; the lengths
    /// it starts on are a quarter note and an eighth of silence.
    fn add_note(&mut self, default_pitch: u8) {
        let id = self.next_item_id;
        self.next_item_id += 1;
        let item = match self.items.last() {
            Some(last) => Item { id, ..*last },
            None => Item {
                id,
                pitch: default_pitch,
                dur: Duration::new(0, Fraction::Quarter),
                space: Duration::new(0, Fraction::Eighth),
            },
        };
        self.items.push(item);
    }

    /// Delete a note *and* the space tied to it — the only way either of them
    /// leaves the row.
    fn delete_item(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.items.remove(idx);
        }
    }

    /// Where each note starts, in units — the running sum of everything before
    /// it. One entry per item; its space starts where the note ends.
    fn starts(&self) -> Vec<i64> {
        let mut at = self.lead_units();
        self.items
            .iter()
            .map(|i| {
                let start = at;
                at += i.total_units();
                start
            })
            .collect()
    }

    /// The row's notes in seconds, at `spu` seconds per grid unit. Spaces only
    /// advance the clock, and a note given no length at all is not played.
    fn planned_notes(&self, spu: f64) -> Vec<PlannedNote> {
        let mut at = self.lead_units();
        let mut out = Vec::new();
        for item in &self.items {
            let units = item.dur.units();
            if units > 0 {
                out.push(PlannedNote {
                    at_secs: at as f64 * spu,
                    dur_secs: units as f64 * spu,
                    pitch: item.pitch,
                });
            }
            at += item.total_units();
        }
        out
    }
}

/// What the Composer wants the app to do with the project. The panel owns the
/// composition but not the plugin instances a save has to read the grids from,
/// nor the Tracks panel a load has to put them into, so the button records the
/// intent and [`ComposerPanel::take_request`] hands it over.
#[derive(Clone, Debug, PartialEq)]
pub enum ProjectRequest {
    /// Write the project to `dir`, creating it if needed.
    Save { dir: PathBuf, name: String },
    /// Read the manifest at this path and replace the composition with it.
    Load { file: PathBuf },
}

/// What the running transport has been told, so the panel can tell whether the
/// user has changed anything since.
struct LiveSnapshot {
    /// `(row id, track id)` per playing row — the part a live edit *cannot*
    /// change, because a different track means a different plugin to load.
    shape: Vec<(u64, Option<u64>)>,
    /// The part it can: notes and gain, per row.
    rows: Vec<RowEdit>,
    loop_secs: f64,
    /// Whether the user has already been told that the shape changed, so the
    /// status line is not rewritten on every frame.
    shape_warned: bool,
}

/// A take in progress: where the keyboard's messages are piling up, and what
/// they are timed against.
///
/// Nothing is written into the composition until it ends. A row is a chain of
/// lengths, so a note's frame cannot be built before the note after it is known
/// — and half a note appearing in a row while the user is still playing would
/// be a distraction, not feedback.
struct Recording {
    /// This take's own copy of the keyboard, stamped as it arrived. A copy, not
    /// the queue itself: an open editor drains that one destructively, and
    /// recording must not take notes away from the instrument the user is
    /// listening to while they play.
    tap: MidiTap,
    /// The clock for a take with no transport behind it (an empty composition
    /// has nothing to play, so there is no stream to ask). With one, the
    /// transport's own [`CompositionPlayer::heard_secs_at`] is used instead, so
    /// what was played lines up with what was heard.
    origin: Instant,
}

/// The Composer panel.
pub struct ComposerPanel {
    registry: TrackRegistry,
    rows: Vec<Row>,
    next_row_id: u64,
    tempo_bpm: f32,
    /// Project name as typed. Sanitised into the folder and manifest name only
    /// when saving, so what the user sees is what they wrote.
    project_name: String,
    /// Where this project was last saved to or loaded from, if anywhere.
    project_dir: Option<PathBuf>,
    /// Set by the Save/Load buttons and taken by the app, which owns the plugin
    /// instances the grids have to be exported from.
    request: Option<ProjectRequest>,
    status: String,
    player: Option<CompositionPlayer>,
    /// A Play that has been pressed but has not made a sound yet: the rows are
    /// loading their plugins on a thread of their own.
    preparing: Option<Receiver<Result<PreparedComposition, String>>>,
    /// Loop the composition instead of stopping at the end. Shared with the
    /// transport's audio callback, so the checkbox works on a running player.
    repeat: Arc<AtomicBool>,
    /// The composition as the looping transport last received it. What an edit
    /// is compared against, so an untouched frame publishes nothing.
    live_sent: Option<LiveSnapshot>,
    /// A running WAV export, which renders on its own thread and sends back the
    /// line to show when it is done. The GUI must not block on it: a long
    /// composition takes seconds, and a frozen window looks like a crash.
    export: Option<Receiver<String>>,
    /// Where a recording gets its own copy of the keyboard from.
    midi_taps: MidiTaps,
    /// The take being played in, if any.
    recording: Option<Recording>,
    /// The note value a take is rounded onto, in units.
    round_units: i64,
    /// The track a recorded row plays. `None` falls back to the first track
    /// there is, which is what an empty registry and a fresh panel both mean.
    record_track: Option<u64>,
}

impl ComposerPanel {
    pub fn new(registry: TrackRegistry, midi_taps: MidiTaps) -> Self {
        Self {
            registry,
            rows: Vec::new(),
            next_row_id: 0,
            tempo_bpm: 120.0,
            project_name: "Untitled".to_string(),
            project_dir: None,
            request: None,
            status: "Add a track row to start composing.".to_string(),
            player: None,
            preparing: None,
            repeat: Arc::new(AtomicBool::new(false)),
            live_sent: None,
            export: None,
            midi_taps,
            recording: None,
            round_units: record::DEFAULT_ROUND_UNITS,
            record_track: None,
        }
    }

    /// Seconds one grid unit lasts.
    fn secs_per_unit(&self) -> f64 {
        60.0 / (self.tempo_bpm.max(1.0) as f64) / UNITS_PER_BEAT as f64
    }

    fn add_row(&mut self) {
        let id = self.next_row_id;
        self.next_row_id += 1;
        let track = self.registry.first_id();
        self.rows.push(Row::new(id, track));
        self.status = match track.and_then(|t| self.registry.name_of(t)) {
            Some(name) => format!("Added a row playing {name}."),
            None => "Added a row — no tracks exist yet to assign it to.".to_string(),
        };
    }

    /// Keep every row pointed at a track that still exists: a row whose track was
    /// removed falls back to the first one available, and a row left without any
    /// (the registry went empty) adopts the first track added afterwards.
    ///
    /// A row whose loaded source went **missing** is left alone. Quietly moving
    /// it onto some other track would hide the one thing the user has to be told
    /// about, and would make the replacement look like it had always been there.
    fn reconcile_tracks(&mut self) {
        let first = self.registry.first_id();
        for row in &mut self.rows {
            if row.missing.is_some() {
                continue;
            }
            let valid = row.track_id.is_some_and(|id| self.registry.contains(id));
            if !valid {
                row.track_id = first;
            }
        }
        if !self.record_track.is_some_and(|id| self.registry.contains(id)) {
            self.record_track = first;
        }
    }

    /// End of the composition in units — the longest row.
    fn end_units(&self) -> i64 {
        self.rows.iter().map(Row::end_units).max().unwrap_or(0)
    }

    /// Every row that has something to play, resolved to seconds. A row with no
    /// notes, or with no track behind it, simply is not in the composition.
    fn build_plans(&self) -> Vec<RowPlan> {
        let spu = self.secs_per_unit();
        let mut plans = Vec::new();
        for row in &self.rows {
            let Some(source) = row.track_id.and_then(|id| self.registry.playback_source(id)) else {
                continue;
            };
            let notes = row.planned_notes(spu);
            if notes.is_empty() {
                continue;
            }
            plans.push(RowPlan {
                row_id: row.id,
                source,
                gain: row.gain,
                notes,
            });
        }
        plans
    }

    /// Whether anything would play — without asking the registry for a playback
    /// source, which exports a LeSynth's live grid and is far too much work for
    /// a question this cheap to answer.
    fn has_playable_rows(&self) -> bool {
        let spu = self.secs_per_unit();
        self.rows
            .iter()
            .any(|row| row.track_id.is_some() && !row.planned_notes(spu).is_empty())
    }

    /// The composition as the transport would need it *now*. Cheap by design:
    /// it resolves note times and reads row gains, and deliberately does not
    /// touch the registry — asking that for a playback source exports a
    /// LeSynth's live grid, which is far too much work for a per-frame check.
    fn live_snapshot(&self) -> LiveSnapshot {
        let spu = self.secs_per_unit();
        let mut shape = Vec::new();
        let mut rows = Vec::new();
        for row in &self.rows {
            // Same filter as `build_plans`: a row with no track or no notes is
            // not in the composition, so it is not in the transport either.
            if row.track_id.is_none() {
                continue;
            }
            let notes = row.planned_notes(spu);
            if notes.is_empty() {
                continue;
            }
            shape.push((row.id, row.track_id));
            rows.push(RowEdit {
                row_id: row.id,
                gain: row.gain,
                notes,
            });
        }
        LiveSnapshot {
            shape,
            rows,
            loop_secs: self.end_units() as f64 * self.secs_per_unit(),
            shape_warned: false,
        }
    }

    /// While a repeat is running, hand the transport anything the user has
    /// changed since it last heard from us. Applied at the next loop point.
    ///
    /// Only notes, gains and the loop length travel this way. Adding a row, or
    /// pointing one at another track, means a plugin to load — which cannot
    /// happen on the audio thread — so that is reported rather than applied.
    fn push_live_edits(&mut self) {
        let Some(player) = &self.player else { return };
        if !self.repeat.load(Ordering::Relaxed) {
            return;
        }
        let Some(sent) = &self.live_sent else { return };

        let snapshot = self.live_snapshot();
        if snapshot.shape != sent.shape {
            if !sent.shape_warned {
                self.status = "Rows or their tracks changed — press Play again to \
                               hear them; note edits still follow the loop."
                    .to_string();
                if let Some(sent) = &mut self.live_sent {
                    sent.shape_warned = true;
                }
            }
            return;
        }
        if snapshot.rows == sent.rows && snapshot.loop_secs == sent.loop_secs {
            return;
        }
        player.update_live(&snapshot.rows, snapshot.loop_secs);
        self.live_sent = Some(snapshot);
    }

    /// Press Play: load the rows on a thread, and start the transport when they
    /// land ([`Self::poll_preparation`]).
    ///
    /// Loading is not quick — one plugin instance per row, and a plugin can take
    /// a second — so doing it here would freeze the window for as long as it
    /// takes, which is what makes a transport feel broken. The composition is
    /// snapshotted now, so what plays is what was on screen when the button went
    /// down, however long the loading takes.
    fn start_playback(&mut self, ctx: &egui::Context) {
        if self.preparing.is_some() {
            return;
        }
        let plans = self.build_plans();
        if plans.is_empty() {
            self.status = "Nothing to play — add a note to a row first.".to_string();
            return;
        }
        let rows = plans.len();
        let (tx, rx) = std::sync::mpsc::channel();
        // The context, so the rows can wake the window the moment they are
        // ready. Without it the transport waits for whenever the GUI next
        // happens to paint, which is a quarter of a second of silence for
        // nothing.
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(CompositionPlayer::prepare(plans).map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
        self.preparing = Some(rx);
        self.live_sent = Some(self.live_snapshot());
        self.status = format!("Loading {rows} row(s)…");
    }

    /// Start the transport once the rows have finished loading. Opening the
    /// stream is a couple of milliseconds, so that half stays here.
    fn poll_preparation(&mut self) {
        let Some(rx) = &self.preparing else { return };
        let ready = match rx.try_recv() {
            Ok(ready) => ready,
            Err(TryRecvError::Empty) => return,
            // The loading thread died without a word, which is still an answer.
            Err(TryRecvError::Disconnected) => Err("the rows stopped loading".to_string()),
        };
        self.preparing = None;

        let prepared = match ready {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Playback failed: {e}");
                self.live_sent = None;
                // A take with no backing track and no clock to place it on is
                // not a take. Abandoning it here is better than recording
                // against a silence the user was not expecting.
                self.recording = None;
                return;
            }
        };
        let (loaded, total) = prepared.loaded_rows();
        // The length may have been edited while the rows were loading; the loop
        // follows what is on screen now, as a live edit would.
        let loop_secs = self.end_units() as f64 * self.secs_per_unit();
        match CompositionPlayer::start_prepared(prepared, loop_secs, self.repeat.clone()) {
            Ok(player) => {
                self.status = if loaded == total {
                    format!("Playing {loaded} row(s).")
                } else {
                    format!("Playing {loaded} of {total} row(s) — the rest failed to load.")
                };
                if let Some(rec) = &self.recording {
                    // The take starts where the sound does. Loading the rows
                    // took as long as it took, and anything played into the tap
                    // meanwhile was played against nothing — the transport's own
                    // clock ([`CompositionPlayer::heard_secs_at`]) takes over
                    // from here, and it starts at this moment.
                    if let Ok(mut tapped) = rec.tap.lock() {
                        tapped.clear();
                    }
                    self.status = format!("⏺ Recording over {loaded} row(s) — play your keyboard.");
                }
                self.player = Some(player);
            }
            Err(e) => {
                self.status = format!("Playback failed: {e:#}");
                self.live_sent = None;
                self.recording = None;
            }
        }
    }

    /// Press Record: play the composition through exactly once, and keep
    /// everything the MIDI keyboard sends while it does.
    ///
    /// Nothing is written into the composition until the pass ends
    /// ([`Self::finish_recording`]). A composition with nothing in it yet still
    /// records — there is simply no transport behind the take, and it runs from
    /// the button press until Stop.
    fn start_recording(&mut self, ctx: &egui::Context) {
        if self.recording.is_some() || self.player.is_some() || self.preparing.is_some() {
            return;
        }
        // Exactly once, as the button says: a loop would have the second pass
        // recorded over the first.
        self.repeat.store(false, Ordering::Relaxed);
        self.recording = Some(Recording {
            tap: add_midi_tap(&self.midi_taps),
            origin: Instant::now(),
        });
        if self.has_playable_rows() {
            self.start_playback(ctx);
            // `start_playback` says how many rows are loading; the take is only
            // armed once they are (see `poll_preparation`).
            self.status = format!("⏺ {}", self.status);
        } else {
            self.status = "⏺ Recording — nothing to play along to, so it runs from now \
                           until you press Stop."
                .to_string();
        }
    }

    /// End a take and append what was played to the composition, rounded onto
    /// the chosen note value.
    ///
    /// Returns the line to show, which the caller puts up *after* whatever
    /// stopping the transport had to say — "Stopped." is not the news here.
    /// Must be called while the player is still in hand: the transport is the
    /// clock the take is placed on.
    fn finish_recording(&mut self) -> Option<String> {
        let rec = self.recording.take()?;
        let messages = match rec.tap.lock() {
            Ok(mut tapped) => std::mem::take(&mut *tapped),
            // The tap is only ever locked for a push and for this; a poisoned
            // one means a MIDI callback panicked, and the take is gone with it.
            Err(_) => return Some("Recording failed: the keyboard tap was lost.".to_string()),
        };

        let notes = {
            let player = self.player.as_ref();
            let clock = |t: Instant| match player {
                Some(p) => p.heard_secs_at(t),
                None => Some(t.saturating_duration_since(rec.origin).as_secs_f64()),
            };
            let end_secs = clock(Instant::now()).unwrap_or(0.0);
            record::notes_from(&messages, clock, end_secs)
        };
        if notes.is_empty() {
            return Some(
                "Recorded nothing — no notes arrived. Connect a keyboard under MIDI \
                 and press Connect."
                    .to_string(),
            );
        }

        let grid = self.round_units;
        let quantized = record::quantize(&notes, self.secs_per_unit(), grid);
        let voices = record::split_voices(&quantized);
        let track = self.record_track.or_else(|| self.registry.first_id());
        for voice in &voices {
            let id = self.next_row_id;
            self.next_row_id += 1;
            let mut row = Row::new(id, track);
            let (lead, items) = record::voice_items(voice, &mut row.next_item_id);
            row.lead = lead;
            row.items = items;
            self.rows.push(row);
        }

        let played = notes.len();
        let rows = voices.len();
        let grid = record::round_label(grid);
        Some(match (track, rows) {
            // A take with no track behind it is still worth keeping: the rows
            // are there, and the select box on each is how they get a sound.
            (None, _) => format!(
                "Recorded {played} note(s) into {rows} new row(s), rounded to {grid} — \
                 no tracks exist yet, so pick one on each row."
            ),
            (Some(_), 1) => format!("Recorded {played} note(s) into a new row, rounded to {grid}."),
            // More rows than one means notes sounded together: a row plays one
            // note at a time, so a chord has to be spread across several. A key
            // merely let go of late does not land here — that stays one row.
            (Some(_), _) => format!(
                "Recorded {played} note(s), rounded to {grid} — spread over {rows} new rows, \
                 because notes sounded together."
            ),
        })
    }

    /// Render the whole composition offline and write it as a `.wav`.
    ///
    /// The render runs on its own thread — it loads its own plugin instances and
    /// works faster than real time, but "faster than real time" is still seconds
    /// for a long piece, and the panel stays usable meanwhile. The path is asked
    /// for first: a file dialog belongs on the GUI thread.
    fn export_wav(&mut self) {
        let plans = self.build_plans();
        if plans.is_empty() {
            self.status = "Nothing to export — add a note to a row first.".to_string();
            return;
        }
        let stem = project::sanitize_name(&self.project_name);
        let mut dialog = rfd::FileDialog::new()
            .set_title("Export the track composition as WAV")
            .add_filter("WAV audio", &["wav"])
            .set_file_name(format!("{stem}.wav"));
        if let Some(dir) = &self.project_dir {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.save_file() else { return };

        let (sample_rate, channels) = player::default_export_format();
        let shown = path.display().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let msg = match player::render_offline(plans, sample_rate, channels) {
                Ok((samples, loaded, total)) => {
                    let secs = samples.len() as f64 / (sample_rate * channels as f64).max(1.0);
                    match crate::audio::write_wav_i16(
                        &path,
                        &samples,
                        channels as u16,
                        sample_rate as u32,
                    ) {
                        Ok(()) if loaded == total => format!(
                            "Exported {:.1} s to {} ({} row(s), {:.0} Hz).",
                            secs,
                            path.display(),
                            loaded,
                            sample_rate
                        ),
                        // Say which rows are missing from the file rather than
                        // let a silent instrument be discovered on playback.
                        Ok(()) => format!(
                            "Exported {:.1} s to {} — only {loaded} of {total} row(s) \
                             loaded, the rest are missing from the file.",
                            secs,
                            path.display()
                        ),
                        Err(e) => format!("Export failed: {e}"),
                    }
                }
                Err(e) => format!("Export failed: {e}"),
            };
            let _ = tx.send(msg);
        });
        self.status = format!("Exporting to {shown}… (rendering offline)");
        self.export = Some(rx);
    }

    fn stop_playback(&mut self) {
        // The take is closed first: the transport is the clock it is placed on,
        // so it cannot be read after the player has gone.
        let recorded = self.finish_recording();
        self.live_sent = None;
        // Dropping the receiver abandons a Play that is still loading: the
        // thread finishes on its own and drops the instances it made, which is
        // exactly where that work belongs — not here.
        let was_preparing = self.preparing.take().is_some();
        if self.player.take().is_some() || was_preparing {
            self.status = "Stopped.".to_string();
        }
        // What was recorded is the news, not that the transport stopped.
        if let Some(msg) = recorded {
            self.status = msg;
        }
    }

    /// Where the transport is, in grid units, while it is running. The frame
    /// containing it is the one lit up in each lane.
    fn playhead_units(&self) -> Option<f64> {
        let player = self.player.as_ref()?;
        Some(player.position_secs() / self.secs_per_unit())
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.reconcile_tracks();
        // A finished composition stops itself, so the transport does not sit on
        // "playing" over silence.
        // A Play whose rows have finished loading starts here, so the button
        // press and the first sound are separate frames and the window never
        // stops painting between them.
        self.poll_preparation();
        if self.player.as_ref().is_some_and(CompositionPlayer::is_finished) {
            // Before the player goes: a take is placed on the transport's clock.
            let recorded = self.finish_recording();
            self.player = None;
            self.live_sent = None;
            self.status = recorded.unwrap_or_else(|| "Finished.".to_string());
        }
        // Anything the user changed while a repeat is running goes in at the
        // next loop point.
        self.push_live_edits();
        // A finished export reports itself. A dropped sender means the render
        // thread died without a word, which is still an answer the user needs.
        if let Some(rx) = &self.export {
            match rx.try_recv() {
                Ok(msg) => {
                    self.status = msg;
                    self.export = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.status = "Export failed: the render stopped unexpectedly.".to_string();
                    self.export = None;
                }
            }
        }

        let tracks = self.registry.list();

        ui.horizontal_wrapped(|ui| {
            if ui
                .button("➕ Add Track Row")
                .on_hover_text("A new lane, playing the first available track")
                .clicked()
            {
                self.add_row();
            }
            if tracks.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "No tracks yet — create one under Tracks, or publish a subtrack \
                         from Resynthesis with “Add as Track”.",
                    )
                    .color(egui::Color32::from_rgb(220, 180, 120)),
                );
            }
        });
        ui.add_space(6.0);
        self.project_ui(ui);

        if !self.rows.is_empty() {
            ui.add_space(6.0);
            self.lanes_ui(ui, &tracks);
        }

        ui.add_space(8.0);
        ui.separator();
        self.transport_ui(ui);
    }

    /// Take whatever the Save/Load buttons asked for, if anything. The app calls
    /// this after drawing and performs the request — see [`ProjectRequest`].
    pub fn take_request(&mut self) -> Option<ProjectRequest> {
        self.request.take()
    }

    /// The composition as it would be saved. `source_of` maps a row's registry
    /// id onto the source to record for it, which only the app can decide: it
    /// knows which tracks are LeSynth, where their grids went, and where a
    /// custom VST was loaded from.
    pub fn to_project(
        &self,
        name: &str,
        mut source_of: impl FnMut(Option<u64>) -> project::TrackSource,
    ) -> project::Project {
        project::Project {
            name: name.to_string(),
            tempo_bpm: self.tempo_bpm,
            rows: self
                .rows
                .iter()
                .map(|row| project::ProjectRow {
                    track_name: row
                        .track_id
                        .and_then(|id| self.registry.name_of(id))
                        .or_else(|| row.missing.as_ref().map(|s| s.describe()))
                        .unwrap_or_default(),
                    // An unresolved row round-trips the source it was looking
                    // for; anything else asks the app what its track is now.
                    source: match &row.missing {
                        Some(src) => src.clone(),
                        None => source_of(row.track_id),
                    },
                    gain: row.gain,
                    lead: row.lead,
                    autosave: row.autosave,
                    items: row.items.clone(),
                })
                .collect(),
        }
    }

    /// Replace the composition with a loaded project. `resolved` is one entry
    /// per row, in order: the registry id the app managed to bind that row's
    /// source to, or `None` if it could not be found.
    pub fn apply_project(
        &mut self,
        project: &project::Project,
        dir: PathBuf,
        resolved: &[Option<u64>],
    ) {
        self.stop_playback();
        self.tempo_bpm = project.tempo_bpm;
        self.project_name = project.name.clone();
        self.project_dir = Some(dir);
        self.rows.clear();
        for (i, prow) in project.rows.iter().enumerate() {
            let id = self.next_row_id;
            self.next_row_id += 1;
            let track_id = resolved.get(i).copied().flatten();
            let mut row = Row::new(id, track_id);
            row.gain = prow.gain;
            row.lead = prow.lead;
            row.autosave = prow.autosave;
            // Nothing to bind means the source is gone. Keep what it was so the
            // row can say so and the user knows what to replace.
            row.missing = match (track_id, &prow.source) {
                (None, project::TrackSource::None) => None,
                (None, src) => Some(src.clone()),
                (Some(_), _) => None,
            };
            for item in &prow.items {
                let item_id = row.next_item_id;
                row.next_item_id += 1;
                row.items.push(Item { id: item_id, ..*item });
            }
            self.rows.push(row);
        }
        let missing = self.rows.iter().filter(|r| r.missing.is_some()).count();
        self.status = match missing {
            0 => format!("Loaded {} — {} row(s).", project.name, self.rows.len()),
            n => format!(
                "Loaded {} — {} row(s), {n} with a missing source: pick a new one \
                 in the row's select box.",
                project.name,
                self.rows.len()
            ),
        };
    }

    /// Which rows want their grid re-exported on the next save, as registry ids.
    pub fn autosave_track_ids(&self) -> Vec<u64> {
        self.rows
            .iter()
            .filter(|r| r.autosave)
            .filter_map(|r| r.track_id)
            .collect()
    }

    /// Record where a save actually landed, so the next one goes to the same
    /// folder rather than asking again.
    pub fn set_project_dir(&mut self, dir: PathBuf, name: String) {
        self.project_dir = Some(dir);
        self.project_name = name;
    }

    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Name, Save and Load. The name is the folder's name and the manifest's,
    /// so there is one thing to type and no separate "save as" dialog to keep in
    /// step with it: saving under a new name writes a new folder beside the old.
    fn project_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Project");
            ui.add(
                egui::TextEdit::singleline(&mut self.project_name)
                    .desired_width(180.0)
                    .hint_text("Untitled"),
            )
            .on_hover_text(
                "The project's name — also the folder it is saved in and the \
                 .gmstn file inside it.\n\nSaving under a new name writes a new \
                 folder next to the old one and leaves that one untouched.",
            );

            let stem = project::sanitize_name(&self.project_name);
            if ui
                .button("💾 Save Project")
                .on_hover_text(format!(
                    "Write {stem}/{stem}.{ext}, with a .lsft beside it for every \
                     LeSynth Fourier track a row plays.",
                    ext = project::EXTENSION
                ))
                .clicked()
            {
                self.request_save(&stem);
            }
            if ui
                .button("📂 Load Project")
                .on_hover_text("Open a .gmstn project — this replaces the rows below.")
                .clicked()
            {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("Gemstone project", &[project::EXTENSION])
                    .add_filter("All files", &["*"])
                    .pick_file()
                {
                    self.request = Some(ProjectRequest::Load { file });
                }
            }
            if let Some(dir) = &self.project_dir {
                ui.label(
                    egui::RichText::new(dir.display().to_string())
                        .small()
                        .color(egui::Color32::from_gray(130)),
                );
            }
        });
    }

    /// Where to save. An existing project saves in place unless the name has
    /// changed, in which case the new name gets its own folder beside it; a
    /// project that has never been saved asks where to put its folder.
    fn request_save(&mut self, stem: &str) {
        let existing = self.project_dir.clone().filter(|d| {
            d.file_name().is_some_and(|n| n == std::ffi::OsStr::new(stem))
        });
        let parent = match &existing {
            Some(dir) => return self.request = Some(ProjectRequest::Save {
                dir: dir.clone(),
                name: stem.to_string(),
            }),
            None => self
                .project_dir
                .as_ref()
                .and_then(|d| d.parent().map(PathBuf::from)),
        };
        let parent = match parent {
            Some(p) => Some(p),
            None => rfd::FileDialog::new()
                .set_title("Where should the project folder go?")
                .pick_folder(),
        };
        let Some(parent) = parent else { return };
        self.request = Some(ProjectRequest::Save {
            dir: parent.join(stem),
            name: stem.to_string(),
        });
    }

    /// One strip per row: the head on the left, the chain of frames scrolling on
    /// the right.
    fn lanes_ui(&mut self, ui: &mut egui::Ui, tracks: &[(u64, String)]) {
        let mut remove_row: Option<usize> = None;
        let playhead = self.playhead_units();
        // Resolved before the loop: `self.rows` is borrowed mutably inside it,
        // and the registry is what knows a drum track from an instrument.
        let percussion: Vec<bool> = self
            .rows
            .iter()
            .map(|r| r.track_id.is_some_and(|id| self.registry.is_percussion(id)))
            .collect();

        for (idx, row) in self.rows.iter_mut().enumerate() {
            let row_id = row.id;
            ui.push_id(row_id, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_gray(30))
                    .inner_margin(egui::Margin::same(6))
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            // ── Row head ──────────────────────────────────
                            ui.allocate_ui_with_layout(
                                egui::vec2(HEAD_W, ROW_H),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.horizontal(|ui| {
                                        // A row whose loaded source is missing
                                        // says so in the box itself, and says
                                        // what it was: picking anything from the
                                        // list is how it gets repaired.
                                        let label = match (&row.missing, row.track_id) {
                                            (Some(want), _) => {
                                                format!("⚠ missing: {}", want.describe())
                                            }
                                            (None, Some(id)) => tracks
                                                .iter()
                                                .find(|(t, _)| *t == id)
                                                .map(|(_, n)| n.clone())
                                                .unwrap_or_else(|| "— no track —".to_string()),
                                            (None, None) => "— no track —".to_string(),
                                        };
                                        // Salt = `row_id` (so two rows never
                                        // share a popup) + the track count. An
                                        // `egui::Area` measures itself only on
                                        // the first pass a given id is shown and
                                        // then caps the Ui, so the ScrollArea
                                        // clips the list and it measures the same
                                        // again — a box first opened with two
                                        // tracks stayed two tall. A new count is
                                        // a new Area, measured afresh.
                                        egui::ComboBox::from_id_salt(("track", row_id, tracks.len()))
                                            .width(168.0)
                                            .selected_text(label)
                                            .show_ui(ui, |ui| {
                                                if tracks.is_empty() {
                                                    ui.label("— no track —");
                                                }
                                                for (id, name) in tracks {
                                                    if ui
                                                        .selectable_label(
                                                            row.missing.is_none()
                                                                && row.track_id == Some(*id),
                                                            name,
                                                        )
                                                        .clicked()
                                                    {
                                                        row.track_id = Some(*id);
                                                        // Repaired: the row is
                                                        // an ordinary row again.
                                                        row.missing = None;
                                                    }
                                                }
                                            });
                                        if ui
                                            .button("🗑")
                                            .on_hover_text("Remove this row")
                                            .clicked()
                                        {
                                            remove_row = Some(idx);
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        if ui
                                            .button("➕ Add Note")
                                            .on_hover_text(
                                                "Append a note frame and, behind it, a \
                                                 space frame for the silence that \
                                                 follows.\n\nThe new frame repeats the \
                                                 last one in the row — pitch, note \
                                                 length and space — so a run of the \
                                                 same thing takes one click each.",
                                            )
                                            .clicked()
                                        {
                                            row.add_note(if percussion[idx] {
                                                DEFAULT_DRUM_PITCH
                                            } else {
                                                DEFAULT_PITCH
                                            });
                                        }
                                        ui.label("Gain");
                                        ui.spacing_mut().slider_width = 76.0;
                                        ui.add(
                                            egui::Slider::new(&mut row.gain, 0.0..=2.0)
                                                .fixed_decimals(2)
                                                .show_value(true),
                                        );
                                        ui.checkbox(&mut row.autosave, "auto")
                                            .on_hover_text(
                                                "Re-export this row's LeSynth Fourier \
                                                 grid into the project folder every time \
                                                 the project is saved, so the project \
                                                 carries what you last edited.\n\n\
                                                 Off keeps the .lsft already in the \
                                                 folder, which pins the sound as it was \
                                                 when it was first written.",
                                            );
                                    });
                                },
                            );

                            // ── The chain of frames ───────────────────────
                            egui::ScrollArea::horizontal()
                                .id_salt("lane")
                                // Room for the frames *and* the scrollbar under
                                // them, which would otherwise clip the cards.
                                .max_height(ROW_H + 12.0)
                                .show(ui, |ui| {
                                    Self::chain_ui(ui, row, playhead, percussion[idx]);
                                });
                        });
                    });
            });
            ui.add_space(4.0);
        }

        if let Some(idx) = remove_row {
            if idx < self.rows.len() {
                self.rows.remove(idx);
                self.status = "Removed a row.".to_string();
            }
        }
    }

    /// A row's frames, left to right in play order — the row's lead space, then
    /// each item as its note frame followed by its tied space frame. Editing a
    /// frame's length or pitch is immediate; everything after it simply shifts,
    /// because a frame's position is nothing but the sum of the lengths before
    /// it.
    fn chain_ui(ui: &mut egui::Ui, row: &mut Row, playhead: Option<f64>, percussion: bool) {
        // Deferred: removing a frame rewrites the list this loop walks.
        let mut pending_delete: Option<usize> = None;
        // Taken before the frames are drawn, so an edit made in one frame moves
        // the ones behind it only on the next pass — never mid-loop.
        let starts = row.starts();
        let row_id = row.id;

        ui.horizontal_top(|ui| {
            ui.set_min_height(ROW_H);
            if row.items.is_empty() {
                ui.label(
                    egui::RichText::new("Empty row — press “➕ Add Note”.")
                        .color(egui::Color32::from_gray(120)),
                );
            }
            // The lead space, ahead of the first note. It belongs to the row,
            // so it stays put when that note is deleted and leads the next one
            // instead. Drawn only when there is a note for it to lead.
            if !row.items.is_empty() {
                Self::frame_ui(ui, row_id, LEAD_SPACE_ID, None, &mut row.lead, false, percussion);
            }
            for (idx, (item, start)) in row.items.iter_mut().zip(starts).enumerate() {
                let sounding = playhead
                    .is_some_and(|p| p >= start as f64 && p < (start + item.dur.units()) as f64);
                if Self::frame_ui(
                    ui,
                    row_id,
                    item.id,
                    Some(&mut item.pitch),
                    &mut item.dur,
                    sounding,
                    percussion,
                ) {
                    pending_delete = Some(idx);
                }
                // The space tied to it, drawn right behind it and carrying no
                // delete button of its own: it leaves only with its note.
                Self::frame_ui(ui, row_id, item.id, None, &mut item.space, false, percussion);
            }
        });

        if let Some(idx) = pending_delete {
            row.delete_item(idx);
        }
    }

    /// One frame. Returns `true` when its delete button was pressed.
    ///
    /// `pitch` decides which frame this is: `Some` draws the blue note frame
    /// (pitch, two length boxes, delete); `None` the amber space frame, which has
    /// *no delete button* — a space leaves only with its note, and the row's lead
    /// space never leaves at all.
    ///
    /// `percussion` names the notes after what they hit on a drum kit.
    fn frame_ui(
        ui: &mut egui::Ui,
        row_id: u64,
        id: u64,
        pitch: Option<&mut u8>,
        dur: &mut Duration,
        sounding: bool,
        percussion: bool,
    ) -> bool {
        let space = pitch.is_none();
        let (fill, stroke, header) = match (space, sounding) {
            (true, _) => (
                egui::Color32::from_rgb(78, 62, 38),
                egui::Color32::from_rgb(186, 146, 84),
                egui::Color32::from_rgb(226, 190, 130),
            ),
            (false, true) => (
                egui::Color32::from_rgb(74, 100, 138),
                PLAYHEAD,
                egui::Color32::from_rgb(245, 225, 220),
            ),
            (false, false) => (
                egui::Color32::from_rgb(52, 66, 92),
                egui::Color32::from_rgb(110, 150, 200),
                egui::Color32::from_rgb(200, 218, 240),
            ),
        };

        let mut deleted = false;
        ui.allocate_ui_with_layout(
            egui::vec2(CARD_W, CARD_H),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::Frame::new()
                    .fill(fill)
                    .stroke(egui::Stroke::new(1.0, stroke))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::same(4))
                    .show(ui, |ui| {
                        let inner_w = CARD_W - 10.0;
                        ui.set_width(inner_w);
                        ui.set_min_height(CARD_H - 10.0);
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.spacing_mut().button_padding = egui::vec2(6.0, 2.0);

                        // Header: what the frame is and how long. The note frame
                        // also carries the delete button, laid out from the
                        // right so it keeps its corner and a long title
                        // truncates instead of pushing the card wider than its
                        // neighbours.
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if !space
                                        && ui
                                            .add(egui::Button::new("✖").small().frame(false))
                                            .on_hover_text(
                                                "Delete this note and the space tied to it",
                                            )
                                            .clicked()
                                    {
                                        deleted = true;
                                    }
                                    // The lead space says so, because it is the
                                    // one space that is not the silence after
                                    // some note and does not leave with one.
                                    let title = match &pitch {
                                        Some(p) => format!("{} · {}", note_label(**p, percussion), dur.label()),
                                        None if id == LEAD_SPACE_ID => {
                                            format!("lead · {}", dur.label())
                                        }
                                        None => format!("space · {}", dur.label()),
                                    };
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(title).small().color(header),
                                        )
                                        .truncate(),
                                    );
                                },
                            );
                        });

                        if let Some(pitch) = pitch {
                            // The popup measures itself once per id and keeps
                            // that width forever, so the id carries `percussion`
                            // — a row moved from a synth to a drum kit gets a
                            // fresh popup instead of one sized for bare pitches.
                            egui::ComboBox::from_id_salt(("pitch", row_id, id, percussion))
                                .width(inner_w)
                                .height(260.0)
                                .selected_text(note_label(*pitch, percussion))
                                .show_ui(ui, |ui| {
                                    for p in PITCH_MIN..=PITCH_MAX {
                                        ui.selectable_value(pitch, p, note_label(p, percussion));
                                    }
                                })
                                .response
                                .on_hover_text(if percussion {
                                    "Which drum to hit. The name is General MIDI's, \
                                     which is the map nearly every kit follows; the \
                                     note is what is actually sent."
                                } else {
                                    "Pitch"
                                });
                        }

                        // Both boxes reach zero, and for a space that is the
                        // point: a 0-length space is a placeholder that keeps
                        // its frame without putting any silence in the row.
                        egui::ComboBox::from_id_salt(("wholes", row_id, id, space))
                            .width(inner_w)
                            .height(260.0)
                            .selected_text(format!("{} whole", dur.wholes))
                            .show_ui(ui, |ui| {
                                for w in 0..=MAX_WHOLES {
                                    ui.selectable_value(&mut dur.wholes, w, format!("{w} whole"));
                                }
                            })
                            .response
                            .on_hover_text("Whole notes — the whole part of the length");

                        egui::ComboBox::from_id_salt(("frac", row_id, id, space))
                            .width(inner_w)
                            .selected_text(dur.frac.label())
                            .show_ui(ui, |ui| {
                                for f in Fraction::ALL {
                                    ui.selectable_value(&mut dur.frac, f, f.label());
                                }
                            })
                            .response
                            .on_hover_text(
                                "Fractional part of the length, down to a 1/256 — added \
                                 to the whole notes above",
                            );
                    });
            },
        );
        deleted
    }

    /// Play / stop, tempo, and where the transport currently is.
    fn transport_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let playing = self.player.is_some();
            let loading = self.preparing.is_some();
            let recording = self.recording.is_some();
            if ui
                .add_enabled(!playing && !loading, egui::Button::new("▶ Play"))
                .on_hover_text("Play every row from the start")
                .clicked()
            {
                self.start_playback(ui.ctx());
            }
            if ui
                .add_enabled(
                    !playing && !loading && !recording,
                    egui::Button::new(
                        egui::RichText::new("⏺ Record & Play Once").color(RECORD),
                    ),
                )
                .on_hover_text(
                    "Play the composition through exactly once and record your MIDI \
                     keyboard over it.\n\nWhat you play is appended as new rows when the \
                     pass ends (or when you press Stop), with every note and every \
                     silence rounded onto the note value in “Round”. Notes that sound \
                     *together* go into a row each — a row plays one note at a time — \
                     so a chord comes out as a row per voice; a key merely let go of \
                     late stays in the line and is trimmed.\n\nTo hear yourself while \
                     you play, open the track's editor: the keyboard feeds it as usual, \
                     and recording takes a copy rather than the events themselves.",
                )
                .clicked()
            {
                self.start_recording(ui.ctx());
            }
            // Stop also abandons a Play still loading its rows, and ends a take.
            if ui
                .add_enabled(playing || loading || recording, egui::Button::new("■ Stop"))
                .clicked()
            {
                self.stop_playback();
            }
            // Live: ticking this mid-play loops from the next pass (or straight
            // away, if the composition is already into its release tail).
            // Not during a take: "once" is the whole of what Record promises,
            // and a second pass would be recorded over the first.
            let mut repeat = self.repeat.load(Ordering::Relaxed);
            if ui
                .add_enabled_ui(!recording, |ui| ui.checkbox(&mut repeat, "🔁 Repeat"))
                .inner
                .on_hover_text(
                    "Play the composition over and over, looping on its written \
                     length. Releases ring on across the loop, and it can be \
                     switched on and off while it plays — untick it and the \
                     current pass is the last one.\n\nUnavailable while recording: \
                     a take is one pass.",
                )
                .changed()
            {
                self.repeat.store(repeat, Ordering::Relaxed);
            }
            let exporting = self.export.is_some();
            if ui
                .add_enabled(!exporting, egui::Button::new("🎵 Export WAV…"))
                .on_hover_text(
                    "Render the whole composition offline and write it as a \
                     16-bit .wav — every row, mixed exactly as the transport \
                     plays it.\n\nThe render runs in the background; the panel \
                     stays usable.",
                )
                .clicked()
            {
                self.export_wav();
            }
            ui.add_space(12.0);
            ui.label("Tempo");
            ui.add(
                egui::DragValue::new(&mut self.tempo_bpm)
                    .range(20.0..=300.0)
                    .speed(1.0)
                    .suffix(" BPM"),
            )
            .on_hover_text("A beat is a quarter note; every note length scales with this.");

            // What a take is rounded onto, and where it lands. Both belong here
            // rather than in a dialog: they are read the moment a take ends, so
            // the rounding can still be reconsidered while the rows load.
            ui.add_space(12.0);
            ui.label("Round");
            egui::ComboBox::from_id_salt("record_round")
                .width(72.0)
                // All nine note values at once: the default popup height cuts
                // the list off at about six and scrolls the rest out of sight.
                .height(9.0 * 44.0)
                .selected_text(record::round_label(self.round_units))
                .show_ui(ui, |ui| {
                    for (label, units) in record::ROUND_CHOICES {
                        ui.selectable_value(&mut self.round_units, units, label);
                    }
                })
                .response
                .on_hover_text(
                    "The note value a recorded take is rounded onto — every note \
                     start, every note length and every silence snaps to the nearest \
                     multiple of it.\n\nA position is rounded exactly: a silence that \
                     no single length can express is carried on placeholder frames \
                     behind the space, so nothing after it drifts. A note's *length* \
                     takes the nearest length the two select boxes can express, which \
                     is the one thing that can come out a fraction short.",
                );
            ui.label("into");
            let record_label = self
                .record_track
                .and_then(|id| self.registry.name_of(id))
                .unwrap_or_else(|| "— no track —".to_string());
            egui::ComboBox::from_id_salt(("record_track", self.registry.list().len()))
                .width(150.0)
                .selected_text(record_label)
                .show_ui(ui, |ui| {
                    if self.registry.list().is_empty() {
                        ui.label("— no track —");
                    }
                    for (id, name) in self.registry.list() {
                        ui.selectable_value(&mut self.record_track, Some(id), name);
                    }
                })
                .response
                .on_hover_text("The track the recorded rows will play.");

            ui.add_space(12.0);
            let length_secs = self.end_units() as f64 * self.secs_per_unit();
            if loading {
                ui.label(
                    egui::RichText::new("⏳ loading rows…").color(PLAYHEAD),
                );
            }
            // A take says how much of it there is so far. Counting note-ons in
            // the tap rather than draining it keeps the one conversion of a take
            // in one place — where it ends.
            if let Some(rec) = &self.recording {
                let notes = rec
                    .tap
                    .lock()
                    .map_or(0, |t| t.iter().filter(|(_, m)| is_note_on(*m)).count());
                ui.label(
                    egui::RichText::new(format!("⏺ {notes} note(s)")).color(RECORD),
                );
            }
            match &self.player {
                // While looping, the length that means anything is the loop's,
                // not "last note plus release".
                Some(p) if self.repeat.load(Ordering::Relaxed) => ui.label(
                    egui::RichText::new(format!(
                        "🔁 {:.1} s / {:.1} s",
                        p.position_secs(),
                        p.loop_secs()
                    ))
                    .color(PLAYHEAD),
                ),
                Some(p) => ui.label(
                    egui::RichText::new(format!(
                        "▶ {:.1} s / {:.1} s",
                        p.position_secs(),
                        p.total_secs
                    ))
                    .color(PLAYHEAD),
                ),
                None => ui.label(
                    egui::RichText::new(format!("{length_secs:.1} s"))
                        .color(egui::Color32::from_gray(160)),
                ),
            };
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new(&self.status).color(egui::Color32::from_gray(170)));

        // While playing, repaint fast enough for the sounding frame to light up
        // on time; otherwise stay idle like the rest of the app. An export has
        // nothing to animate but must not sit finished on screen unnoticed, so
        // it only asks for the occasional frame.
        if self.player.is_some() || self.preparing.is_some() || self.recording.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(16));
        } else if self.export.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
    }
}

/// Widget id of a row's lead space frame. Item ids count up from zero and are
/// handed out one at a time, so this cannot collide with one.
const LEAD_SPACE_ID: u64 = u64::MAX;

/// Colour of the transport's position readout, and of the frame sounding under
/// it.
const PLAYHEAD: egui::Color32 = egui::Color32::from_rgb(240, 120, 110);

/// Colour of the record button and of a take's note count.
const RECORD: egui::Color32 = egui::Color32::from_rgb(235, 90, 90);

/// Whether a MIDI message starts a note. The zero-velocity note-on most
/// keyboards send instead of a note-off does not.
fn is_note_on(msg: [u8; 3]) -> bool {
    msg[0] & 0xF0 == 0x90 && msg[2] > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUARTER: i64 = UNITS_PER_WHOLE / 4;
    const EIGHTH: i64 = UNITS_PER_WHOLE / 8;

    /// `(pitch, note length, space length)` per item.
    fn row_with(items: &[(u8, Duration, Duration)]) -> Row {
        let mut row = Row::new(0, None);
        for (pitch, dur, space) in items {
            let id = row.next_item_id;
            row.next_item_id += 1;
            row.items.push(Item {
                id,
                pitch: *pitch,
                dur: *dur,
                space: *space,
            });
        }
        row
    }

    /// Shorthand for a length with no whole-note part.
    const fn frac(f: Fraction) -> Duration {
        Duration::new(0, f)
    }

    /// A panel with one row of `items`, playing track `track`.
    fn panel_with(items: &[(u8, Duration, Duration)], track: Option<u64>) -> ComposerPanel {
        let mut panel = ComposerPanel::new(TrackRegistry::default(), crate::midi::new_midi_taps());
        let mut row = row_with(items);
        row.track_id = track;
        panel.rows.push(row);
        panel
    }

    /// Adding a note repeats the frame before it **whole** — pitch, note length
    /// and space, whole notes and fraction alike. Building a row means repeating
    /// something far more often than changing it — a hi-hat line, a run on one
    /// drum, a rhythm of one length — so none of the five select boxes should
    /// have to be touched again for the next frame.
    #[test]
    fn a_new_note_carries_on_from_the_one_before_it() {
        let mut row = Row::new(0, None);
        // The first note of a row has nothing to follow, so it takes the
        // defaults: middle-of-the-road lengths and the caller's pitch.
        row.add_note(DEFAULT_DRUM_PITCH);
        assert_eq!(row.items[0].pitch, DEFAULT_DRUM_PITCH);
        assert_eq!(row.items[0].dur, Duration::new(0, Fraction::Quarter));
        assert_eq!(row.items[0].space, Duration::new(0, Fraction::Eighth));

        // Change every box of it, and everything added after follows all of them.
        row.items[0].pitch = 42;
        row.items[0].dur = Duration::new(2, Fraction::Sixteenth);
        row.items[0].space = Duration::new(1, Fraction::Half);
        row.add_note(DEFAULT_DRUM_PITCH);
        row.add_note(DEFAULT_DRUM_PITCH);
        for i in [1, 2] {
            assert_eq!(row.items[i].pitch, 42, "the pitch was not carried over");
            assert_eq!(
                row.items[i].dur,
                Duration::new(2, Fraction::Sixteenth),
                "the note length was not carried over"
            );
            assert_eq!(
                row.items[i].space,
                Duration::new(1, Fraction::Half),
                "the space was not carried over"
            );
        }

        // It follows the *last* frame, not the first.
        row.items[2].pitch = 46;
        row.items[2].dur = Duration::new(0, Fraction::ThirtySecond);
        row.add_note(DEFAULT_DRUM_PITCH);
        assert_eq!(row.items[3].pitch, 46);
        assert_eq!(row.items[3].dur, Duration::new(0, Fraction::ThirtySecond));

        // Every frame still gets an id of its own, or two of them would share
        // their widgets.
        let ids: std::collections::HashSet<u64> = row.items.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), row.items.len(), "two frames share an id");

        // A row emptied of notes starts from the defaults again.
        row.items.clear();
        row.add_note(DEFAULT_PITCH);
        assert_eq!(row.items[0].pitch, DEFAULT_PITCH);
        assert_eq!(row.items[0].dur, Duration::new(0, Fraction::Quarter));
        assert_eq!(row.items[0].space, Duration::new(0, Fraction::Eighth));
    }

    /// Pressing Play must not wait for the plugins.
    ///
    /// The Composer loads one instance per row, and that is hundreds of
    /// milliseconds — seconds, for a big synth or a long composition. Doing it
    /// in the button's own call freezes the window until the first sound, which
    /// is what "the transport lags" means. So Play hands the loading to a thread
    /// and returns: the test is that it comes back with no transport yet, and
    /// that one turns up on a later frame.
    #[test]
    fn pressing_play_hands_the_loading_to_a_thread() {
        let registry = TrackRegistry::default();
        let plugin = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("internal_plugins")
            .join("liblesynth_fourier.so");
        if !plugin.exists() {
            println!("no internal plugin — nothing to load");
            return;
        }
        let id = registry.add(
            "LeSynth",
            plugin,
            Some(crate::vst::class_ids::FOURIER_SYNTH),
            true,
            None,
        );
        let mut panel = ComposerPanel::new(registry, crate::midi::new_midi_taps());
        let mut row = row_with(&[(60, frac(Fraction::Quarter), frac(Fraction::Eighth))]);
        row.track_id = Some(id);
        panel.rows.push(row);

        let pressed = std::time::Instant::now();
        panel.start_playback(&egui::Context::default());
        let button_took = pressed.elapsed();
        assert!(
            panel.preparing.is_some(),
            "Play did not start loading: {}",
            panel.status
        );
        assert!(
            panel.player.is_none(),
            "Play waited for the transport instead of handing it off"
        );
        assert!(
            button_took < std::time::Duration::from_millis(100),
            "the button itself took {button_took:?}"
        );

        // The transport turns up on a later frame, without the button's help.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while panel.player.is_none() && std::time::Instant::now() < deadline {
            panel.poll_preparation();
            if panel.status.starts_with("Playback failed") {
                println!("nothing to play through ({}) — the rest needs a device", panel.status);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(panel.player.is_some(), "the transport never started: {}", panel.status);
        assert!(panel.status.starts_with("Playing"), "status reads {:?}", panel.status);

        panel.stop_playback();
        assert!(panel.player.is_none() && panel.preparing.is_none());
    }

    /// A drum track names its notes after what they hit; an instrument track
    /// leaves them as pitches. The name is *added* to the pitch, never swapped
    /// for it — a kit that is not General MIDI still shows what is being sent.
    #[test]
    fn a_drum_row_labels_its_notes_with_the_drum() {
        assert_eq!(note_label(36, false), "C2");
        assert_eq!(note_label(36, true), "C2 · Bass Drum (Kick)");
        assert_eq!(note_label(42, true), "F#2 · Closed Hi-Hat");
        // Outside the General MIDI percussion range there is nothing to add.
        assert_eq!(note_label(100, true), "E7");
        // And the first note on a drum row is the kick, not middle C's bongo.
        assert_eq!(gm_percussion_name(DEFAULT_DRUM_PITCH), Some("Bass Drum (Kick)"));
        assert_eq!(gm_percussion_name(DEFAULT_PITCH), Some("Hi Bongo"));
    }

    /// What a live edit may and may not change, which is what decides whether the
    /// looping transport can take it or the user has to press Play again.
    ///
    /// The panel compares these snapshots every frame while a repeat runs, so
    /// "nothing changed" has to come out equal — otherwise the transport would be
    /// handed the same composition over and over.
    #[test]
    fn a_live_snapshot_separates_note_edits_from_track_changes() {
        let items = [(60, frac(Fraction::Quarter), frac(Fraction::Eighth))];
        let panel = panel_with(&items, Some(7));
        let before = panel.live_snapshot();
        assert_eq!(
            before.rows,
            panel.live_snapshot().rows,
            "an untouched composition must snapshot equal, or every frame \
             republishes it"
        );

        // A note edit: same rows and tracks, different schedule — the transport
        // can swap this in at the loop point.
        let mut edited = panel_with(&items, Some(7));
        edited.rows[0].items[0].pitch = 64;
        let after = edited.live_snapshot();
        assert_eq!(after.shape, before.shape, "a pitch change is not structural");
        assert_ne!(after.rows, before.rows, "the pitch change was not noticed");

        // Tempo moves every note *and* the loop length.
        let mut faster = panel_with(&items, Some(7));
        faster.tempo_bpm = 180.0;
        let after = faster.live_snapshot();
        assert_eq!(after.shape, before.shape, "tempo is not structural");
        assert_ne!(after.rows, before.rows, "the tempo change did not move the notes");
        assert_ne!(after.loop_secs, before.loop_secs, "the loop length did not follow");

        // A different track means a different plugin, which an audio callback
        // cannot load: structural, and reported rather than applied.
        let moved = panel_with(&items, Some(8));
        assert_ne!(
            moved.live_snapshot().shape,
            before.shape,
            "pointing a row at another track must count as structural"
        );

        // A row with nothing to play is in neither the plans nor the snapshot.
        let silent = panel_with(&[(60, frac(Fraction::None), frac(Fraction::Eighth))], Some(7));
        assert!(silent.live_snapshot().rows.is_empty());
    }

    /// The requirement: a row opens with a space before its very first note, and
    /// everything the row plays waits for it.
    #[test]
    fn a_row_opens_with_a_space_before_its_first_note() {
        let mut row = row_with(&[
            (60, frac(Fraction::Quarter), frac(Fraction::None)),
            (62, frac(Fraction::Quarter), frac(Fraction::None)),
        ]);
        // Zero by default, so a row still starts at time zero.
        assert_eq!(row.lead.units(), 0);
        assert_eq!(row.starts(), vec![0, QUARTER]);

        row.lead = frac(Fraction::Eighth);
        assert_eq!(
            row.starts(),
            vec![EIGHTH, EIGHTH + QUARTER],
            "the lead space must push every note in the row, not just the first"
        );
        assert_eq!(row.end_units(), EIGHTH + 2 * QUARTER);
        let notes = row.planned_notes(1.0);
        assert_eq!(notes[0].at_secs, EIGHTH as f64);
        assert_eq!(notes[1].at_secs, (EIGHTH + QUARTER) as f64);
    }

    /// The other half of it: deleting the first note must leave the lead space
    /// where it is, leading whichever note is first afterwards. It belongs to
    /// the row, so there is no path that can take it away with a note.
    #[test]
    fn deleting_the_first_note_keeps_the_lead_space_for_the_next_one() {
        let mut row = row_with(&[
            (60, frac(Fraction::Quarter), frac(Fraction::None)),
            (62, frac(Fraction::Half), frac(Fraction::None)),
        ]);
        row.lead = frac(Fraction::Eighth);

        row.delete_item(0);

        assert_eq!(row.lead, frac(Fraction::Eighth), "the lead space went with the note");
        assert_eq!(row.items.len(), 1);
        assert_eq!(row.items[0].pitch, 62);
        assert_eq!(
            row.starts(),
            vec![EIGHTH],
            "the note that is first now must wait through the same lead space"
        );

        // And deleting the last note leaves the lead intact for the next one
        // added, though an empty row has no first note to lead and so no delay.
        row.delete_item(0);
        assert!(row.items.is_empty());
        assert_eq!(row.lead, frac(Fraction::Eighth));
        assert_eq!(row.end_units(), 0, "an empty row has nothing to wait for");
        row.add_note(DEFAULT_PITCH);
        assert_eq!(row.starts(), vec![EIGHTH]);
    }

    /// The requirement: one press of "Add Note" leaves a note *and* the space
    /// behind it.
    #[test]
    fn adding_a_note_appends_the_note_and_a_space_behind_it() {
        let mut row = row_with(&[]);
        row.add_note(DEFAULT_PITCH);
        assert_eq!(row.items.len(), 1);
        assert_eq!(row.items[0].pitch, DEFAULT_PITCH);
        assert!(row.items[0].dur.units() > 0);
        assert!(row.items[0].space.units() > 0);

        row.add_note(DEFAULT_PITCH);
        assert_eq!(row.items.len(), 2);
        // Ids stay unique, so two items never share a widget id.
        assert_ne!(row.items[0].id, row.items[1].id);
    }

    /// The space is tied to its note: there is no way to remove one and keep the
    /// other, and deleting the note takes its space with it.
    #[test]
    fn a_space_can_only_leave_with_the_note_it_is_tied_to() {
        let mut row = row_with(&[
            (60, frac(Fraction::Quarter), frac(Fraction::Half)),
            (64, frac(Fraction::Quarter), frac(Fraction::Eighth)),
        ]);
        assert_eq!(
            row.end_units(),
            QUARTER + UNITS_PER_WHOLE / 2 + QUARTER + EIGHTH
        );

        // Delete the first note: its half-note space goes too, so the row loses
        // both lengths and the second note starts at zero.
        row.delete_item(0);
        assert_eq!(row.items.len(), 1);
        assert_eq!(row.items[0].pitch, 64);
        assert_eq!(row.end_units(), QUARTER + EIGHTH);
        assert_eq!(row.starts(), vec![0]);
    }

    /// A space of length zero is legal — the placeholder case: the frame stays,
    /// the row does not grow, and the next note follows on immediately.
    #[test]
    fn a_space_may_be_zero_length() {
        let spu = 1.0;
        let row = row_with(&[
            (60, frac(Fraction::Quarter), Duration::new(0, Fraction::None)),
            (64, frac(Fraction::Quarter), frac(Fraction::Eighth)),
        ]);
        // The zero space is still an item's space — nothing is dropped …
        assert_eq!(row.items[0].space.units(), 0);
        assert_eq!(row.items[0].space.label(), "0");
        // … and it adds no time: the second note starts as the first one ends.
        let notes = row.planned_notes(spu);
        assert_eq!(notes[1].at_secs, QUARTER as f64);
        assert_eq!(row.end_units(), QUARTER + QUARTER + EIGHTH);
    }

    /// A length is the whole part plus the fractional part, and 1/256 is the
    /// finest value the boxes offer.
    #[test]
    fn a_length_is_its_whole_part_plus_its_fraction() {
        assert_eq!(Duration::new(0, Fraction::None).units(), 0);
        assert_eq!(Duration::new(1, Fraction::None).units(), UNITS_PER_WHOLE);
        assert_eq!(
            Duration::new(1, Fraction::Half).units(),
            UNITS_PER_WHOLE + UNITS_PER_WHOLE / 2
        );
        // A dotted half: 1/2 + 1/4.
        assert_eq!(
            Duration::new(0, Fraction::Half).units() + Duration::new(0, Fraction::Quarter).units(),
            UNITS_PER_WHOLE * 3 / 4
        );
        // Every fraction is a whole number of units, halving down to a 1/256.
        assert_eq!(Fraction::TwoHundredFiftySixth.units(), 1);
        for pair in Fraction::ALL[1..].windows(2) {
            assert_eq!(pair[1].units(), pair[0].units() / 2);
        }
        assert_eq!(Duration::new(2, Fraction::Eighth).label(), "2 + 1/8");
        assert_eq!(Duration::new(0, Fraction::Eighth).label(), "1/8");
        assert_eq!(Duration::new(3, Fraction::None).label(), "3");
    }

    /// Frames are played one after another: a note starts where the previous
    /// note's space ended, and a space is silence of exactly its own length.
    #[test]
    fn frames_play_in_sequence_and_spaces_are_the_silences() {
        let spu = 1.0; // one second per unit keeps the arithmetic readable
        let row = row_with(&[
            (60, frac(Fraction::Quarter), frac(Fraction::Eighth)),
            (64, frac(Fraction::Quarter), frac(Fraction::None)),
        ]);
        let notes = row.planned_notes(spu);
        assert_eq!(notes.len(), 2); // the spaces are not played
        assert_eq!(notes[0].at_secs, 0.0);
        assert_eq!(notes[0].dur_secs, QUARTER as f64);
        // The second note waits out the quarter *and* the eighth of silence.
        assert_eq!(notes[1].at_secs, (QUARTER + EIGHTH) as f64);
        assert_eq!(notes[1].pitch, 64);
        assert_eq!(row.end_units(), QUARTER + EIGHTH + QUARTER);

        // A note left at length zero is silence, not a click — but its space
        // still counts, so what follows keeps its place.
        let row = row_with(&[
            (60, frac(Fraction::None), frac(Fraction::Eighth)),
            (62, frac(Fraction::Half), frac(Fraction::None)),
        ]);
        let notes = row.planned_notes(spu);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, 62);
        assert_eq!(notes[0].at_secs, EIGHTH as f64);
    }

    /// Every row starts at zero, so rows sound together exactly when the lengths
    /// before a note add up the same — that is the only thing that makes a chord
    /// now that nothing is positioned by hand.
    #[test]
    fn equal_lengths_before_a_note_make_it_sound_with_another_row() {
        let spu = 0.5 / UNITS_PER_BEAT as f64; // 120 BPM

        // Two eighths played back to back, then the note.
        let dense = row_with(&[
            (60, frac(Fraction::Eighth), frac(Fraction::None)),
            (62, frac(Fraction::Eighth), frac(Fraction::None)),
            (64, Duration::new(1, Fraction::None), frac(Fraction::None)),
        ]);
        // A note cut short and a long space instead — the same moment, reached
        // by a different route.
        let sparse = row_with(&[
            (59, frac(Fraction::Eighth), frac(Fraction::Eighth)),
            (67, Duration::new(1, Fraction::None), frac(Fraction::None)),
        ]);

        let third = dense.planned_notes(spu)[2].at_secs;
        let second = sparse.planned_notes(spu)[1].at_secs;
        assert_eq!(third, second);
        assert_eq!(third, (EIGHTH + EIGHTH) as f64 * spu);
        // …and a row whose lengths do not add up the same does not join them.
        let off = row_with(&[
            (59, frac(Fraction::Sixteenth), frac(Fraction::None)),
            (67, Duration::new(1, Fraction::None), frac(Fraction::None)),
        ]);
        assert_ne!(off.planned_notes(spu)[1].at_secs, third);
    }

    /// Deleting from the middle closes the gap: everything behind moves earlier
    /// by exactly the note *and* space that went with it.
    #[test]
    fn deleting_a_frame_pulls_the_rest_forward() {
        let mut row = row_with(&[
            (60, frac(Fraction::Quarter), frac(Fraction::None)),
            (62, frac(Fraction::Quarter), frac(Fraction::Quarter)),
            (64, frac(Fraction::Quarter), frac(Fraction::None)),
        ]);
        assert_eq!(row.starts(), vec![0, QUARTER, 3 * QUARTER]);
        row.delete_item(1); // takes its own quarter of silence with it
        assert_eq!(row.items.len(), 2);
        assert_eq!(row.starts(), vec![0, QUARTER]);
        assert_eq!(row.end_units(), 2 * QUARTER);
    }

    #[test]
    fn pitch_names_follow_scientific_notation() {
        assert_eq!(pitch_name(60), "C4");
        assert_eq!(pitch_name(61), "C#4");
        assert_eq!(pitch_name(PITCH_MIN), "C0");
        assert_eq!(pitch_name(PITCH_MAX), "B8");
    }

    fn registry_with(names: &[&str]) -> (TrackRegistry, Vec<u64>) {
        let registry = TrackRegistry::default();
        let ids = names
            .iter()
            .map(|n| {
                registry.add(
                    *n,
                    std::path::PathBuf::from("/nonexistent/plugin.so"),
                    None,
                    false,
                    None,
                )
            })
            .collect();
        (registry, ids)
    }

    /// A row must never point at a track that is gone: the requirement is that
    /// deleting the track behind a row silently re-points it, that an empty
    /// registry leaves the select box on its placeholder, and that a track added
    /// afterwards is adopted.
    #[test]
    fn a_row_follows_the_track_list_as_tracks_come_and_go() {
        let (registry, ids) = registry_with(&["one", "two"]);
        let mut panel = ComposerPanel::new(registry.clone(), crate::midi::new_midi_taps());
        panel.add_row();
        assert_eq!(panel.rows[0].track_id, Some(ids[0]));

        // The track this row plays is deleted → it takes the next available one.
        registry.remove(ids[0]);
        panel.reconcile_tracks();
        assert_eq!(panel.rows[0].track_id, Some(ids[1]));

        // Every track deleted → the placeholder, not a dangling id.
        registry.remove(ids[1]);
        panel.reconcile_tracks();
        assert_eq!(panel.rows[0].track_id, None);

        // A track exists again → the row picks it up.
        let late = registry.add(
            "late",
            std::path::PathBuf::from("/nonexistent/plugin.so"),
            None,
            false,
            None,
        );
        panel.reconcile_tracks();
        assert_eq!(panel.rows[0].track_id, Some(late));
    }

    /// Lay the whole panel out for real (headless egui, no window) in the states
    /// that have layout of their own: no rows, an empty row, a row of note and
    /// space frames, two rows, and a row whose track list has gone empty.
    /// Catches the panics a layout test can catch — duplicate widget ids, bad
    /// rects — which no amount of model testing would.
    #[test]
    fn the_panel_lays_out_in_every_state_without_panicking() {
        let (registry, ids) = registry_with(&["one"]);
        let mut panel = ComposerPanel::new(registry.clone(), crate::midi::new_midi_taps());
        let ctx = egui::Context::default();
        let frame = |panel: &mut ComposerPanel| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| panel.ui(ui));
            });
        };

        frame(&mut panel); // no rows: just the add button and the transport

        // The transport in its recording state — the note-count readout, the
        // rounding and target select boxes, and Repeat disabled beneath them.
        // Started with no rows, so nothing is loading behind it.
        panel.start_recording(&ctx);
        frame(&mut panel);
        let _ = panel.finish_recording();

        panel.add_row();
        frame(&mut panel); // a row with no frames yet

        panel.rows[0].add_note(DEFAULT_PITCH);
        panel.rows[0].add_note(DEFAULT_PITCH);
        frame(&mut panel);

        // The lead space is drawn too, and must lay out at a real length as
        // well as at its zero default.
        panel.rows[0].lead = Duration::new(MAX_WHOLES, Fraction::TwoHundredFiftySixth);
        frame(&mut panel);

        // Two rows on the same track, which the spec allows. The longest length
        // the boxes can name, and a zero-length placeholder space, are the two
        // extremes a frame has to lay out at.
        panel.add_row();
        panel.rows[1].add_note(DEFAULT_PITCH);
        panel.rows[1].items[0].dur = Duration::new(MAX_WHOLES, Fraction::TwoHundredFiftySixth);
        panel.rows[1].items[0].space = Duration::new(0, Fraction::None);
        frame(&mut panel);

        // A deleted item must not leave a stale widget id behind — neither for
        // its note frame nor for the space that went with it.
        panel.rows[0].delete_item(0);
        frame(&mut panel);

        registry.remove(ids[0]);
        frame(&mut panel);
        assert!(panel.rows.iter().all(|r| r.track_id.is_none()));
    }
    /// A take with nothing to play against is still a take: it runs from the
    /// button press, and what was played comes back as rows of the composition —
    /// rounded, on the track the recorder was pointed at.
    #[test]
    fn a_take_becomes_rows_on_the_composition() {
        let (registry, ids) = registry_with(&["one"]);
        let mut panel = ComposerPanel::new(registry, crate::midi::new_midi_taps());
        let ctx = egui::Context::default();
        panel.reconcile_tracks();
        panel.start_recording(&ctx);
        // Nothing to play along to, so no transport was started — the take is
        // timed from the press instead.
        assert!(panel.player.is_none() && panel.preparing.is_none());
        let tap = panel.recording.as_ref().expect("a take is running").tap.clone();

        // The keyboard, as the MIDI callback delivers it: two quarter notes at
        // the default 120 BPM, the second a beat after the first.
        let now = Instant::now();
        let at = |ms: u64| now + std::time::Duration::from_millis(ms);
        tap.lock().unwrap().extend([
            (at(0), [0x90, 60, 100]),
            (at(480), [0x80, 60, 0]),
            (at(500), [0x90, 64, 100]),
            (at(990), [0x80, 64, 0]),
        ]);

        let status = panel.finish_recording().expect("the take reports itself");
        assert!(status.contains("2 note(s)"), "{status}");
        assert_eq!(panel.rows.len(), 1);
        assert_eq!(panel.rows[0].track_id, Some(ids[0]));
        assert_eq!(
            panel.rows[0].items.iter().map(|i| i.pitch).collect::<Vec<_>>(),
            vec![60, 64]
        );
        // Rounded to 1/16 and back to seconds: a quarter note each, a beat apart.
        let played = panel.rows[0].planned_notes(panel.secs_per_unit());
        assert_eq!(played.len(), 2);
        assert!((played[0].at_secs - 0.0).abs() < 1e-9, "{played:?}");
        assert!((played[1].at_secs - 0.5).abs() < 1e-9, "{played:?}");
        assert!((played[0].dur_secs - 0.5).abs() < 1e-9, "{played:?}");
        // The take is over: the tap is emptied and nothing is left running.
        assert!(panel.recording.is_none());
        assert!(tap.lock().unwrap().is_empty());
    }

    /// A row's track select box must offer every track in the registry, not the
    /// ones that happened to exist when the row was added.
    ///
    /// Driven with a real click on the real popup Area, and — crucially — the
    /// box is **opened before the tracks are created**, which is what used to
    /// freeze it (an Area measures itself once per id). The assertion is on the
    /// text actually painted, so it fails whether the list is stale, the popup is
    /// clipped away, or two rows collide on one widget id.
    #[test]
    fn a_rows_select_box_offers_tracks_created_after_the_row() {
        let registry = TrackRegistry::default();
        for n in 1..=2 {
            registry.add(format!("t{n}"), std::path::PathBuf::from("/x"), None, false, None);
        }
        let mut panel = ComposerPanel::new(registry.clone(), crate::midi::new_midi_taps());
        let ctx = egui::Context::default();
        // The app's own spacing and text sizes: how tall a popup item is decides
        // how much of the list a stuck popup can still show, so the default
        // style hides the defect this exists to catch.
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        {
            use egui::{FontFamily::Proportional, FontId, TextStyle};
            style.text_styles = [
                (TextStyle::Heading, FontId::new(18.0, Proportional)),
                (TextStyle::Body, FontId::new(14.0, Proportional)),
                (TextStyle::Button, FontId::new(14.0, Proportional)),
                (TextStyle::Monospace, FontId::new(13.0, egui::FontFamily::Monospace)),
                (TextStyle::Small, FontId::new(11.0, Proportional)),
            ]
            .into();
        }
        ctx.set_style(style);

        // Every string the frame paints, popups included, with where it landed.
        let run = |panel: &mut ComposerPanel, input: egui::RawInput| -> Vec<String> {
            let out = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| panel.ui(ui));
                });
            });
            let mut texts = Vec::new();
            fn walk(sh: &egui::Shape, out: &mut Vec<String>) {
                match sh {
                    egui::Shape::Text(t) => {
                        out.push(format!("{}@{},{}", t.galley.text(), t.pos.x, t.pos.y))
                    }
                    egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                    _ => {}
                }
            }
            for cs in &out.shapes {
                walk(&cs.shape, &mut texts);
            }
            texts
        };
        let click = |at: egui::Pos2| {
            let mut i = egui::RawInput::default();
            i.events = vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ];
            i
        };
        // What the popup that opened under `y` is listing: the items sit in the
        // popup's own left column, below the button that opened it.
        let listed = |texts: &[String], y: f32| -> Vec<String> {
            texts
                .iter()
                .filter_map(|s| {
                    let (name, pos) = s.rsplit_once('@')?;
                    let (nx, ny) = pos.split_once(',')?;
                    let (nx, ny): (f32, f32) = (nx.parse().ok()?, ny.parse().ok()?);
                    // The popup's own column: its frame indents its items past
                    // the row controls behind it.
                    ((29.0..40.0).contains(&nx) && ny > y + 10.0 && ny < y + 260.0)
                        .then(|| name.to_string())
                })
                .collect()
        };

        // Where the row heads' select boxes are, found from the painted track
        // name rather than hardcoded — the panel grows a header now and then,
        // and a test that silently clicks past it proves nothing.
        let boxes = |texts: &[String]| -> Vec<egui::Pos2> {
            texts
                .iter()
                .filter_map(|s| {
                    let (name, pos) = s.rsplit_once('@')?;
                    let (nx, ny) = pos.split_once(',')?;
                    let (nx, ny): (f32, f32) = (nx.parse().ok()?, ny.parse().ok()?);
                    (name.starts_with('t') && (20.0..28.0).contains(&nx))
                        .then(|| egui::pos2(nx + 66.0, ny + 2.0))
                })
                .collect()
        };

        panel.add_row();
        let laid_out = run(&mut panel, egui::RawInput::default());

        // The first row's select box, opened once while only one track exists.
        // Opening it is the whole point: that first view is what its popup used
        // to be stuck at.
        let box1 = *boxes(&laid_out).first().expect("the row's select box is drawn");
        run(&mut panel, click(box1));
        let open = run(&mut panel, egui::RawInput::default());
        assert_eq!(
            listed(&open, box1.y),
            vec!["t1", "t2"],
            "the popup did not open over the first row's select box — the click \
             missed, and this test is not testing anything: {open:?}"
        );

        // Close it, add a second row, then create another track.
        run(&mut panel, click(egui::pos2(900.0, 900.0)));
        let closed = run(&mut panel, egui::RawInput::default());
        assert!(
            listed(&closed, box1.y).is_empty(),
            "the popup was still open, so reopening it proves nothing: {closed:?}"
        );
        panel.add_row();
        run(&mut panel, egui::RawInput::default());
        for n in 3..=4 {
            registry.add(format!("t{n}"), std::path::PathBuf::from("/x"), None, false, None);
        }
        let laid_out = run(&mut panel, egui::RawInput::default());
        let box2 = *boxes(&laid_out).get(1).expect("the second row's select box is drawn");

        // The first row must now offer them too. It is a fresh popup Area, so it
        // takes a sizing pass to appear — hence the second frame.
        run(&mut panel, click(box1));
        run(&mut panel, egui::RawInput::default());
        let reopened = run(&mut panel, egui::RawInput::default());
        assert_eq!(
            listed(&reopened, box1.y),
            vec!["t1", "t2", "t3", "t4"],
            "tracks created after this row's select box was first opened are \
             missing from it: {reopened:?}"
        );

        // And the row added later, which never had a popup of its own.
        run(&mut panel, click(egui::pos2(900.0, 900.0)));
        run(&mut panel, egui::RawInput::default());
        run(&mut panel, click(box2));
        run(&mut panel, egui::RawInput::default());
        let second = run(&mut panel, egui::RawInput::default());
        assert_eq!(
            listed(&second, box2.y),
            vec!["t1", "t2", "t3", "t4"],
            "the second row's select box is missing a track: {second:?}"
        );
    }

    /// The requirement's second half: a project whose grid has been deleted must
    /// load with that row marked, must not have some other track silently
    /// adopted onto it, must still say what it wanted, and must **survive being
    /// saved again** — losing the file name on the next save would make the
    /// damage permanent.
    #[test]
    fn a_missing_source_is_kept_until_the_user_replaces_it() {
        let (registry, ids) = registry_with(&["other"]);
        let mut panel = ComposerPanel::new(registry.clone(), crate::midi::new_midi_taps());
        let loaded = project::Project {
            name: "Song".to_string(),
            tempo_bpm: 100.0,
            rows: vec![project::ProjectRow {
                track_name: "Voice".to_string(),
                source: project::TrackSource::LeSynth { file: "Voice.lsft".to_string() },
                gain: 0.5,
                lead: Duration::new(0, Fraction::Eighth),
                autosave: false,
                items: vec![Item {
                    id: 0,
                    pitch: 62,
                    dur: Duration::new(0, Fraction::Quarter),
                    space: Duration::new(0, Fraction::None),
                }],
            }],
        };
        // The app could not bind it: the grid is gone.
        panel.apply_project(&loaded, std::path::PathBuf::from("/tmp/Song"), &[None]);

        assert_eq!(panel.rows.len(), 1);
        assert!(panel.rows[0].missing.is_some(), "the row must be marked");
        assert_eq!(panel.rows[0].track_id, None);
        assert_eq!(panel.rows[0].gain, 0.5);
        assert!(!panel.rows[0].autosave, "autosave must round-trip");
        assert_eq!(panel.rows[0].items.len(), 1);

        // A track exists, and the row must NOT be quietly moved onto it.
        panel.reconcile_tracks();
        assert_eq!(panel.rows[0].track_id, None, "an unresolved row was adopted silently");

        // Saving again keeps the reference rather than erasing it.
        let again = panel.to_project("Song", |_| project::TrackSource::None);
        assert_eq!(
            again.rows[0].source,
            project::TrackSource::LeSynth { file: "Voice.lsft".to_string() },
            "a re-save lost what the row was looking for"
        );
        assert_eq!(again.rows[0].track_name, "Voice.lsft");

        // The user picks a replacement: the row becomes ordinary again.
        panel.rows[0].track_id = Some(ids[0]);
        panel.rows[0].missing = None;
        panel.reconcile_tracks();
        assert_eq!(panel.rows[0].track_id, Some(ids[0]));
        let fixed = panel.to_project("Song", |_| project::TrackSource::LeSynthDefault);
        assert_eq!(fixed.rows[0].source, project::TrackSource::LeSynthDefault);
    }

    /// Loading replaces the composition, and everything on a row that is not the
    /// notes has to come back too — a project that forgets the tempo or a row's
    /// lead is not a saved project.
    #[test]
    fn a_loaded_project_restores_every_row_setting() {
        let (registry, ids) = registry_with(&["a", "b"]);
        let mut panel = ComposerPanel::new(registry, crate::midi::new_midi_taps());
        panel.add_row();
        let loaded = project::Project {
            name: "Two".to_string(),
            tempo_bpm: 88.0,
            rows: vec![
                project::ProjectRow {
                    track_name: "a".to_string(),
                    source: project::TrackSource::LeSynthDefault,
                    gain: 1.5,
                    lead: Duration::new(1, Fraction::Sixteenth),
                    autosave: true,
                    items: vec![],
                },
                project::ProjectRow {
                    track_name: "b".to_string(),
                    source: project::TrackSource::LeSynthDefault,
                    gain: 0.25,
                    lead: Duration::new(0, Fraction::None),
                    autosave: false,
                    items: vec![Item {
                        id: 0,
                        pitch: 70,
                        dur: Duration::new(0, Fraction::Half),
                        space: Duration::new(0, Fraction::Quarter),
                    }],
                },
            ],
        };
        panel.apply_project(&loaded, std::path::PathBuf::from("/tmp/Two"), &[
            Some(ids[0]),
            Some(ids[1]),
        ]);

        assert_eq!(panel.rows.len(), 2, "the old row must be replaced, not appended to");
        assert_eq!(panel.tempo_bpm, 88.0);
        assert_eq!(panel.rows[0].gain, 1.5);
        assert_eq!(panel.rows[0].lead, Duration::new(1, Fraction::Sixteenth));
        assert_eq!(panel.rows[1].track_id, Some(ids[1]));
        assert!(!panel.rows[1].autosave);
        assert_eq!(panel.rows[1].items[0].pitch, 70);
        // Row ids stay unique, so two rows never share egui widget state.
        assert_ne!(panel.rows[0].id, panel.rows[1].id);
        // And only rows with autosave on ask for a fresh grid.
        assert_eq!(panel.autosave_track_ids(), vec![ids[0]]);
    }
}
