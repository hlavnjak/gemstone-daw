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
//! Track Composer — arrange the registered Tracks on a timeline and play them.
//!
//! Horizontal is time. Each row plays exactly one Track (picked in the select
//! box at its head), and several rows may share the same Track. Rows are added
//! by hand: a fresh instance starts with none.
//!
//! **Slots, not pixels.** A row is a list of fixed-width slots, each either a
//! note or a rest. "+ Add Note" appends after the last note; dragging a note
//! moves it between slots and refuses to land on an occupied one, so notes in a
//! row can never overlap and the only thing dragging can produce is silence.
//!
//! **Time is sequential.** A note's onset is where the previous slot ended: a
//! note lasts what its length box says, an empty slot lasts [`REST_BEATS`]. So
//! the length box shapes the rhythm and the gaps stretch it, at the tempo set on
//! the transport. Playback itself lives in [`player`].

pub mod player;

use eframe::egui;

use self::player::{CompositionPlayer, PlannedNote, RowPlan};
use super::registry::TrackRegistry;

/// Beats one empty slot is silent for.
const REST_BEATS: f64 = 1.0;

/// Note card size. Constant by design — a note's width says nothing about its
/// length here; the length box does.
const SLOT_W: f32 = 104.0;
const SLOT_H: f32 = 104.0;
/// Width of the fixed row head (track select, add-note, gain).
const HEAD_W: f32 = 292.0;
/// Empty slots drawn past the end of a row, so there is always somewhere to drag
/// a note to.
const TRAILING_SLOTS: usize = 4;

/// Lowest and highest note offered, C0..B8.
const PITCH_MIN: u8 = 12;
const PITCH_MAX: u8 = 119;
/// A new note starts at middle C.
const DEFAULT_PITCH: u8 = 60;

/// Note duration, as the fraction of a whole note it is named for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoteLength {
    Whole,
    Half,
    Quarter,
    Eighth,
    Sixteenth,
    ThirtySecond,
    SixtyFourth,
    HundredTwentyEighth,
}

impl NoteLength {
    const ALL: [NoteLength; 8] = [
        NoteLength::Whole,
        NoteLength::Half,
        NoteLength::Quarter,
        NoteLength::Eighth,
        NoteLength::Sixteenth,
        NoteLength::ThirtySecond,
        NoteLength::SixtyFourth,
        NoteLength::HundredTwentyEighth,
    ];

    /// Length in beats, a beat being a quarter note.
    fn beats(self) -> f64 {
        match self {
            NoteLength::Whole => 4.0,
            NoteLength::Half => 2.0,
            NoteLength::Quarter => 1.0,
            NoteLength::Eighth => 0.5,
            NoteLength::Sixteenth => 0.25,
            NoteLength::ThirtySecond => 0.125,
            NoteLength::SixtyFourth => 0.0625,
            NoteLength::HundredTwentyEighth => 0.03125,
        }
    }

    fn label(self) -> &'static str {
        match self {
            NoteLength::Whole => "whole",
            NoteLength::Half => "1/2",
            NoteLength::Quarter => "1/4",
            NoteLength::Eighth => "1/8",
            NoteLength::Sixteenth => "1/16",
            NoteLength::ThirtySecond => "1/32",
            NoteLength::SixtyFourth => "1/64",
            NoteLength::HundredTwentyEighth => "1/128",
        }
    }
}

/// Scientific pitch name of a MIDI note number (`60` → `C4`).
fn pitch_name(pitch: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[(pitch % 12) as usize],
        pitch as i32 / 12 - 1
    )
}

#[derive(Clone, Copy)]
struct Note {
    pitch: u8,
    length: NoteLength,
}

impl Default for Note {
    fn default() -> Self {
        Self {
            pitch: DEFAULT_PITCH,
            length: NoteLength::Quarter,
        }
    }
}

