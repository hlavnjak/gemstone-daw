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
//! Real-time playback of a composition: one output stream, one plugin instance
//! per row, mixed with per-row gain.
//!
//! **One stream, many plugins.** An editor drives its own instance from its own
//! `cpal` stream; a composition needs several instruments summed inside one
//! callback on one sample clock, hence this second audio path.
//!
//! **The Composer loads its own instances** from each track's recipe
//! ([`PlaybackSource`]) rather than borrowing the editor's: a VST3 processor is
//! not re-entrant, so one already pulled by an editor's stream cannot also be
//! pulled by this one. The grid is snapshotted from the live editor when there
//! is one, so what plays is what the user is editing.
//!
//! **One instance serves as many rows as it can** ([`share_groups`]). An
//! instrument is polyphonic — the rows of a chord are one instrument playing
//! three notes, not three instruments — and an instance is not cheap: a LeSynth
//! grid and its rendered key buffers are tens of megabytes, and a sampler's
//! whole kit is more. A row therefore only gets an instance of its own when it
//! *needs* one: a different sound, a different level, or notes that would
//! collide with another row's on the same instance.
//!
//! The schedule is resolved to sample times up front, so the callback only walks
//! a sorted per-row cursor — no allocation or locking of its own.
//!
//! **A repeat picks up edits.** While it loops, the panel hands the transport
//! the composition again whenever it changes ([`CompositionPlayer::update_live`]);
//! the callback swaps it in at the loop point, so a pass is never rearranged
//! under itself. Only the schedules and gains change that way — which plugin
//! each row plays cannot, because loading one is not something an audio callback
//! can do.
//!
//! **Repeat** loops on the composition's *written* length — the longest row,
//! trailing silence included — not on the last note plus its release. The wrap is
//! cut at the exact sample, inside the device block if need be, so a loop does
//! not drift by up to a buffer every pass.
//!
//! **Export renders through the same voices** ([`render_offline`]): the loading,
//! the schedule and the per-block mix are shared with playback, so a `.wav` is
//! what the transport plays rather than a second implementation of it that can
//! drift. It runs as fast as the plugins allow — a LeSynth key renders
//! synchronously when its buffer is not ready, so no note can come out silent
//! for being asked for early.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use vst3::Steinberg::Vst::{
    AudioBusBuffers, IAudioProcessorTrait, IEventList,
    ProcessData, SymbolicSampleSizes_,
};
use vst3::{ComPtr, ComWrapper};

use crate::audio::midi_to_vst3_event;
use crate::gui::registry::PlaybackSource;
use crate::audio::engine::{bus_buffers, declared_block_size, AudioScratch};
use crate::vst::{next_instance_token, EventList, PluginInstance, Vst3Module};

/// Velocity every composed note is played at. The Composer has no velocity
/// control, and a mid-scale value keeps VSTs that map velocity to level audible
/// without slamming them.
const NOTE_VELOCITY: u8 = 100;

/// Extra time played after the last note-off so releases and (for LeSynth in
/// Analysis mode) the tail of a long note are not cut off by the transport
/// stopping exactly on the final event.
const TAIL_SECS: f64 = 1.5;

/// One note on the timeline, already resolved to seconds.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PlannedNote {
    pub at_secs: f64,
    pub dur_secs: f64,
    pub pitch: u8,
}

impl PlannedNote {
    /// Whether this note is sounding at the same time as `other` **on the same
    /// pitch** — the one thing two rows cannot do on one instance, because a
    /// note-off names only a pitch and would cut the other row's note.
    fn collides_with(&self, other: &Self) -> bool {
        self.pitch == other.pitch
            && self.at_secs < other.at_secs + other.dur_secs
            && other.at_secs < self.at_secs + self.dur_secs
    }
}

/// One composer row, ready to play.
pub struct RowPlan {
    /// The panel's row id, which is how a live edit finds the voice again.
    pub row_id: u64,
    pub source: PlaybackSource,
    pub gain: f32,
    pub notes: Vec<PlannedNote>,
}

/// One row of an edit made while the transport is running: everything about a
/// row that can be changed without loading a different plugin.
#[derive(Clone, PartialEq, Debug)]
pub struct RowEdit {
    pub row_id: u64,
    pub gain: f32,
    pub notes: Vec<PlannedNote>,
}

/// A composition edited mid-flight, waiting for the next loop point.
struct LiveUpdate {
    rows: Vec<VoiceEdit>,
    loop_sample: u64,
    /// Set by the audio callback once it has taken this. What is left in `rows`
    /// afterwards is the *old* schedules, swapped out rather than dropped —
    /// freeing them is the GUI thread's job, not the audio thread's.
    applied: bool,
}

struct VoiceEdit {
    /// Which voice this is for, by position. The voice list is fixed for the
    /// life of a player — loading an instance is not something an audio callback
    /// can do — so an index is a stable name for one, and a voice serving
    /// several rows has no single row id to be found by.
    voice: usize,
    gain: f32,
    schedule: Vec<(u64, u8, bool)>,
}

