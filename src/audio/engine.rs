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
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, IAudioProcessorTrait,
    IEventList, IParameterChanges, ParamID, ParamValue, ProcessData, SymbolicSampleSizes_,
};
use vst3::Steinberg::Vst::Event_::EventTypes_;
use vst3::ComWrapper;

use crate::vst::{EventList, ParamChanges, PluginInstance};
use crate::midi::MidiEventQueue;

/// Audio engine configuration derived from the system audio device.
pub struct AudioConfig {
    pub sample_rate: f64,
    pub max_buffer_size: u32,
    pub channels: usize,
}

/// The audio engine manages the CPAL stream and routes audio through VST3 plugins.
pub struct AudioEngine {
    _stream: Option<cpal::Stream>,
    pub config: AudioConfig,
}

impl AudioEngine {
    /// Query the default audio device and return its configuration.
    pub fn query_device_config() -> Result<AudioConfig> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No audio output device found"))?;
        let cfg = device.default_output_config()?;

        let sample_rate = cfg.sample_rate().0 as f64;
        let max_buffer_size = declared_block_size(cfg.buffer_size());
        let channels = cfg.channels() as usize;

        Ok(AudioConfig {
            sample_rate,
            max_buffer_size,
            channels,
        })
    }

    /// Start audio processing for `plugin`, fed by `midi_events`.
    ///
    /// The whole instance is taken, not just its processor, and the callback
    /// holds it: while a stream exists the plugin cannot be terminated and its
    /// library cannot be unloaded, whatever order its owner drops things in. That
    /// is not defensive — tearing the plugin down under a running stream is a
    /// crash on the audio thread, and it was one.
    ///
    /// The bus layout comes from the plugin's own negotiation in
    /// [`crate::vst::PluginInstance::initialize_audio`]. It is not decoration:
    /// `process()` reads `numInputs` buses' worth of channel pointers whether or
    /// not the host meant to send any, so an effect (or any plugin with a
    /// side-chain) handed a hard-coded "no inputs, one stereo output"
    /// dereferences pointers that were never provided.
    pub fn start(plugin: Arc<PluginInstance>, midi_events: MidiEventQueue) -> Result<Self> {
        let io = plugin.io();
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No audio output device found"))?;
        let cfg = device.default_output_config()?;

        let sample_rate = cfg.sample_rate().0 as f64;
        let max_buffer_size = declared_block_size(cfg.buffer_size());
        let channels = cfg.channels() as usize;
        let stream_cfg: cpal::StreamConfig = cfg.into();

        let event_impl = Arc::new(EventList::default());
        let vst_event_list = ComWrapper::new((*event_impl).clone());
        let event_list_ptr = vst_event_list
            .to_com_ptr::<IEventList>()
            .expect("Failed to create event list COM ptr");

        // What the plugin's own editor changed since the last block. It reaches
        // the plugin only here: while it is processing it will not write its own
        // parameters, so a block that leaves `inputParameterChanges` null is a
        // block in which nothing the user touched in the editor happened.
        let param_edits = plugin.param_edits().clone();
        let param_changes = ParamChanges::default();
        let param_changes_ptr = ComWrapper::new(param_changes.clone())
            .to_com_ptr::<IParameterChanges>()
            .expect("Failed to create parameter changes COM ptr");
        // Drained into once a block; kept out here so the callback allocates
        // nothing while a knob is being dragged.
        let mut edits_this_block: Vec<(ParamID, ParamValue)> = Vec::new();

        // A plugin that declares no output bus at all still has to be given one
        // to write into; and the main bus is what the device hears.
        let out_bus_channels: Vec<usize> = if io.outputs.is_empty() {
            vec![channels.max(1)]
        } else {
            io.outputs.clone()
        };
        let in_bus_channels: Vec<usize> = io.inputs.clone();
        let main_out = out_bus_channels[0];

        // Every buffer the plugin will be handed, allocated once: the audio
        // callback must not allocate, and the channel pointers below have to stay
        // put for the life of the stream. Input channels come first, then output.
        // What the plugin was actually set up for. The device is asked for its
        // format twice — once to initialise the plugin, once here — and if those
        // two answers ever differ, the plugin is the one that gets a block it
        // never sized for. Its own promise wins.
        let plugin_max_block = if io.max_block > 0 {
            io.max_block
        } else {
            max_buffer_size as usize
        };

        let in_channels: usize = in_bus_channels.iter().sum();
        let out_channels: usize = out_bus_channels.iter().sum();
        let mut scratch =
            AudioScratch::new(in_channels + out_channels, plugin_max_block);
        // The bus descriptors point into the scratch's pointer table, whose
        // allocation does not move when the scratch is moved into the closure.
        let mut in_buses = bus_buffers(&in_bus_channels, &mut scratch.ptrs_mut()[..in_channels]);
        let mut out_buses =
            bus_buffers(&out_bus_channels, &mut scratch.ptrs_mut()[in_channels..]);

        static WARNED_SHORT: AtomicBool = AtomicBool::new(false);
        WARNED_SHORT.store(false, AtomicOrdering::Relaxed);

        let stream = device.build_output_stream(
            &stream_cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Never ask a plugin for more than the block size it was set up
                // for. A device callback bigger than that is not expected; the
                // tail is left silent rather than written past the plugin's own
                // buffers, which is what a mismatch here actually costs.
                let frames = (out.len() / channels).min(plugin_max_block);
                if frames * channels < out.len() && !WARNED_SHORT.swap(true, AtomicOrdering::Relaxed)
                {
                    log::warn!(
                        "device asked for {} frames but the plugin was set up for {plugin_max_block}; \
                         the rest of each block is silence",
                        out.len() / channels
                    );
                }

                scratch.reset(frames);

                let mut data = ProcessData {
                    numInputs: in_buses.len() as i32,
                    inputs: if in_buses.is_empty() {
                        std::ptr::null_mut()
                    } else {
                        in_buses.as_mut_ptr()
                    },
                    numOutputs: out_buses.len() as i32,
                    outputs: out_buses.as_mut_ptr(),
                    numSamples: frames as i32,
                    processMode: 0,
                    symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                    ..unsafe { std::mem::zeroed() }
                };

                // Convert MIDI events to VST3 events
                {
                    let mut events = event_impl.events.write().unwrap();
                    let mut queue = midi_events.lock().unwrap();
                    while let Some(msg) = queue.pop_front() {
                        if let Some(vst_event) = midi_to_vst3_event(msg) {
                            events.push(vst_event);
                        }
                    }
                }

                data.inputEvents = event_list_ptr.as_ptr() as *mut _;

                // …and what the plugin's editor changed since the last block.
                // Null when nothing did: an empty list is a list, and a plugin
                // is entitled to walk one it is handed.
                param_edits.drain_into(&mut edits_this_block);
                if param_changes.load(&edits_this_block) {
                    data.inputParameterChanges = param_changes_ptr.as_ptr() as *mut _;
                }

                unsafe {
                    plugin.processor.as_com_ref().process(&mut data as *mut _);
                }

                // Consume events after processing
                {
                    let mut events = event_impl.events.write().unwrap();
                    events.clear();
                }

                // Main output bus → the device, interleaved. The two channel
                // counts need not match: a mono plugin on a stereo device repeats
                // its last channel, a wider plugin gets its extra channels dropped.
                if frames * channels < out.len() {
                    out[frames * channels..].fill(0.0);
                }
                if main_out == 0 {
                    out.fill(0.0);
                } else {
                    for frame in 0..frames {
                        for ch in 0..channels {
                            let src = in_channels + ch.min(main_out - 1);
                            out[frame * channels + ch] = scratch.channel(src)[frame];
                        }
                    }
                }
            },
            |e| log::error!("Audio error: {}", e),
            None,
        )?;

        stream.play()?;
        log::info!("Audio stream started");

        Ok(AudioEngine {
            _stream: Some(stream),
            config: AudioConfig {
                sample_rate,
                max_buffer_size,
                channels,
            },
        })
    }
}