/// One lane: a Track and the notes played on it.
struct Row {
    /// Stable id, so egui widget state and the drag in progress survive rows
    /// being removed above them.
    id: u64,
    /// The Track this row plays, by registry id. `None` only while the registry
    /// is empty.
    track_id: Option<u64>,
    gain: f32,
    /// Fixed-width slots along the timeline; `None` is a rest.
    slots: Vec<Option<Note>>,
}

impl Row {
    fn new(id: u64, track_id: Option<u64>) -> Self {
        Self {
            id,
            track_id,
            gain: 1.0,
            slots: Vec::new(),
        }
    }

    /// Append a note right after the last one.
    fn add_note(&mut self) {
        self.slots.push(Some(Note::default()));
    }

    /// Move the note in `from` to slot `to`, growing the row if it lands past
    /// the end. Refuses to leave the row (`to < 0`) or to land on a slot that
    /// already holds a note — the rule that keeps notes in a row from
    /// overlapping.
    fn move_note(&mut self, from: usize, to: i64) -> bool {
        if to < 0 || self.slots.get(from).is_none_or(Option::is_none) {
            return false;
        }
        let to = to as usize;
        if to >= self.slots.len() {
            self.slots.resize(to + 1, None);
        } else if self.slots[to].is_some() {
            return false;
        }
        self.slots[to] = self.slots[from].take();
        self.trim();
        true
    }

    fn delete_note(&mut self, slot: usize) {
        if let Some(s) = self.slots.get_mut(slot) {
            *s = None;
        }
        self.trim();
    }

    /// Drop trailing rests: they are silence after the last note, which is not
    /// part of the composition, and keeping them would let a row grow forever.
    fn trim(&mut self) {
        while matches!(self.slots.last(), Some(None)) {
            self.slots.pop();
        }
    }

    /// The row's notes in seconds at `spb` seconds per beat, laid out
    /// sequentially: each slot starts where the previous one ended.
    fn planned_notes(&self, spb: f64) -> Vec<PlannedNote> {
        let mut at = 0.0f64;
        let mut notes = Vec::new();
        for slot in &self.slots {
            match slot {
                Some(note) => {
                    let dur = note.length.beats() * spb;
                    notes.push(PlannedNote {
                        at_secs: at,
                        dur_secs: dur,
                        pitch: note.pitch,
                    });
                    at += dur;
                }
                None => at += REST_BEATS * spb,
            }
        }
        notes
    }

    /// Where the row ends, in seconds.
    fn length_secs(&self, spb: f64) -> f64 {
        self.slots
            .iter()
            .map(|s| s.map_or(REST_BEATS, |n| n.length.beats()) * spb)
            .sum()
    }
}

/// A drag in progress, tracked across frames because a note moves in whole slots
/// while the pointer moves in pixels.
struct Drag {
    row_id: u64,
    slot: usize,
    /// Pointer travel not yet spent on a slot move.
    accum_x: f32,
}

/// The Composer panel.
pub struct ComposerPanel {
    registry: TrackRegistry,
    rows: Vec<Row>,
    next_row_id: u64,
    tempo_bpm: f32,
    status: String,
    player: Option<CompositionPlayer>,
    drag: Option<Drag>,
    /// Shared horizontal scroll of every lane, so the rows keep a common time
    /// axis instead of drifting apart.
    scroll_x: f32,
}

impl ComposerPanel {
    pub fn new(registry: TrackRegistry) -> Self {
        Self {
            registry,
            rows: Vec::new(),
            next_row_id: 0,
            tempo_bpm: 120.0,
            status: "Add a track row to start composing.".to_string(),
            player: None,
            drag: None,
            scroll_x: 0.0,
        }
    }