/// A row's live instance plus its event schedule, as the callback sees it.
struct Voice {
    /// The panel's row ids: which rows of the composition this one instance is
    /// playing. Usually one; several when they were found to be shareable (see
    /// [`share_groups`]).
    row_ids: Vec<u64>,
    /// The instance this voice plays. Held here, not just in the player, so the
    /// plugin cannot be terminated or its library unloaded while the callback
    /// that calls `process()` on it still exists.
    plugin: Arc<PluginInstance>,
    event_impl: Arc<EventList>,
    event_list: ComPtr<IEventList>,
    gain: f32,
    /// `(sample time, pitch, note-on)`, sorted by time.
    schedule: Vec<(u64, u8, bool)>,
    cursor: usize,
    /// Total channels across this plugin's audio input buses, which is where its
    /// output channels start in `scratch`.
    in_channels: usize,
    /// Channels on the main output bus — the ones that are mixed down.
    main_out: usize,
    /// The block size this instance was set up for; it must never be handed more.
    max_block: usize,
    /// Per-channel buffers (inputs then outputs) and the pointer table
    /// `process()` reads them through, owned so the mix allocates nothing per
    /// block and one voice's layout cannot disturb another's.
    scratch: AudioScratch,
    /// The bus descriptors, laid out over `scratch` once.
    in_buses: Vec<AudioBusBuffers>,
    out_buses: Vec<AudioBusBuffers>,
}

/// A running composition. Dropping it stops playback: the stream field is
/// declared first, so it is torn down before the plugin instances (and the
/// libraries they live in) it points at.
pub struct CompositionPlayer {
    stream: Option<cpal::Stream>,
    /// Keeps every loaded instance (and its library) alive for the stream's life.
    _plugins: Vec<Arc<PluginInstance>>,
    position: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
    /// Where a repeat wraps, in samples. Owned by the callback (it is the one
    /// that adopts a live edit's new length) and read here for the transport
    /// readout.
    loop_sample: Arc<AtomicU64>,
    /// An edit waiting for the next loop point. The callback only ever
    /// `try_lock`s it, so a busy GUI thread cannot stall the audio thread.
    live: Arc<Mutex<Option<LiveUpdate>>>,
    /// When the first block was produced. Set by the callback rather than by
    /// `start_prepared`, because `stream.play()` returning is not the same
    /// moment as the device asking for audio — and a recording is lined up
    /// against this.
    started: Arc<OnceLock<Instant>>,
    /// How far the callback runs ahead of the ear, in microseconds: the device's
    /// own output latency, as it reports it.
    latency_us: Arc<AtomicU64>,
    sample_rate: f64,
    /// Which rows each voice is playing, by position — the grouping
    /// [`share_groups`] settled on, which a live edit has to merge along.
    voice_rows: Vec<Vec<u64>>,
    /// Set when a live edit gave two rows sharing an instance different gains.
    /// One instance has one output and so one level, so the panel has to say
    /// that the transport cannot do what the sliders now ask for.
    gains_diverged: Arc<AtomicBool>,
    /// Rows that actually loaded, and rows asked for.
    pub loaded_rows: usize,
    /// Plugin instances those rows are sharing.
    pub instances: usize,
    pub total_rows: usize,
    /// End of the composition, tail included.
    pub total_secs: f64,
}

/// A composition with its plugins loaded and its schedules resolved, waiting for
/// a stream. Built by [`CompositionPlayer::prepare`], off the GUI thread.
pub struct PreparedComposition {
    voices: Vec<Voice>,
    plugins: Vec<Arc<PluginInstance>>,
    end_sample: u64,
    sample_rate: f64,
    channels: usize,
    stream_cfg: cpal::StreamConfig,
    total_rows: usize,
    /// Rows that ended up with an instance behind them — not the same as the
    /// number of instances, since one can serve several rows.
    rows_playing: usize,
}

impl PreparedComposition {
    /// Rows that loaded, and rows asked for.
    pub fn loaded_rows(&self) -> (usize, usize) {
        (self.rows_playing, self.total_rows)
    }

    /// How many plugin instances those rows are sharing.
    pub fn instances(&self) -> usize {
        self.plugins.len()
    }
}

impl CompositionPlayer {
    /// Load an instance per row and start the output stream.
    ///
    /// A row whose plugin fails to load is logged and skipped rather than
    /// aborting the transport — the rest of the composition still plays, and the
    /// caller reports the shortfall through [`Self::loaded_rows`].
    /// `loop_secs` is the composition's written length — what a repeat loops on,
    /// which is not the same as where playback ends when it does not (that is the
    /// last note-off plus [`TAIL_SECS`]). `repeat` is read every block, so the
    /// checkbox takes effect on a running transport.
    pub fn start(plans: Vec<RowPlan>, loop_secs: f64, repeat: Arc<AtomicBool>) -> Result<Self> {
        Self::start_prepared(Self::prepare(plans)?, loop_secs, repeat)
    }

    /// Load every row's plugin and resolve its schedule — the slow half of
    /// starting, and the reason it is a step of its own.
    ///
    /// A plugin instance costs real time to create (jdrummer ~80 ms, a big
    /// synth a second) and the Composer needs one *per row*, so a six-row
    /// composition is half a second before a sound. Doing that on the GUI thread
    /// freezes the window, which is what makes a Play button feel broken; the
    /// result is `Send`, so the caller can do this on a thread of its own and
    /// hand it to [`Self::start_prepared`] when it lands.
    pub fn prepare(plans: Vec<RowPlan>) -> Result<PreparedComposition> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("No audio output device found")?;
        let cfg = device.default_output_config()?;
        let sample_rate = cfg.sample_rate().0 as f64;
        // Not the device's raw maximum: see `declared_block_size`. Setting a
        // plugin up for millions of frames makes it — and the scratch below —
        // reserve memory by the hundreds of megabytes, per row.
        let max_block = declared_block_size(cfg.buffer_size());
        let channels = cfg.channels() as usize;
        let stream_cfg: cpal::StreamConfig = cfg.into();

