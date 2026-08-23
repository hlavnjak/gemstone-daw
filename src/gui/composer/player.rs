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
//! **Export renders through the same voices** ([`render_offline`]): the loading,
//! the schedule and the per-block mix are shared with playback, so a `.wav` is
//! what the transport plays rather than a second implementation of it that can
//! drift. It runs as fast as the plugins allow — a LeSynth key renders
//! synchronously when its buffer is not ready, so no note can come out silent
//! for being asked for early.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, IAudioProcessor, IAudioProcessorTrait, IEventList,
    ProcessData, SymbolicSampleSizes_,
};
use vst3::{ComPtr, ComWrapper};

use crate::audio::midi_to_vst3_event;
use crate::gui::registry::PlaybackSource;
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
#[derive(Clone, Copy)]
pub struct PlannedNote {
    pub at_secs: f64,
    pub dur_secs: f64,
    pub pitch: u8,
}

/// One composer row, ready to play.
pub struct RowPlan {
    pub source: PlaybackSource,
    pub gain: f32,
    pub notes: Vec<PlannedNote>,
}

/// A row's live instance plus its event schedule, as the callback sees it.
struct Voice {
    processor: ComPtr<IAudioProcessor>,
    event_impl: Arc<EventList>,
    event_list: ComPtr<IEventList>,
    gain: f32,
    /// `(sample time, pitch, note-on)`, sorted by time.
    schedule: Vec<(u64, u8, bool)>,
    cursor: usize,
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
    pub fn start(plans: Vec<RowPlan>) -> Result<Self> {
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

        let cb_position = position.clone();
        let cb_finished = finished.clone();
        // Scratch for one row's output, reused every block: the callback mixes
        // each row in as soon as it is processed, so one buffer serves them all.
        let mut planar: Vec<Vec<f32>> = (0..channels).map(|_| Vec::new()).collect();

        let stream = device.build_output_stream(
            &stream_cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let frames = out.len() / channels;
                if frames == 0 {
                    return;
                }
                let block_start = cb_position.load(Ordering::Relaxed);
                let block_end = block_start + frames as u64;
                mix_block(&mut voices, &mut planar, out, channels, block_start, frames);
                cb_position.store(block_end, Ordering::Relaxed);
                if block_end >= end_sample {
                    cb_finished.store(true, Ordering::Relaxed);
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
            sample_rate,
            loaded_rows,
            total_rows,
            total_secs: end_sample as f64 / sample_rate,
        })
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

        let mut schedule: Vec<(u64, u8, bool)> = Vec::with_capacity(plan.notes.len() * 2);
        for n in &plan.notes {
            let on = (n.at_secs * sample_rate).round().max(0.0) as u64;
            // At least one sample of note, so a 1/128 at a low tempo cannot
            // collapse into a note-off that precedes its own note-on.
            let off = on + ((n.dur_secs * sample_rate).round().max(1.0) as u64);
            schedule.push((on, n.pitch, true));
            schedule.push((off, n.pitch, false));
            last_event = last_event.max(off);
        }
        schedule.sort_by_key(|&(t, _, on)| (t, on));

        let event_impl = Arc::new(EventList::default());
        let event_list = ComWrapper::new((*event_impl).clone())
            .to_com_ptr::<IEventList>()
            .context("Failed to create event list COM ptr")?;

        voices.push(Voice {
            processor: inst.processor.clone(),
            event_impl,
            event_list,
            gain: plan.gain,
            schedule,
            cursor: 0,
        });
        plugins.push(inst);
    }

    anyhow::ensure!(!voices.is_empty(), "no playable rows");
    let end_sample = last_event + (TAIL_SECS * sample_rate).round() as u64;
    Ok((voices, plugins, end_sample))
}

/// Process one block of every voice and sum it into `out` (interleaved,
/// `frames * channels`), which is overwritten. `planar` is per-channel scratch,
/// reused across blocks so the real-time path allocates nothing.
///
/// Shared by the transport's callback and the offline export, so what a `.wav`
/// contains is what the transport plays — down to the summing and the clamp.
fn mix_block(
    voices: &mut [Voice],
    planar: &mut [Vec<f32>],
    out: &mut [f32],
    channels: usize,
    block_start: u64,
    frames: usize,
) {
    let block_end = block_start + frames as u64;
    out.fill(0.0);

    for voice in voices.iter_mut() {
        // Events due in this block, offset to their sample in it.
        {
            let mut events = voice.event_impl.events.write().unwrap();
            events.clear();
            while let Some(&(at, pitch, on)) = voice.schedule.get(voice.cursor) {
                if at >= block_end {
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

        for ch in planar.iter_mut() {
            ch.clear();
            ch.resize(frames, 0.0);
        }
        let mut ptrs: Vec<*mut f32> = planar.iter_mut().map(|v| v.as_mut_ptr()).collect();
        let mut bus = AudioBusBuffers {
            numChannels: channels as i32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: ptrs.as_mut_ptr(),
            },
        };
        let mut data = ProcessData {
            numInputs: 0,
            inputs: std::ptr::null_mut(),
            numOutputs: 1,
            outputs: &mut bus as *mut _,
            numSamples: frames as i32,
            processMode: 0,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            ..unsafe { std::mem::zeroed() }
        };
        data.inputEvents = voice.event_list.as_ptr() as *mut _;

        unsafe {
            voice.processor.as_com_ref().process(&mut data as *mut _);
        }
        voice.event_impl.events.write().unwrap().clear();

        for frame in 0..frames {
            for ch in 0..channels {
                out[frame * channels + ch] += planar[ch][frame] * voice.gain;
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

    let mut planar: Vec<Vec<f32>> = (0..channels).map(|_| Vec::new()).collect();
    let mut block = vec![0f32; BLOCK * channels];
    let mut out: Vec<f32> = Vec::with_capacity(end_sample as usize * channels);
    let mut pos = 0u64;
    while pos < end_sample {
        let frames = BLOCK.min((end_sample - pos) as usize);
        let buf = &mut block[..frames * channels];
        mix_block(&mut voices, &mut planar, buf, channels, pos, frames);
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