    fn secs_per_beat(&self) -> f64 {
        60.0 / (self.tempo_bpm.max(1.0) as f64)
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
    fn reconcile_tracks(&mut self) {
        let first = self.registry.first_id();
        for row in &mut self.rows {
            let valid = row.track_id.is_some_and(|id| self.registry.contains(id));
            if !valid {
                row.track_id = first;
            }
        }
    }

    fn start_playback(&mut self) {
        let spb = self.secs_per_beat();
        let mut plans = Vec::new();
        for row in &self.rows {
            let Some(source) = row.track_id.and_then(|id| self.registry.playback_source(id)) else {
                continue;
            };
            let notes = row.planned_notes(spb);
            if notes.is_empty() {
                continue;
            }
            plans.push(RowPlan {
                source,
                gain: row.gain,
                notes,
            });
        }
        if plans.is_empty() {
            self.status = "Nothing to play — add a note to a row first.".to_string();
            return;
        }
        match CompositionPlayer::start(plans) {
            Ok(player) => {
                self.status = if player.loaded_rows == player.total_rows {
                    format!("Playing {} row(s).", player.loaded_rows)
                } else {
                    format!(
                        "Playing {} of {} row(s) — the rest failed to load.",
                        player.loaded_rows, player.total_rows
                    )
                };
                self.player = Some(player);
            }
            Err(e) => self.status = format!("Playback failed: {e}"),
        }
    }

    fn stop_playback(&mut self) {
        if self.player.take().is_some() {
            self.status = "Stopped.".to_string();
        }
    }

    /// Total length of the composition, in seconds.
    fn length_secs(&self) -> f64 {
        let spb = self.secs_per_beat();
        self.rows
            .iter()
            .map(|r| r.length_secs(spb))
            .fold(0.0, f64::max)
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.reconcile_tracks();
        // A finished composition stops itself, so the transport does not sit on
        // "playing" over silence.
        if self.player.as_ref().is_some_and(CompositionPlayer::is_finished) {
            self.player = None;
            self.status = "Finished.".to_string();
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

        if !self.rows.is_empty() {
            ui.add_space(6.0);
            self.rows_ui(ui, &tracks);
        }

        ui.add_space(8.0);
        ui.separator();
        self.transport_ui(ui);
    }

    /// The lanes: a fixed head per row, then a scrolling timeline. All lanes
    /// share one scroll offset, so a note in one row lines up with the note
    /// under it in the next.
    fn rows_ui(&mut self, ui: &mut egui::Ui, tracks: &[(u64, String)]) {
        enum Act {
            Remove(usize),
        }
        let mut act: Option<Act> = None;
        let mut offset = self.scroll_x;
        let dragging_id = self.drag.as_ref().map(|d| (d.row_id, d.slot));

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
                                egui::vec2(HEAD_W, SLOT_H),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.horizontal(|ui| {
                                        let label = row
                                            .track_id
                                            .and_then(|id| {
                                                tracks
                                                    .iter()
                                                    .find(|(t, _)| *t == id)
                                                    .map(|(_, n)| n.clone())
                                            })
                                            .unwrap_or_else(|| "— no track —".to_string());
                                        egui::ComboBox::from_id_salt("track")
                                            .width(168.0)
                                            .selected_text(label)
                                            .show_ui(ui, |ui| {
                                                if tracks.is_empty() {
                                                    ui.label("— no track —");
                                                }
                                                for (id, name) in tracks {
                                                    ui.selectable_value(
                                                        &mut row.track_id,
                                                        Some(*id),
                                                        name,
                                                    );
                                                }
                                            });
                                        if ui
                                            .button("🗑")
                                            .on_hover_text("Remove this row")
                                            .clicked()
                                        {
                                            act = Some(Act::Remove(idx));
                                        }
                                    });
                                    if ui
                                        .button("➕ Add Note")
                                        .on_hover_text("Append a note after the last one in this row")
                                        .clicked()
                                    {
                                        row.add_note();
                                    }
                                    ui.horizontal(|ui| {
                                        ui.label("Gain");
                                        ui.add(
                                            egui::Slider::new(&mut row.gain, 0.0..=2.0)
                                                .fixed_decimals(2)
                                                .show_value(true),
                                        );
                                    });
                                },
                            );

                            // ── Timeline lane ─────────────────────────────
                            let out = egui::ScrollArea::horizontal()
                                .id_salt("lane")
                                .horizontal_scroll_offset(self.scroll_x)
                                .max_height(SLOT_H + 8.0)
                                .show(ui, |ui| {
                                    Self::lane_ui(ui, row, &mut self.drag, dragging_id);
                                });
                            // Whichever lane the user actually scrolled wins,
                            // and the rest follow it next frame.
                            if (out.state.offset.x - self.scroll_x).abs() > 0.5 {
                                offset = out.state.offset.x;
                            }
                        });
                    });
            });
            ui.add_space(4.0);
        }

        self.scroll_x = offset;

        if let Some(Act::Remove(idx)) = act {
            if idx < self.rows.len() {
                let removed = self.rows.remove(idx);
                if self.drag.as_ref().is_some_and(|d| d.row_id == removed.id) {
                    self.drag = None;
                }
                self.status = "Removed a row.".to_string();
            }
        }
    }

    /// One row's slots. Notes are cards of a constant size; empty slots are drawn
    /// as faint drop targets so the silence in a row is visible.
    fn lane_ui(
        ui: &mut egui::Ui,
        row: &mut Row,
        drag: &mut Option<Drag>,
        dragging: Option<(u64, usize)>,
    ) {
        let count = row.slots.len() + TRAILING_SLOTS;
        let (lane, _) =
            ui.allocate_exact_size(egui::vec2(count as f32 * SLOT_W, SLOT_H), egui::Sense::hover());
        let painter = ui.painter();

        for i in 0..count {
            let cell = egui::Rect::from_min_size(
                egui::pos2(lane.left() + i as f32 * SLOT_W, lane.top()),
                egui::vec2(SLOT_W - 4.0, SLOT_H),
            );
            let has_note = row.slots.get(i).is_some_and(Option::is_some);
            if !has_note {
                painter.rect_stroke(
                    cell,
                    4.0,
                    egui::Stroke::new(1.0, egui::Color32::from_gray(52)),
                    egui::StrokeKind::Inside,
                );
                continue;
            }
            let held = dragging == Some((row.id, i));
            painter.rect_filled(
                cell,
                4.0,
                if held {
                    egui::Color32::from_rgb(72, 96, 132)
                } else {
                    egui::Color32::from_rgb(52, 66, 92)
                },
            );
            painter.rect_stroke(
                cell,
                4.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(110, 150, 200)),
                egui::StrokeKind::Inside,
            );
        }

        // Widgets on top of the painted cards. Deferred, because moving a note
        // rewrites the very slots this loop walks.
        let mut pending_move: Option<(usize, i64)> = None;
        let mut pending_delete: Option<usize> = None;

        for i in 0..row.slots.len() {
            if row.slots[i].is_none() {
                continue;
            }
            let cell = egui::Rect::from_min_size(
                egui::pos2(lane.left() + i as f32 * SLOT_W, lane.top()),
                egui::vec2(SLOT_W - 4.0, SLOT_H),
            );

            // Drag handle: the card's top strip. The combo boxes below own their
            // own clicks, so the grip has to be somewhere they are not.
            let grip = egui::Rect::from_min_size(cell.min, egui::vec2(cell.width() - 22.0, 20.0));
            let grip_resp = ui.interact(
                grip,
                ui.id().with(("grip", row.id, i)),
                egui::Sense::click_and_drag(),
            );
            ui.painter().text(
                grip.center(),
                egui::Align2::CENTER_CENTER,
                "⣿ drag",
                egui::TextStyle::Small.resolve(ui.style()),
                egui::Color32::from_gray(190),
            );
            if grip_resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            if grip_resp.drag_started() {
                *drag = Some(Drag {
                    row_id: row.id,
                    slot: i,
                    accum_x: 0.0,
                });
            }
            if grip_resp.dragged() {
                if let Some(d) = drag.as_mut().filter(|d| d.row_id == row.id && d.slot == i) {
                    d.accum_x += grip_resp.drag_delta().x;
                    // One slot per SLOT_W of travel; a refused move (occupied
                    // slot, or the start of the row) spends the travel anyway so
                    // the note does not leap once the way clears.
                    if d.accum_x.abs() >= SLOT_W {
                        let step = d.accum_x.signum() as i64;
                        pending_move = Some((i, i as i64 + step));
                        d.accum_x -= step as f32 * SLOT_W;
                    }
                }
            }
            if grip_resp.drag_stopped() {
                *drag = None;
            }

            let close = egui::Rect::from_min_size(
                egui::pos2(cell.right() - 20.0, cell.top() + 1.0),
                egui::vec2(18.0, 18.0),
            );
            if ui
                .put(close, egui::Button::new("✖").small().frame(false))
                .on_hover_text("Delete this note")
                .clicked()
            {
                pending_delete = Some(i);
            }

            let note = row.slots[i].as_mut().expect("checked above");
            let body = egui::Rect::from_min_size(
                egui::pos2(cell.left() + 6.0, cell.top() + 24.0),
                egui::vec2(cell.width() - 12.0, cell.height() - 30.0),
            );
            let mut body_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(body)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            body_ui.spacing_mut().item_spacing.y = 4.0;
            egui::ComboBox::from_id_salt(("pitch", row.id, i))
                .width(body.width())
                .height(260.0)
                .selected_text(pitch_name(note.pitch))
                .show_ui(&mut body_ui, |ui| {
                    for p in PITCH_MIN..=PITCH_MAX {
                        ui.selectable_value(&mut note.pitch, p, pitch_name(p));
                    }
                });
            egui::ComboBox::from_id_salt(("len", row.id, i))
                .width(body.width())
                .selected_text(note.length.label())
                .show_ui(&mut body_ui, |ui| {
                    for l in NoteLength::ALL {
                        ui.selectable_value(&mut note.length, l, l.label());
                    }
                });
        }

        if let Some((from, to)) = pending_move {
            if row.move_note(from, to) {
                if let Some(d) = drag.as_mut().filter(|d| d.row_id == row.id) {
                    d.slot = to.max(0) as usize;
                }
            }
        }
        if let Some(slot) = pending_delete {
            row.delete_note(slot);
            *drag = None;
        }
    }

    /// Play / stop, tempo, and where the transport currently is.
    fn transport_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let playing = self.player.is_some();
            if ui
                .add_enabled(!playing, egui::Button::new("▶ Play"))
                .on_hover_text("Play every row from the start")
                .clicked()
            {
                self.start_playback();
            }
            if ui
                .add_enabled(playing, egui::Button::new("■ Stop"))
                .clicked()
            {
                self.stop_playback();
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
            ui.add_space(12.0);
            match &self.player {
                Some(p) => ui.label(
                    egui::RichText::new(format!(
                        "▶ {:.1} s / {:.1} s",
                        p.position_secs(),
                        p.total_secs
                    ))
                    .color(egui::Color32::from_rgb(130, 210, 150)),
                ),
                None => ui.label(
                    egui::RichText::new(format!("{:.1} s", self.length_secs()))
                        .color(egui::Color32::from_gray(160)),
                ),
            };
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new(&self.status).color(egui::Color32::from_gray(170)));

        // While playing, keep the position readout moving and notice the end of
        // the composition promptly; otherwise stay idle like the rest of the app.
        if self.player.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_with(slots: Vec<Option<Note>>) -> Row {
        Row {
            id: 0,
            track_id: None,
            gain: 1.0,
            slots,
        }
    }

    fn note(pitch: u8, length: NoteLength) -> Option<Note> {
        Some(Note { pitch, length })
    }

    /// "Right after the last note", including when the row is empty.
    #[test]
    fn add_note_appends_after_the_last_note() {
        let mut row = row_with(Vec::new());
        row.add_note();
        row.add_note();
        assert_eq!(row.slots.len(), 2);
        assert!(row.slots.iter().all(Option::is_some));
    }

    /// The whole point of dragging: a gap in the row, and no way to stack two
    /// notes on the same slot.
    #[test]
    fn a_note_moves_into_free_slots_only() {
        let mut row = row_with(vec![note(60, NoteLength::Quarter), note(62, NoteLength::Quarter)]);

        // Right, into empty space past the end: the row grows and slot 1 is now
        // a rest.
        assert!(row.move_note(1, 2));
        assert_eq!(row.slots.len(), 3);
        assert!(row.slots[1].is_none());
        assert_eq!(row.slots[2].unwrap().pitch, 62);

        // Onto the note still sitting in slot 0: refused, nothing moves.
        assert!(!row.move_note(2, 0));
        assert_eq!(row.slots[2].unwrap().pitch, 62);
        assert_eq!(row.slots[0].unwrap().pitch, 60);

        // Off the left edge: refused.
        assert!(!row.move_note(0, -1));
        assert_eq!(row.slots[0].unwrap().pitch, 60);
    }

    /// Trailing rests are silence after the music stops — dropping them keeps a
    /// dragged-and-returned note from leaving the row longer every time.
    #[test]
    fn trailing_rests_are_trimmed() {
        let mut row = row_with(vec![note(60, NoteLength::Quarter), note(62, NoteLength::Quarter)]);
        assert!(row.move_note(1, 4));
        assert_eq!(row.slots.len(), 5);
        assert!(row.move_note(4, 1));
        assert_eq!(row.slots.len(), 2, "the empty tail should be gone");
    }

    /// Sequential timing: each note starts where the previous slot ended, and a
    /// rest is worth one beat.
    #[test]
    fn notes_are_scheduled_back_to_back_with_rests_between() {
        let row = row_with(vec![
            note(60, NoteLength::Quarter), // 1 beat
            note(62, NoteLength::Half),    // 2 beats
            None,                          // 1 beat of silence
            note(64, NoteLength::Eighth),  // 0.5 beats
        ]);
        // 120 BPM: one beat is half a second.
        let spb = 0.5;
        let notes = row.planned_notes(spb);
        let times: Vec<(f64, f64, u8)> = notes
            .iter()
            .map(|n| (n.at_secs, n.dur_secs, n.pitch))
            .collect();
        assert_eq!(
            times,
            vec![(0.0, 0.5, 60), (0.5, 1.0, 62), (2.0, 0.25, 64)]
        );
        assert_eq!(row.length_secs(spb), 2.25);
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
        let mut panel = ComposerPanel::new(registry.clone());
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
    /// that have layout of their own: no rows, a row with notes and a gap, and a
    /// row whose track list has gone empty. Catches the panics a layout test can
    /// catch — duplicate widget ids, bad rects — which no amount of model testing
    /// would.
    #[test]
    fn the_panel_lays_out_in_every_state_without_panicking() {
        let (registry, ids) = registry_with(&["one"]);
        let mut panel = ComposerPanel::new(registry.clone());
        let ctx = egui::Context::default();
        let frame = |panel: &mut ComposerPanel| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| panel.ui(ui));
            });
        };

        frame(&mut panel); // no rows: just the add button and the transport

        panel.add_row();
        panel.rows[0].add_note();
        panel.rows[0].add_note();
        assert!(panel.rows[0].move_note(1, 3)); // a gap between the two notes
        frame(&mut panel);

        // Two rows on the same track, which the spec allows.
        panel.add_row();
        panel.rows[1].add_note();
        frame(&mut panel);

        registry.remove(ids[0]);
        frame(&mut panel);
        assert!(panel.rows.iter().all(|r| r.track_id.is_none()));
    }

    #[test]
    fn pitch_names_follow_scientific_notation() {
        assert_eq!(pitch_name(60), "C4");
        assert_eq!(pitch_name(61), "C#4");
        assert_eq!(pitch_name(PITCH_MIN), "C0");
        assert_eq!(pitch_name(PITCH_MAX), "B8");
    }

    #[test]
    fn note_lengths_halve_from_a_whole_note() {
        assert_eq!(NoteLength::Whole.beats(), 4.0);
        for pair in NoteLength::ALL.windows(2) {
            assert_eq!(pair[1].beats(), pair[0].beats() / 2.0);
        }
        assert_eq!(NoteLength::HundredTwentyEighth.beats(), 4.0 / 128.0);
    }
}