        let total_rows = plans.len();
        let PreparedVoices {
            voices,
            plugins,
            end_sample,
            rows_playing,
        } = prepare_voices(plans, sample_rate, max_block as i32)?;
        Ok(PreparedComposition {
            voices,
            plugins,
            end_sample,
            sample_rate,
            channels,
            stream_cfg,
            total_rows,
            rows_playing,
        })
    }

    /// Open the output stream for an already-prepared composition. Cheap — a
    /// couple of milliseconds — so this half belongs on the GUI thread, where
    /// `cpal::Stream` has to live anyway.
    pub fn start_prepared(
        prepared: PreparedComposition,
        loop_secs: f64,
        repeat: Arc<AtomicBool>,
    ) -> Result<Self> {
        let PreparedComposition {
            voices,
            plugins,
            end_sample,
            sample_rate,
            channels,
            stream_cfg,
            total_rows,
            rows_playing,
        } = prepared;
        // Which rows each voice plays, kept on this side too: a live edit has to
        // merge the rows of a voice back into one schedule, and the voices
        // themselves belong to the audio callback from here on.
        let voice_rows: Vec<Vec<u64>> = voices.iter().map(|v| v.row_ids.clone()).collect();
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("No audio output device found")?;

        let mut voices = voices;
        let position = Arc::new(AtomicU64::new(0));
        let finished = Arc::new(AtomicBool::new(false));

        // Where a repeat wraps. Never before the last event: a length that
        // rounds down a hair must not cut the note it lands on.
        let last_event = end_sample.saturating_sub((TAIL_SECS * sample_rate).round() as u64);
        let loop_sample = Arc::new(AtomicU64::new(
            loop_sample_for(loop_secs, sample_rate, last_event),
        ));
        let live: Arc<Mutex<Option<LiveUpdate>>> = Arc::new(Mutex::new(None));

        let cb_position = position.clone();
        let cb_finished = finished.clone();
        let cb_loop = loop_sample.clone();
        let cb_live = live.clone();
        let cb_repeat = repeat;
        let started = Arc::new(OnceLock::new());
        let latency_us = Arc::new(AtomicU64::new(0));
        let cb_started = started.clone();
        let cb_latency = latency_us.clone();

        let stream = device.build_output_stream(
            &stream_cfg,
            move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let frames = out.len() / channels;
                if frames == 0 {
                    return;
                }
                // Where this pass sits on the wall clock, for anything lining
                // itself up against the transport. The device says when this
                // block will actually be *heard*, which is a device buffer
                // ahead of when it is asked for — 100 ms on this machine, most
                // of a 1/16 note, and a recording placed without it lands a
                // grid step late all the way through.
                if cb_started.get().is_none() {
                    let _ = cb_started.set(Instant::now());
                    let stamp = info.timestamp();
                    let latency = stamp
                        .playback
                        .duration_since(&stamp.callback)
                        .map_or(0, |d| d.as_micros() as u64);
                    cb_latency.store(latency, Ordering::Relaxed);
                }
                // One pass per chunk of the device block. Without a repeat there
                // is exactly one; with one, the block is split at the loop point
                // so the wrap lands on the right sample.
                let mut done = 0usize;
                while done < frames {
                    let pos = cb_position.load(Ordering::Relaxed);
                    let repeat = cb_repeat.load(Ordering::Relaxed);
                    let loop_sample = cb_loop.load(Ordering::Relaxed);

                    // Ticking Repeat on during the release tail: the loop point
                    // is already behind us and will not come round again, so go
                    // back now rather than play on to the end.
                    if repeat && pos >= loop_sample {
                        rewind(&mut voices, &cb_position);
                        continue;
                    }

                    let remaining = frames - done;
                    let n = if repeat {
                        remaining.min((loop_sample - pos) as usize)
                    } else {
                        remaining
                    };
                    // The last chunk of a pass takes every event still in hand.
                    // A note ending exactly on the loop point would otherwise
                    // have its note-off skipped and sound forever.
                    let wraps = repeat && pos + n as u64 >= loop_sample;
                    mix_block(
                        &mut voices,
                        &mut out[done * channels..(done + n) * channels],
                        channels,
                        pos,
                        n,
                        wraps,
                    );

                    let new_pos = pos + n as u64;
                    cb_position.store(new_pos, Ordering::Relaxed);
                    done += n;

                    if wraps {
                        // Straight into the next pass: nothing is reset on the
                        // plugins, so releases ring on over the loop.
                        rewind(&mut voices, &cb_position);
                        // The loop point is also where an edit made while this
                        // was playing comes in, so a pass is never rearranged
                        // underneath itself.
                        take_live_update(&cb_live, &mut voices, &cb_loop);
                    } else if !repeat && new_pos >= end_sample {
                        cb_finished.store(true, Ordering::Relaxed);
                    }
                }
            },
            |e| log::error!("Composer audio error: {}", e),
            None,
        )?;
        stream.play()?;

        let instances = plugins.len();
        log::info!(
            "Composer playback started: {rows_playing}/{total_rows} row(s) on \
             {instances} instance(s), {:.1}s",
            end_sample as f64 / sample_rate
        );

        Ok(Self {
            stream: Some(stream),
            _plugins: plugins,
            position,
            finished,
            loop_sample,
            live,
            started,
            latency_us,
            sample_rate,
            voice_rows,
            gains_diverged: Arc::new(AtomicBool::new(false)),
            loaded_rows: rows_playing,
            instances,
            total_rows,
            total_secs: end_sample as f64 / sample_rate,
        })
    }

    /// The length the transport is looping on, in seconds — it follows a live
    /// edit, so a tempo change or an added note moves it.
    pub fn loop_secs(&self) -> f64 {
        self.loop_sample.load(Ordering::Relaxed) as f64 / self.sample_rate.max(1.0)
    }

    /// Hand the transport a composition edited while it plays. It is taken up
    /// whole at the next loop point; until then the pass in flight is untouched.
    ///
    /// Rows are matched by [`RowEdit::row_id`]: a row the transport is not
    /// playing is ignored (it has no plugin loaded, and an audio callback cannot
    /// load one), and a row it *is* playing that no longer appears simply falls
    /// silent. Which track a row plays therefore cannot be changed this way.
    ///
    /// Rows sharing an instance are merged back along the grouping the transport
    /// started with — that cannot be revisited either, for the same reason. If an
    /// edit gives two of them different gains, the instance keeps the first row's
    /// and [`Self::gains_diverged`] says so: one output cannot be at two levels.
    pub fn update_live(&self, rows: &[RowEdit], loop_secs: f64) {
        let mut last_event = 0u64;
        let mut diverged = false;
        let rows: Vec<VoiceEdit> = self
            .voice_rows
            .iter()
            .enumerate()
            .map(|(voice, ids)| {
                let mut notes: Vec<PlannedNote> = Vec::new();
                let mut gain = None;
                for edit in ids.iter().filter_map(|id| rows.iter().find(|r| r.row_id == *id)) {
                    notes.extend_from_slice(&edit.notes);
                    match gain {
                        None => gain = Some(edit.gain),
                        Some(first) => diverged |= first != edit.gain,
                    }
                }
                let (schedule, last) = schedule_from(&notes, self.sample_rate);
                last_event = last_event.max(last);
                VoiceEdit {
                    voice,
                    gain: gain.unwrap_or(0.0),
                    schedule,
                }
            })
            .collect();
        self.gains_diverged.store(diverged, Ordering::Relaxed);
        let update = LiveUpdate {
            rows,
            loop_sample: loop_sample_for(loop_secs, self.sample_rate, last_event),
            applied: false,
        };
        // Blocking is fine here: the audio callback only ever tries the lock, and
        // holds it for the length of a swap. Replacing what is in the slot drops
        // the previous update — old schedule buffers included — on this thread.
        if let Ok(mut slot) = self.live.lock() {
            *slot = Some(update);
        }
    }

    /// Where the composition was *heard* to be at wall-clock instant `t`, in
    /// seconds from its start — what a note played then lines up with.
    ///
    /// Not simply `t` minus the moment Play was pressed: the callback produces
    /// audio a device buffer before it is heard, so the ear is that far behind
    /// the transport and a player answering what they hear is that far "late".
    /// Subtracting the device's own reported latency puts the take where the
    /// user meant it.
    ///
    /// `None` until the device has asked for its first block (there is no
    /// clock to place anything on yet), and negative for an instant before it.
    pub fn heard_secs_at(&self, t: Instant) -> Option<f64> {
        let started = *self.started.get()?;
        // Signed: an instant from before the first block is a negative time,
        // which is how the caller tells a keystroke that beat the transport
        // from one that followed it.
        let since = match t.checked_duration_since(started) {
            Some(d) => d.as_secs_f64(),
            None => -started.saturating_duration_since(t).as_secs_f64(),
        };
        Some(since - self.latency_us.load(Ordering::Relaxed) as f64 / 1e6)
    }

    /// Whether the last live edit asked for two levels on one instance — rows
    /// that share one were given different gains. The transport plays them at
    /// the first row's; hearing the rest takes a fresh Play, which is free to
    /// group them differently.
    pub fn gains_diverged(&self) -> bool {
        self.gains_diverged.load(Ordering::Relaxed)
    }

    /// Seconds played so far.
    pub fn position_secs(&self) -> f64 {
        self.position.load(Ordering::Relaxed) as f64 / self.sample_rate.max(1.0)
    }

    /// True once the last note plus its tail has been played.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