/// Upper bound on the block size declared to a plugin.
///
/// A device's *supported* range can top out in the millions of frames — this
/// machine's reports 4 194 304 — and `setupProcessing` is a promise: a plugin
/// sizes its internal buffers for the maximum it is told. Hand over the raw
/// range maximum and a plugin with seventeen stereo buses reserves half a
/// gigabyte it will never use, which is seconds of stall on the thread that
/// asked. Real callbacks are three orders of magnitude below this cap.
pub(crate) const MAX_DECLARED_BLOCK: u32 = 16_384;

/// The block size to set a plugin up for, given what the device says it can
/// deliver. Every path that starts a plugin goes through this: the number is a
/// promise the plugin allocates against, and two copies of it drift.
pub(crate) fn declared_block_size(buffer_size: &cpal::SupportedBufferSize) -> u32 {
    match buffer_size {
        cpal::SupportedBufferSize::Range { max, .. } => (*max).min(MAX_DECLARED_BLOCK),
        _ => 512,
    }
}

/// The buffers a plugin's `process()` writes through, and the pointer table it
/// reads them from.
///
/// Allocated once per stream and owned by whoever drives `process()` — the audio
/// callback must not allocate, and the pointer table the bus descriptors read
/// must not move. Raw pointers are not `Send` on their own; this whole set is,
/// because nothing but its owner ever touches it.
pub(crate) struct AudioScratch {
    planar: Vec<Vec<f32>>,
    ptrs: Vec<*mut f32>,
}

