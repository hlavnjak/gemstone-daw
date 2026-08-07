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
//! Track Composer — arrange the registered Tracks in rows of frames and play them.
//!
//! **A row is a sequence, not a canvas.** Each row plays exactly one Track
//! (picked in the select box at its head), and several rows may share the same
//! Track. A row holds a chain of frames laid left to right, and each frame is
//! simply played after the one before it: nothing is positioned, nothing is
//! dragged, and nothing can overlap. Every row starts at time zero, so rows sound
//! together exactly to the extent that the lengths before a frame add up the
//! same — that is what makes a chord.
//!
//! **Two kinds of frame.** A *note* frame carries a pitch and a length. A *space*
//! frame (drawn in its own colour) carries only a length: it is silence. Pressing
//! "➕ Add Note" appends both — the note, then a space right behind it — because
//! a note followed by nothing but the next note is rarely what is wanted, and the
//! space is where the silence between them is edited.
//!
//! **Length is two select boxes**, not one: a whole-note count and a fraction
//! down to a 1/256, added together. Time is counted in [`UNITS_PER_WHOLE`]ths of
//! a whole note, so every length either box can name is a whole number of units
//! and the arithmetic stays exact.
//!
//! Playback (and the transport that highlights the sounding frame) lives in
//! [`player`].

pub mod player;

use eframe::egui;

use self::player::{CompositionPlayer, PlannedNote, RowPlan};
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

/// One frame in a row: either a note or the silence after one.
#[derive(Clone, Copy)]
struct Item {
    /// Stable within its row, so the egui widget ids survive frames being
    /// deleted around it.
    id: u64,
    /// The pitch a note frame sounds; `None` marks a space frame, which has a
    /// length but nothing to play.
    pitch: Option<u8>,
    dur: Duration,
}

impl Item {
    fn is_space(&self) -> bool {
        self.pitch.is_none()
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
    /// In play order: frame `n` starts where frame `n - 1` ended.
    items: Vec<Item>,
    next_item_id: u64,
}

impl Row {
    fn new(id: u64, track_id: Option<u64>) -> Self {
        Self {
            id,
            track_id,
            gain: 1.0,
            items: Vec::new(),
            next_item_id: 0,
        }
    }

    /// Total length of the row in units — the frames simply add up.
    fn end_units(&self) -> i64 {
        self.items.iter().map(|i| i.dur.units()).sum()
    }

    fn push(&mut self, pitch: Option<u8>, dur: Duration) {
        let id = self.next_item_id;
        self.next_item_id += 1;
        self.items.push(Item { id, pitch, dur });
    }

    /// Append a note and, right behind it, the space that separates it from
    /// whatever comes next.
    fn add_note(&mut self) {
        self.push(Some(DEFAULT_PITCH), Duration::new(0, Fraction::Quarter));
        self.push(None, Duration::new(0, Fraction::Eighth));
    }

    fn delete_item(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.items.remove(idx);
        }
    }

    /// Where each frame starts, in units — the running sum of the lengths before
    /// it. One entry per frame.
    fn starts(&self) -> Vec<i64> {
        let mut at = 0;
        self.items
            .iter()
            .map(|i| {
                let start = at;
                at += i.dur.units();
                start
            })
            .collect()
    }

    /// The row's notes in seconds, at `spu` seconds per grid unit. Spaces only
    /// advance the clock, and a note given no length at all is not played.
    fn planned_notes(&self, spu: f64) -> Vec<PlannedNote> {
        let mut at = 0i64;
        let mut out = Vec::new();
        for item in &self.items {
            let units = item.dur.units();
            if let Some(pitch) = item.pitch {
                if units > 0 {
                    out.push(PlannedNote {
                        at_secs: at as f64 * spu,
                        dur_secs: units as f64 * spu,
                        pitch,
                    });
                }
            }
            at += units;
        }
        out
    }
}

/// The Composer panel.
pub struct ComposerPanel {
    registry: TrackRegistry,
    rows: Vec<Row>,
    next_row_id: u64,
    tempo_bpm: f32,
    status: String,
    player: Option<CompositionPlayer>,
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
    fn reconcile_tracks(&mut self) {
        let first = self.registry.first_id();
        for row in &mut self.rows {
            let valid = row.track_id.is_some_and(|id| self.registry.contains(id));
            if !valid {
                row.track_id = first;
            }
        }
    }

    /// End of the composition in units — the longest row.
    fn end_units(&self) -> i64 {
        self.rows.iter().map(Row::end_units).max().unwrap_or(0)
    }

    fn start_playback(&mut self) {
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
            self.lanes_ui(ui, &tracks);
        }