/// Where a repeat wraps, in samples: the written length, but never before the
/// last event — a length that rounds down a hair must not cut the note on it.
fn loop_sample_for(loop_secs: f64, sample_rate: f64, last_event: u64) -> u64 {
    ((loop_secs * sample_rate).round().max(1.0) as u64).max(last_event)
}

/// A row's notes as the sorted `(sample, pitch, note-on)` schedule the callback
/// walks, and the sample its last event falls on.
fn schedule_from(notes: &[PlannedNote], sample_rate: f64) -> (Vec<(u64, u8, bool)>, u64) {
    let mut schedule: Vec<(u64, u8, bool)> = Vec::with_capacity(notes.len() * 2);
    let mut last_event = 0u64;
    for n in notes {
        let on = (n.at_secs * sample_rate).round().max(0.0) as u64;
        // At least one sample of note, so a 1/128 at a low tempo cannot
        // collapse into a note-off that precedes its own note-on.
        let off = on + ((n.dur_secs * sample_rate).round().max(1.0) as u64);
        schedule.push((on, n.pitch, true));
        schedule.push((off, n.pitch, false));
        last_event = last_event.max(off);
    }
    schedule.sort_by_key(|&(t, _, on)| (t, on));
    (schedule, last_event)
}

/// Adopt an edit, if one is waiting. Called at the loop point, from the audio
/// thread, so it must not allocate or free: the new schedules are *swapped* with
/// the old, which are left in the slot for the GUI thread to drop.
fn take_live_update(
    live: &Mutex<Option<LiveUpdate>>,
    voices: &mut [Voice],
    loop_sample: &AtomicU64,
) {
    let Ok(mut slot) = live.try_lock() else {
        // The GUI is mid-write; next pass, then.
        return;
    };
    let Some(update) = slot.as_mut() else { return };
    if update.applied {
        return;
    }
    for (i, voice) in voices.iter_mut().enumerate() {
        // By position: an edit is built from the same voice list this walks, so
        // the two cannot drift, and a voice serving several rows has no single
        // row id to be found by.
        match update.rows.iter_mut().find(|r| r.voice == i) {
            Some(edit) => {
                std::mem::swap(&mut voice.schedule, &mut edit.schedule);
                voice.gain = edit.gain;
            }
            // Every row this voice played was deleted. Its plugin stays loaded —
            // unloading is not something this thread can do — but it has nothing
            // left to play.
            None => voice.schedule.clear(),
        }
        voice.cursor = 0;
    }
    loop_sample.store(update.loop_sample, Ordering::Relaxed);
    update.applied = true;
}