unsafe impl Send for AudioScratch {}

impl AudioScratch {
    /// `channels` buffers of `frames` samples each, plus the overrun pad.
    pub(crate) fn new(channels: usize, frames: usize) -> Self {
        let mut planar: Vec<Vec<f32>> = (0..channels.max(1))
            .map(|_| vec![0.0f32; frames + PROCESS_OVERRUN_PAD])
            .collect();
        let ptrs = planar.iter_mut().map(|v| v.as_mut_ptr()).collect();
        AudioScratch { planar, ptrs }
    }

    /// Silence `frames` samples (and the pad) on every channel, ready for a
    /// block: input buses have nothing to carry, and a plugin may add into its
    /// output rather than overwrite it. Also re-takes the channel pointers, which
    /// never change but must be derived afresh to stay valid to use.
    pub(crate) fn reset(&mut self, frames: usize) {
        for (slot, channel) in self.ptrs.iter_mut().zip(self.planar.iter_mut()) {
            channel[..frames + PROCESS_OVERRUN_PAD].fill(0.0);
            *slot = channel.as_mut_ptr();
        }
    }

    /// The pointer table, to lay out as bus descriptors.
    pub(crate) fn ptrs_mut(&mut self) -> &mut [*mut f32] {
        &mut self.ptrs
    }

    /// One channel's samples, after a block has been processed.
    pub(crate) fn channel(&self, index: usize) -> &[f32] {
        &self.planar[index]
    }
}

/// Slack allocated past `numSamples` on every channel buffer handed to a plugin.
///
/// Plenty of plugins process in a fixed internal block and round the host's
/// block *up* to it: Dexed works in 16 samples, Surge XT in 32. Ask either for
/// 4410 frames — which is exactly what this machine's device asks us for — and
/// the last internal block runs past the end of a buffer sized to the letter,
/// corrupting the heap. Hosts get away with tight buffers only because their
/// block sizes are powers of two; this pad is what makes any block size safe,
/// and it is far larger than any plausible internal block.
pub(crate) const PROCESS_OVERRUN_PAD: usize = 1024;

/// Lay a flat list of channel pointers out as the per-bus `AudioBusBuffers` the
/// VST3 `ProcessData` wants.
pub(crate) fn bus_buffers(bus_channels: &[usize], ptrs: &mut [*mut f32]) -> Vec<AudioBusBuffers> {
    let mut buses = Vec::with_capacity(bus_channels.len());
    let mut offset = 0;
    for &n in bus_channels {
        buses.push(AudioBusBuffers {
            numChannels: n as i32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: unsafe { ptrs.as_mut_ptr().add(offset) },
            },
        });
        offset += n;
    }
    buses
}

/// Convert a 3-byte MIDI message to a VST3 Event.
pub fn midi_to_vst3_event(msg: [u8; 3]) -> Option<vst3::Steinberg::Vst::Event> {
    let status = msg[0] & 0xF0;
    let channel = msg[0] & 0x0F;
    let note = msg[1];
    let velocity = msg[2];

    match status {
        0x90 if velocity > 0 => {
            let note_on = vst3::Steinberg::Vst::NoteOnEvent {
                channel: channel as i16,
                pitch: note as i16,
                tuning: 0.0,
                velocity: (velocity as f32) / 127.0,
                length: -1,
                noteId: -1,
            };
            Some(vst3::Steinberg::Vst::Event {
                busIndex: 0,
                sampleOffset: 0,
                ppqPosition: 0.0,
                flags: 0,
                r#type: EventTypes_::kNoteOnEvent as u16,
                __field0: vst3::Steinberg::Vst::Event__type0 { noteOn: note_on },
            })
        }
        0x90 | 0x80 => {
            let note_off = vst3::Steinberg::Vst::NoteOffEvent {
                channel: channel as i16,
                pitch: note as i16,
                velocity: (velocity as f32) / 127.0,
                noteId: -1,
                tuning: 0.0,
            };
            Some(vst3::Steinberg::Vst::Event {
                busIndex: 0,
                sampleOffset: 0,
                ppqPosition: 0.0,
                flags: 0,
                r#type: EventTypes_::kNoteOffEvent as u16,
                __field0: vst3::Steinberg::Vst::Event__type0 { noteOff: note_off },
            })
        }
        _ => None,
    }
}