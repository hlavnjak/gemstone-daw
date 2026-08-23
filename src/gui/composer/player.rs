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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use vst3::Steinberg::Vst::{
    AudioBusBuffers, IAudioProcessorTrait, IEventList,
    ProcessData, SymbolicSampleSizes_,
};
use vst3::{ComPtr, ComWrapper};

use crate::audio::midi_to_vst3_event;
use crate::gui::registry::PlaybackSource;
use crate::audio::engine::{bus_buffers, AudioScratch};
use crate::vst::{next_instance_token, EventList, PluginInstance};

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
    row_id: u64,
    gain: f32,
    schedule: Vec<(u64, u8, bool)>,
}

/// A row's live instance plus its event schedule, as the callback sees it.
struct Voice {
    /// The panel's row id: which row of the composition this voice is playing,
    /// so an edit can be matched to it by identity rather than by position.
    row_id: u64,
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
    sample_rate: f64,
    /// Rows that actually loaded, and rows asked for.
    pub loaded_rows: usize,
    pub total_rows: usize,
    /// End of the composition, tail included.
    pub total_secs: f64,
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
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("No audio output device found")?;
        let cfg = device.default_output_config()?;
        let sample_rate = cfg.sample_rate().0 as f64;
        let max_block = match cfg.buffer_size() {
            cpal::SupportedBufferSize::Range { max, .. } => *max,
            _ => 512,
        };
        let channels = cfg.channels() as usize;
        let stream_cfg: cpal::StreamConfig = cfg.into();

        let total_rows = plans.len();
        let (voices, plugins, end_sample) = prepare_voices(plans, sample_rate, max_block as i32)?;
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

        let stream = device.build_output_stream(
            &stream_cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let frames = out.len() / channels;
                if frames == 0 {
                    return;
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

        let loaded_rows = plugins.len();
        log::info!(
            "Composer playback started: {loaded_rows}/{total_rows} row(s), {:.1}s",
            end_sample as f64 / sample_rate
        );

        Ok(Self {
            stream: Some(stream),
            _plugins: plugins,
            position,
            finished,
            loop_sample,
            live,
            sample_rate,
            loaded_rows,
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
    pub fn update_live(&self, rows: &[RowEdit], loop_secs: f64) {
        let mut last_event = 0u64;
        let rows: Vec<VoiceEdit> = rows
            .iter()
            .map(|row| {
                let (schedule, last) = schedule_from(&row.notes, self.sample_rate);
                last_event = last_event.max(last);
                VoiceEdit {
                    row_id: row.row_id,
                    gain: row.gain,
                    schedule,
                }
            })
            .collect();
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
    for voice in voices.iter_mut() {
        match update.rows.iter_mut().find(|r| r.row_id == voice.row_id) {
            Some(edit) => {
                std::mem::swap(&mut voice.schedule, &mut edit.schedule);
                voice.gain = edit.gain;
            }
            // The row was deleted. Its plugin stays loaded — unloading is not
            // something this thread can do — but it has nothing left to play.
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

/// Load an instance per row, import its grid and resolve its notes to sample
/// times. Returns the voices, the instances keeping their libraries alive, and
/// the last sample of the composition (the final note-off plus [`TAIL_SECS`]).
///
/// A row whose plugin fails to load is logged and skipped: one broken track must
/// not take the rest of the composition with it. `plugins.len()` is how many
/// actually loaded.
fn prepare_voices(
    plans: Vec<RowPlan>,
    sample_rate: f64,
    max_block: i32,
) -> Result<(Vec<Voice>, Vec<Arc<PluginInstance>>, u64)> {
    let mut voices: Vec<Voice> = Vec::new();
    let mut plugins: Vec<Arc<PluginInstance>> = Vec::new();
    let mut last_event = 0u64;

    for plan in plans {
        // Only LeSynth exposes the state ABI, and only a tagged instance can
        // be addressed by it.
        let token = plan.source.is_lesynth.then(next_instance_token);
        let inst = match PluginInstance::load(
            &plan.source.plugin_path,
            plan.source.class_id.as_ref(),
            token,
        ) {
            Ok(i) => Arc::new(i),
            Err(e) => {
                log::warn!("Composer: '{}' failed to load: {e}", plan.source.name);
                continue;
            }
        };
        let _ = inst.initialize_audio(sample_rate, max_block);
        if let Some(state) = &plan.source.state {
            if let Err(e) = inst.import_state(state) {
                log::warn!("Composer: '{}' grid import failed: {e}", plan.source.name);
            }
        }

        let (schedule, row_last_event) = schedule_from(&plan.notes, sample_rate);
        last_event = last_event.max(row_last_event);

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
            row_id: plan.row_id,
            plugin: inst.clone(),
            event_impl,
            event_list,
            gain: plan.gain,
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
    Ok((voices, plugins, end_sample))
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
    let (mut voices, plugins, end_sample) = prepare_voices(plans, sample_rate, BLOCK as i32)?;

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

    let loaded_rows = plugins.len();
    // Voices hold `ComPtr`s into these libraries: drop them first, in the order
    // the transport tears its stream down in.
    drop(voices);
    drop(plugins);
    Ok((out, loaded_rows, total_rows))
}

impl Drop for CompositionPlayer {
    fn drop(&mut self) {
        // Explicit, so the ordering that matters is stated rather than inferred
        // from field order: the stream stops before `_plugins` unloads the
        // libraries its callback calls into.
        self.stream = None;
    }
}