/// Back to the top of the composition: every row plays its schedule again from
/// the first event. The plugins are left alone — a note still releasing carries
/// over into the next pass, which is what makes a loop sound like a loop.
fn rewind(voices: &mut [Voice], position: &AtomicU64) {
    for voice in voices.iter_mut() {
        voice.cursor = 0;
    }
    position.store(0, Ordering::Relaxed);
}

/// Which rows can be played by one plugin instance, as groups of indices into
/// `plans`.
///
/// **Why share.** An instance is expensive in exactly the way a composition
/// multiplies: a LeSynth track carries its grid and a rendered buffer per key,
/// a sampler its whole kit, and the Composer used to load one per row. Six rows
/// of one drum track were six copies of that kit in memory — and six times the
/// loading. A chord recorded from the keyboard is three rows of one instrument,
/// which is the same instrument playing three notes.
///
/// **When it is safe.** Three things have to hold, and each of them is a way the
/// sharing would otherwise be audible:
///
/// * **The same recipe.** A different library, class, grid or saved state is a
///   different sound; nothing to share.
/// * **The same gain.** One instance has one output, so rows sharing it are
///   mixed at one level. Rows at different levels get an instance each.
/// * **No collision.** A note-off names a pitch, not a note: if two rows have
///   the same pitch sounding at once, one row's release would cut the other's
///   note short. Rows that would do that are kept apart.
///
/// First-fit, in row order, so the first row of a group is the one whose recipe
/// and gain the group is named by — and a composition of unrelated rows comes
/// out exactly as it went in, one group each.
fn share_groups(plans: &[RowPlan]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    'plan: for (i, plan) in plans.iter().enumerate() {
        for group in groups.iter_mut() {
            let host = &plans[group[0]];
            if !same_recipe(&host.source, &plan.source) || host.gain != plan.gain {
                continue;
            }
            if group
                .iter()
                .any(|&j| notes_collide(&plans[j].notes, &plan.notes))
            {
                continue;
            }
            group.push(i);
            continue 'plan;
        }
        groups.push(vec![i]);
    }
    groups
}

/// Whether two rows would load the identical instance. The track's *name* is not
/// part of it: two registry entries pointing at the same library with the same
/// state are the same sound whatever they are called.
fn same_recipe(a: &PlaybackSource, b: &PlaybackSource) -> bool {
    a.plugin_path == b.plugin_path
        && a.class_id == b.class_id
        && a.is_lesynth == b.is_lesynth
        && a.state == b.state
        && a.vst_state == b.vst_state
}

/// Whether any note of `a` sounds at the same time as one of `b` on the same
/// pitch.
///
/// Quadratic in principle, and deliberately so: it runs once per candidate pair
/// when Play is pressed, over the notes a person typed into a row. The pitch
/// test comes first, so the arithmetic only happens for the few notes that could
/// possibly collide.
fn notes_collide(a: &[PlannedNote], b: &[PlannedNote]) -> bool {
    a.iter()
        .any(|x| b.iter().any(|y| x.collides_with(y)))
}

/// Load an instance per row, import its grid and resolve its notes to sample
/// times. Returns the voices, the instances keeping their libraries alive, and
/// the last sample of the composition (the final note-off plus [`TAIL_SECS`]).
///
/// A row whose plugin fails to load is logged and skipped: one broken track must
/// not take the rest of the composition with it. `plugins.len()` is how many
/// actually loaded.
/// One ready instance per plan, `None` where the row could not be loaded — a
/// broken row is logged and skipped rather than taking the composition with it.
///
/// **One at a time, deliberately.** Loading six rows takes six times as long as
/// one, and doing them on six threads would take about one — but a plugin's
/// instances are not independent enough for that. jdrummer livelocks on it: two
/// threads inside its construction spin forever, about one run in three. Making
/// the first instance alone (a plugin's one-time global setup) and the rest
/// together only lowered that to one in ten. A DAW that freezes for good one
/// time in ten is worse than one that takes three seconds, so this stays
/// sequential; the loading is off the GUI thread instead, where waiting for it
/// costs the user a message rather than a dead window.
fn load_instances(
    plans: &[RowPlan],
    groups: &[Vec<usize>],
    sample_rate: f64,
    max_block: i32,
) -> Vec<Option<Arc<PluginInstance>>> {
    // One module per distinct library rather than one per group: that is what a
    // VST3 module is for, and it saves a `dlopen` and a `ModuleEntry` each time.
    let mut modules: HashMap<PathBuf, Option<Arc<Vst3Module>>> = HashMap::new();
    for plan in plans {
        modules
            .entry(plan.source.plugin_path.clone())
            .or_insert_with(|| match Vst3Module::open(&plan.source.plugin_path) {
                Ok(m) => Some(Arc::new(m)),
                Err(e) => {
                    log::warn!("Composer: '{}' failed to load: {e:#}", plan.source.name);
                    None
                }
            });
    }

    // One instance per **group**, built from the row that opened it — every row
    // in a group has the same recipe, so any of them describes it.
    groups
        .iter()
        .map(|group| {
            let plan = &plans[group[0]];
            let module = modules.get(&plan.source.plugin_path).cloned().flatten();
            load_one(module, plan, sample_rate, max_block)
        })
        .collect()
}