        ui.add_space(8.0);
        ui.separator();
        self.transport_ui(ui);
    }

    /// One strip per row: the head on the left, the chain of frames scrolling on
    /// the right.
    fn lanes_ui(&mut self, ui: &mut egui::Ui, tracks: &[(u64, String)]) {
        let mut remove_row: Option<usize> = None;
        let playhead = self.playhead_units();

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
                                            remove_row = Some(idx);
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        if ui
                                            .button("➕ Add Note")
                                            .on_hover_text(
                                                "Append a note frame and, behind it, a \
                                                 space frame for the silence that follows",
                                            )
                                            .clicked()
                                        {
                                            row.add_note();
                                        }
                                        ui.label("Gain");
                                        ui.spacing_mut().slider_width = 96.0;
                                        ui.add(
                                            egui::Slider::new(&mut row.gain, 0.0..=2.0)
                                                .fixed_decimals(2)
                                                .show_value(true),
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
                                    Self::chain_ui(ui, row, playhead);
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

    /// A row's frames, left to right in play order. Editing a frame's length or
    /// pitch is immediate; everything after it simply shifts, because a frame's
    /// position is nothing but the sum of the lengths before it.
    fn chain_ui(ui: &mut egui::Ui, row: &mut Row, playhead: Option<f64>) {
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
            for (idx, (item, start)) in row.items.iter_mut().zip(starts).enumerate() {
                let sounding = playhead.is_some_and(|p| {
                    !item.is_space()
                        && p >= start as f64
                        && p < (start + item.dur.units()) as f64
                });
                if Self::frame_ui(ui, row_id, item, sounding) {
                    pending_delete = Some(idx);
                }
            }
        });

        if let Some(idx) = pending_delete {
            row.delete_item(idx);
        }
    }

    /// One frame. Returns `true` when its delete button was pressed.
    ///
    /// A note frame is blue and carries three select boxes — pitch, whole part
    /// of the length, fractional part. A space frame is amber and carries only
    /// the two length boxes: it has no pitch to choose.
    fn frame_ui(ui: &mut egui::Ui, row_id: u64, item: &mut Item, sounding: bool) -> bool {
        let space = item.is_space();
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
        let id = item.id;
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

                        // Header: what the frame is and how long, plus its
                        // delete button. Laid out from the right so the button
                        // keeps its corner and a long title truncates instead of
                        // pushing the card wider than its neighbours.
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(egui::Button::new("✖").small().frame(false))
                                        .on_hover_text(if space {
                                            "Delete this space"
                                        } else {
                                            "Delete this note"
                                        })
                                        .clicked()
                                    {
                                        deleted = true;
                                    }
                                    let title = match item.pitch {
                                        Some(p) => {
                                            format!("{} · {}", pitch_name(p), item.dur.label())
                                        }
                                        None => format!("space · {}", item.dur.label()),
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

                        if let Some(pitch) = item.pitch.as_mut() {
                            egui::ComboBox::from_id_salt(("pitch", row_id, id))
                                .width(inner_w)
                                .height(260.0)
                                .selected_text(pitch_name(*pitch))
                                .show_ui(ui, |ui| {
                                    for p in PITCH_MIN..=PITCH_MAX {
                                        ui.selectable_value(pitch, p, pitch_name(p));
                                    }
                                })
                                .response
                                .on_hover_text("Pitch");
                        }

                        egui::ComboBox::from_id_salt(("wholes", row_id, id))
                            .width(inner_w)
                            .height(260.0)
                            .selected_text(format!("{} whole", item.dur.wholes))
                            .show_ui(ui, |ui| {
                                for w in 0..=MAX_WHOLES {
                                    ui.selectable_value(
                                        &mut item.dur.wholes,
                                        w,
                                        format!("{w} whole"),
                                    );
                                }
                            })
                            .response
                            .on_hover_text("Whole notes — the whole part of the length");

                        egui::ComboBox::from_id_salt(("frac", row_id, id))
                            .width(inner_w)
                            .selected_text(item.dur.frac.label())
                            .show_ui(ui, |ui| {
                                for f in Fraction::ALL {
                                    ui.selectable_value(&mut item.dur.frac, f, f.label());
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
            let length_secs = self.end_units() as f64 * self.secs_per_unit();
            match &self.player {
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
        // on time; otherwise stay idle like the rest of the app.
        if self.player.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

/// Colour of the transport's position readout, and of the frame sounding under
/// it.
const PLAYHEAD: egui::Color32 = egui::Color32::from_rgb(240, 120, 110);

#[cfg(test)]
mod tests {
    use super::*;

    const QUARTER: i64 = UNITS_PER_WHOLE / 4;
    const EIGHTH: i64 = UNITS_PER_WHOLE / 8;

    fn row_with(items: &[(Option<u8>, u8, Fraction)]) -> Row {
        let mut row = Row::new(0, None);
        for (pitch, wholes, frac) in items {
            row.push(*pitch, Duration::new(*wholes, *frac));
        }
        row
    }

    /// The requirement: one press of "Add Note" leaves a note *and* the space
    /// behind it, in that order, and the space has no pitch to edit.
    #[test]
    fn adding_a_note_appends_the_note_and_a_space_behind_it() {
        let mut row = row_with(&[]);
        row.add_note();
        assert_eq!(row.items.len(), 2);
        assert_eq!(row.items[0].pitch, Some(DEFAULT_PITCH));
        assert!(!row.items[0].is_space());
        assert!(row.items[1].is_space());
        assert_eq!(row.items[1].pitch, None);

        // And again: the chain grows note, space, note, space.
        row.add_note();
        let kinds: Vec<bool> = row.items.iter().map(Item::is_space).collect();
        assert_eq!(kinds, vec![false, true, false, true]);
        // Ids stay unique, so two frames never share a widget id.
        let mut ids: Vec<u64> = row.items.iter().map(|i| i.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), row.items.len());
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

    /// Frames are played one after another: a frame starts where the previous
    /// one ended, and a space is silence of exactly its own length.
    #[test]
    fn frames_play_in_sequence_and_spaces_are_the_silences() {
        let spu = 1.0; // one second per unit keeps the arithmetic readable
        let row = row_with(&[
            (Some(60), 0, Fraction::Quarter),
            (None, 0, Fraction::Eighth),
            (Some(64), 0, Fraction::Quarter),
        ]);
        let notes = row.planned_notes(spu);
        assert_eq!(notes.len(), 2); // the space is not played
        assert_eq!(notes[0].at_secs, 0.0);
        assert_eq!(notes[0].dur_secs, QUARTER as f64);
        // The second note waits out the quarter *and* the eighth of silence.
        assert_eq!(notes[1].at_secs, (QUARTER + EIGHTH) as f64);
        assert_eq!(notes[1].pitch, 64);
        assert_eq!(row.end_units(), QUARTER + EIGHTH + QUARTER);

        // A note left at length zero is silence, not a click.
        let row = row_with(&[(Some(60), 0, Fraction::None), (Some(62), 0, Fraction::Half)]);
        let notes = row.planned_notes(spu);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, 62);
        assert_eq!(notes[0].at_secs, 0.0);
    }

    /// Every row starts at zero, so rows sound together exactly when the lengths
    /// before a note add up the same — that is the only thing that makes a chord
    /// now that nothing is positioned by hand.
    #[test]
    fn equal_lengths_before_a_note_make_it_sound_with_another_row() {
        let spu = 0.5 / UNITS_PER_BEAT as f64; // 120 BPM

        // Two eighths played, then the note.
        let dense = row_with(&[
            (Some(60), 0, Fraction::Eighth),
            (Some(62), 0, Fraction::Eighth),
            (Some(64), 1, Fraction::None),
        ]);
        // A quarter of silence, then the note — the same moment, reached by a
        // different route.
        let sparse = row_with(&[(None, 0, Fraction::Quarter), (Some(67), 1, Fraction::None)]);

        let third = dense.planned_notes(spu)[2].at_secs;
        // The space is not a played note, so the row's only note is at index 0.
        let second = sparse.planned_notes(spu)[0].at_secs;
        assert_eq!(third, second);
        assert_eq!(third, QUARTER as f64 * spu);
        // …and a row whose lengths do not add up the same does not join them.
        let off = row_with(&[
            (None, 0, Fraction::Eighth),
            (Some(67), 1, Fraction::None),
        ]);
        assert_ne!(off.planned_notes(spu)[0].at_secs, third);
    }

    /// Deleting a frame closes the gap: everything behind it moves earlier by
    /// exactly that frame's length.
    #[test]
    fn deleting_a_frame_pulls_the_rest_forward() {
        let mut row = row_with(&[
            (Some(60), 0, Fraction::Quarter),
            (None, 0, Fraction::Half),
            (Some(64), 0, Fraction::Quarter),
        ]);
        assert_eq!(row.starts(), vec![0, QUARTER, QUARTER + UNITS_PER_WHOLE / 2]);
        row.delete_item(1); // drop the silence
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
    /// that have layout of their own: no rows, an empty row, a row of note and
    /// space frames, two rows, and a row whose track list has gone empty.
    /// Catches the panics a layout test can catch — duplicate widget ids, bad
    /// rects — which no amount of model testing would.
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
        frame(&mut panel); // a row with no frames yet

        panel.rows[0].add_note();
        panel.rows[0].add_note();
        frame(&mut panel);

        // Two rows on the same track, which the spec allows.
        panel.add_row();
        panel.rows[1].add_note();
        panel.rows[1].items[0].dur = Duration::new(MAX_WHOLES, Fraction::TwoHundredFiftySixth);
        frame(&mut panel);

        // A deleted frame must not leave a stale widget id behind.
        panel.rows[0].delete_item(0);
        frame(&mut panel);

        registry.remove(ids[0]);
        frame(&mut panel);
        assert!(panel.rows.iter().all(|r| r.track_id.is_none()));
    }
}