/// One instance, ready to play: created, given the state the project saved, and
/// set up for audio.
fn load_one(
    module: Option<Arc<Vst3Module>>,
    plan: &RowPlan,
    sample_rate: f64,
    max_block: i32,
) -> Option<Arc<PluginInstance>> {
    let module = module?;
    // Only LeSynth exposes the state ABI, and only a tagged instance can be
    // addressed by it.
    let token = plan.source.is_lesynth.then(next_instance_token);
    let inst = match PluginInstance::from_module(module, plan.source.class_id.as_ref(), token) {
        Ok(i) => Arc::new(i),
        Err(e) => {
            log::warn!("Composer: '{}' failed to load: {e:#}", plan.source.name);
            return None;
        }
    };

    // Before activation, as the spec has it: a plugin sizes its buffers for the
    // state it is given. This is where a third-party VST3's knobs come back —
    // what the user set in its editor, or what the project saved.
    if let Some(bytes) = &plan.source.vst_state {
        // Only if it would change something. A fresh instance is often already
        // in the saved state (the project was saved from one), and restoring
        // anyway is not free: jdrummer reloads its whole SoundFont on
        // `setState` whether the kit changed or not — 430 ms a row, against a
        // fraction of a millisecond to ask what state it is in.
        let unchanged = inst
            .component_state()
            .is_ok_and(|current| current == *bytes);
        if !unchanged {
            if let Err(e) = inst.set_component_state(bytes) {
                log::warn!("Composer: '{}' state restore failed: {e:#}", plan.source.name);
            }
        }
    }
    let _ = inst.initialize_audio(sample_rate, max_block);
    if let Some(state) = &plan.source.state {
        if let Err(e) = inst.import_state(state) {
            log::warn!("Composer: '{}' grid import failed: {e:#}", plan.source.name);
        }
    }
    Some(inst)
}

/// Everything [`prepare_voices`] settles: the voices themselves, the instances
/// keeping their libraries alive, where the composition ends, and how many rows
/// actually got a voice — which is not the number of voices, since one serves
/// however many rows it can.
struct PreparedVoices {
    voices: Vec<Voice>,
    plugins: Vec<Arc<PluginInstance>>,
    end_sample: u64,
    rows_playing: usize,
}

fn prepare_voices(
    plans: Vec<RowPlan>,
    sample_rate: f64,
    max_block: i32,
) -> Result<PreparedVoices> {
    // Which rows can be played by one instance. Everything below is per *group*
    // from here on: the loading, the schedule and the mix.
    let groups = share_groups(&plans);
    // The module is opened once per library rather than once per group: that is
    // what a VST3 module is for, and `ModuleEntry` is not a thing to race.
    let instances = load_instances(&plans, &groups, sample_rate, max_block);

    let mut voices: Vec<Voice> = Vec::new();
    let mut plugins: Vec<Arc<PluginInstance>> = Vec::new();
    let mut last_event = 0u64;
    let mut rows_playing = 0usize;

    for (group, inst) in groups.iter().zip(instances) {
        let Some(inst) = inst else { continue };
        let host = &plans[group[0]];

        // Every note of every row in the group, on one instance. Sorted into one
        // schedule by `schedule_from`, so the callback still only walks a cursor.
        let notes: Vec<PlannedNote> = group
            .iter()
            .flat_map(|&i| plans[i].notes.iter().copied())
            .collect();
        let (schedule, group_last_event) = schedule_from(&notes, sample_rate);
        last_event = last_event.max(group_last_event);
        rows_playing += group.len();

        let event_impl = Arc::new(EventList::default());
        let event_list = ComWrapper::new((*event_impl).clone())
            .to_com_ptr::<IEventList>()
            .context("Failed to create event list COM ptr")?;

        // The bus layout the plugin settled on in `initialize_audio`. A plugin
        // that declares no output bus still needs somewhere to write, so it gets
        // a stereo one; anything else is taken as declared.
        let io = inst.io();
        let out_channels_per_bus = if io.outputs.is_empty() {
            vec![2usize]
        } else {
            io.outputs.clone()
        };
        let in_channels: usize = io.inputs.iter().sum();
        let out_channels: usize = out_channels_per_bus.iter().sum();
        let voice_max_block = if io.max_block > 0 {
            io.max_block
        } else {
            max_block.max(0) as usize
        };
        let mut scratch = AudioScratch::new(in_channels + out_channels, voice_max_block);
        let in_buses = bus_buffers(&io.inputs, &mut scratch.ptrs_mut()[..in_channels]);
        let out_buses = bus_buffers(
            &out_channels_per_bus,
            &mut scratch.ptrs_mut()[in_channels..],
        );

        voices.push(Voice {
            row_ids: group.iter().map(|&i| plans[i].row_id).collect(),
            plugin: inst.clone(),
            event_impl,
            event_list,
            gain: host.gain,
            schedule,
            cursor: 0,
            in_channels,
            main_out: out_channels_per_bus[0],
            max_block: voice_max_block,
            scratch,
            in_buses,
            out_buses,
        });
        plugins.push(inst);
    }

    anyhow::ensure!(!voices.is_empty(), "no playable rows");
    let end_sample = last_event + (TAIL_SECS * sample_rate).round() as u64;
    Ok(PreparedVoices {
        voices,
        plugins,
        end_sample,
        rows_playing,
    })
}

/// Process one block of every voice and sum it into `out` (interleaved,
/// `frames * channels`), which is overwritten. Each voice carries its own
/// per-channel scratch, sized to the bus layout its plugin negotiated.
///
/// Shared by the transport's callback and the offline export, so what a `.wav`
/// contains is what the transport plays — down to the summing and the clamp.
fn mix_block(
    voices: &mut [Voice],
    out: &mut [f32],
    channels: usize,
    block_start: u64,
    frames: usize,
    flush_events: bool,
) {
    let block_end = block_start + frames as u64;
    out.fill(0.0);

    for voice in voices.iter_mut() {
        // Events due in this block, offset to their sample in it.
        {
            let mut events = voice.event_impl.events.write().unwrap();
            events.clear();
            while let Some(&(at, pitch, on)) = voice.schedule.get(voice.cursor) {
                // `flush_events` empties the schedule into this block: the caller
                // is about to rewind, and anything left behind is a note-off that
                // would never be sent.
                if at >= block_end && !flush_events {
                    break;
                }
                let status = if on { 0x90 } else { 0x80 };
                let velocity = if on { NOTE_VELOCITY } else { 0 };
                if let Some(mut ev) = midi_to_vst3_event([status, pitch, velocity]) {
                    // Events already due (a late start, or two in the same
                    // block) land on the block's first sample.
                    ev.sampleOffset =
                        at.saturating_sub(block_start).min(frames as u64 - 1) as i32;
                    events.push(ev);
                }
                voice.cursor += 1;
            }
        }

        // Never more than this instance was set up for (see `PluginIo::max_block`).
        let frames = frames.min(voice.max_block);
        voice.scratch.reset(frames);

        let mut data = ProcessData {
            numInputs: voice.in_buses.len() as i32,
            inputs: if voice.in_buses.is_empty() {
                std::ptr::null_mut()
            } else {
                voice.in_buses.as_mut_ptr()
            },
            numOutputs: voice.out_buses.len() as i32,
            outputs: voice.out_buses.as_mut_ptr(),
            numSamples: frames as i32,
            processMode: 0,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            ..unsafe { std::mem::zeroed() }
        };
        data.inputEvents = voice.event_list.as_ptr() as *mut _;

        unsafe {
            voice.plugin.processor.as_com_ref().process(&mut data as *mut _);
        }
        voice.event_impl.events.write().unwrap().clear();

        // Main output bus into the mix. Its channel count is the plugin's, not
        // the device's: a mono plugin repeats, a wider one has the extra dropped.
        if voice.main_out > 0 {
            for frame in 0..frames {
                for ch in 0..channels {
                    let src = voice.in_channels + ch.min(voice.main_out - 1);
                    out[frame * channels + ch] += voice.scratch.channel(src)[frame] * voice.gain;
                }
            }
        }
    }

    // The mix is a sum of independent instruments, so clamp rather than let the
    // device wrap on a loud chord.
    for s in out.iter_mut() {
        *s = s.clamp(-1.0, 1.0);
    }
}

/// Rate and channel count to export at: the default output device's, so a
/// rendered file matches what the transport plays through it. Falls back to CD
/// stereo when there is no device to ask — an export is not playback and has no
/// reason to fail for want of one.
pub fn default_export_format() -> (f64, usize) {
    cpal::default_host()
        .default_output_device()
        .and_then(|d| d.default_output_config().ok())
        .map(|cfg| (cfg.sample_rate().0 as f64, cfg.channels() as usize))
        .unwrap_or((44_100.0, 2))
}

/// The composition rendered to interleaved samples, off the audio device — what
/// "Export WAV" writes.
///
/// Faster than real time: nothing waits on a clock, and a LeSynth key with no
/// pre-rendered buffer renders synchronously on its note-on, so a note cannot
/// come out silent for being asked for sooner than the transport would.
///
/// Returns the samples and how many of `plans` actually loaded, so the caller
/// can report a composition that exported short of what was asked for.
pub fn render_offline(
    plans: Vec<RowPlan>,
    sample_rate: f64,
    channels: usize,
) -> Result<(Vec<f32>, usize, usize)> {
    anyhow::ensure!(channels > 0, "an export needs at least one channel");
    let total_rows = plans.len();
    // The same block size the transport asks for by default. Block size changes
    // nothing about the result — events are placed by sample — so a fixed one
    // keeps the export independent of whatever device happens to be attached.
    const BLOCK: usize = 512;
    let PreparedVoices {
        mut voices,
        plugins,
        end_sample,
        rows_playing,
    } = prepare_voices(plans, sample_rate, BLOCK as i32)?;

    let mut block = vec![0f32; BLOCK * channels];
    let mut out: Vec<f32> = Vec::with_capacity(end_sample as usize * channels);
    let mut pos = 0u64;
    while pos < end_sample {
        let frames = BLOCK.min((end_sample - pos) as usize);
        let buf = &mut block[..frames * channels];
        mix_block(&mut voices, buf, channels, pos, frames, false);
        out.extend_from_slice(buf);
        pos += frames as u64;
    }

    // Voices hold `ComPtr`s into these libraries: drop them first, in the order
    // the transport tears its stream down in.
    drop(voices);
    drop(plugins);
    Ok((out, rows_playing, total_rows))
}

impl Drop for CompositionPlayer {
    fn drop(&mut self) {
        // Explicit, so the ordering that matters is stated rather than inferred
        // from field order: the stream stops before `_plugins` unloads the
        // libraries its callback calls into.
        self.stream = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(at_secs: f64, dur_secs: f64, pitch: u8) -> PlannedNote {
        PlannedNote {
            at_secs,
            dur_secs,
            pitch,
        }
    }

    fn source(path: &str) -> PlaybackSource {
        PlaybackSource {
            name: path.to_string(),
            plugin_path: PathBuf::from(path),
            class_id: None,
            is_lesynth: false,
            state: None,
            vst_state: None,
        }
    }

    fn plan(row_id: u64, source: PlaybackSource, gain: f32, notes: Vec<PlannedNote>) -> RowPlan {
        RowPlan {
            row_id,
            source,
            gain,
            notes,
        }
    }

    /// The saving this exists for: a chord recorded from the keyboard is several
    /// rows of one instrument, and one instance can play all of it.
    #[test]
    fn rows_of_one_instrument_share_one_instance() {
        let plans = vec![
            plan(0, source("/x.so"), 1.0, vec![note(0.0, 1.0, 60)]),
            plan(1, source("/x.so"), 1.0, vec![note(0.0, 1.0, 64)]),
            plan(2, source("/x.so"), 1.0, vec![note(0.0, 1.0, 67)]),
        ];
        assert_eq!(share_groups(&plans), vec![vec![0, 1, 2]]);
    }

    /// A different sound is a different instance, however alike the rows look.
    #[test]
    fn a_different_recipe_is_never_shared() {
        let mut other = source("/x.so");
        other.class_id = Some([9; 16]);
        let mut stateful = source("/x.so");
        stateful.vst_state = Some(vec![1, 2, 3]);
        let plans = vec![
            plan(0, source("/x.so"), 1.0, vec![note(0.0, 1.0, 60)]),
            plan(1, source("/y.so"), 1.0, vec![note(0.0, 1.0, 62)]),
            plan(2, other, 1.0, vec![note(0.0, 1.0, 64)]),
            plan(3, stateful, 1.0, vec![note(0.0, 1.0, 65)]),
            // Same library, same state, a different name: the same sound.
            plan(4, source("/x.so"), 1.0, vec![note(0.0, 1.0, 67)]),
        ];
        assert_eq!(
            share_groups(&plans),
            vec![vec![0, 4], vec![1], vec![2], vec![3]]
        );
    }

    /// One instance has one output and so one level. Rows at different gains
    /// have to be kept apart or one of them plays at the other's volume.
    #[test]
    fn rows_at_different_levels_are_never_shared() {
        let plans = vec![
            plan(0, source("/x.so"), 1.0, vec![note(0.0, 1.0, 60)]),
            plan(1, source("/x.so"), 0.5, vec![note(0.0, 1.0, 64)]),
            plan(2, source("/x.so"), 1.0, vec![note(0.0, 1.0, 67)]),
        ];
        assert_eq!(share_groups(&plans), vec![vec![0, 2], vec![1]]);
    }

    /// A note-off names a pitch, not a note: two rows sounding the same pitch at
    /// once on one instance would have the first release cut the second note.
    #[test]
    fn rows_whose_notes_would_collide_are_never_shared() {
        let plans = vec![
            plan(0, source("/x.so"), 1.0, vec![note(0.0, 1.0, 60)]),
            // Same pitch, overlapping — must not share.
            plan(1, source("/x.so"), 1.0, vec![note(0.5, 1.0, 60)]),
            // Same pitch, but after the first has finished — safe.
            plan(2, source("/x.so"), 1.0, vec![note(2.0, 1.0, 60)]),
        ];
        assert_eq!(share_groups(&plans), vec![vec![0, 2], vec![1]]);

        // Touching end to end is not a collision: the note-off is sorted ahead
        // of the note-on at the same sample.
        let touching = vec![
            plan(0, source("/x.so"), 1.0, vec![note(0.0, 1.0, 60)]),
            plan(1, source("/x.so"), 1.0, vec![note(1.0, 1.0, 60)]),
        ];
        assert_eq!(share_groups(&touching), vec![vec![0, 1]]);
    }

    /// Sharing must not change what is played: the merged schedule is every
    /// note of every row in the group, in time order, note-offs ahead of the
    /// note-ons they meet.
    #[test]
    fn a_shared_schedule_holds_every_row_s_notes() {
        let rate = 1000.0;
        let notes = vec![note(0.0, 1.0, 60), note(0.0, 1.0, 64), note(1.0, 1.0, 60)];
        let (schedule, last) = schedule_from(&notes, rate);
        assert_eq!(
            schedule,
            vec![
                (0, 60, true),
                (0, 64, true),
                (1000, 60, false),
                (1000, 64, false),
                (1000, 60, true),
                (2000, 60, false),
            ]
        );
        assert_eq!(last, 2000);
    }
}
