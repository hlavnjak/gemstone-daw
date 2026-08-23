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
use std::sync::Arc;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, IAudioProcessorTrait,
    IEventList, ProcessData, SymbolicSampleSizes_,
};
use vst3::Steinberg::Vst::Event_::EventTypes_;
use vst3::{ComPtr, ComWrapper};
use vst3::Steinberg::Vst::IAudioProcessor;

use crate::vst::{EventList, PluginIo};
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
    /// Upper bound on the block size declared to a plugin.
    ///
    /// A device's *supported* range can top out in the millions of frames, and
    /// `setupProcessing` is a promise: a plugin sizes its internal buffers for
    /// the maximum it is told, so handing it the raw range maximum makes it
    /// allocate hundreds of megabytes it will never use (some simply refuse).
    /// Real callbacks are three orders of magnitude below this cap.
    const MAX_DECLARED_BLOCK: u32 = 16_384;

    /// Query the default audio device and return its configuration.
    pub fn query_device_config() -> Result<AudioConfig> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No audio output device found"))?;
        let cfg = device.default_output_config()?;

        let sample_rate = cfg.sample_rate().0 as f64;
        let max_buffer_size = match cfg.buffer_size() {
            cpal::SupportedBufferSize::Range { max, .. } => (*max).min(Self::MAX_DECLARED_BLOCK),
            _ => 512,
        };
        let channels = cfg.channels() as usize;

        Ok(AudioConfig {
            sample_rate,
            max_buffer_size,
            channels,
        })
    }

    /// Start audio processing with the given VST3 processor and MIDI event queue.
    ///
    /// `io` is the bus layout the plugin negotiated in
    /// [`crate::vst::PluginInstance::initialize_audio`]. It is not decoration:
    /// `process()` reads `numInputs` buses' worth of channel pointers whether or
    /// not the host meant to send any, so an effect (or any plugin with a
    /// side-chain) handed the old hard-coded "no inputs, one stereo output"
    /// dereferences pointers that were never provided.
    pub fn start(
        processor: ComPtr<IAudioProcessor>,
        io: PluginIo,
        midi_events: MidiEventQueue,
    ) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No audio output device found"))?;
        let cfg = device.default_output_config()?;

        let sample_rate = cfg.sample_rate().0 as f64;
        let max_buffer_size = match cfg.buffer_size() {
            cpal::SupportedBufferSize::Range { max, .. } => (*max).min(Self::MAX_DECLARED_BLOCK),
            _ => 512,
        };
        let channels = cfg.channels() as usize;
        let stream_cfg: cpal::StreamConfig = cfg.into();

        let event_impl = Arc::new(EventList::default());
        let vst_event_list = ComWrapper::new((*event_impl).clone());
        let event_list_ptr = vst_event_list
            .to_com_ptr::<IEventList>()
            .expect("Failed to create event list COM ptr");

        // A plugin that declares no output bus at all still has to be given one
        // to write into; and the main bus is what the device hears.
        let out_bus_channels: Vec<usize> = if io.outputs.is_empty() {
            vec![channels.max(1)]
        } else {
            io.outputs.clone()
        };
        let in_bus_channels: Vec<usize> = io.inputs.clone();
        let main_out = out_bus_channels[0];

        let stream = device.build_output_stream(
            &stream_cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Never ask a plugin for more than the block size it was set up
                // for; anything past it would run off the buffers it sized then.
                // Devices do not do this in practice — the guard is here because
                // the cost of being wrong is a write past a plugin's own memory.
                let frames = (out.len() / channels).min(max_buffer_size as usize);

                // One planar buffer per channel of every bus, input buses first.
                // Inputs stay silent — nothing in this host feeds an effect yet —
                // but they must exist, because the plugin will read them.
                let mut in_planar: Vec<Vec<f32>> = in_bus_channels
                    .iter()
                    .flat_map(|&n| (0..n).map(|_| vec![0.0f32; frames]))
                    .collect();
                let mut out_planar: Vec<Vec<f32>> = out_bus_channels
                    .iter()
                    .flat_map(|&n| (0..n).map(|_| vec![0.0f32; frames]))
                    .collect();

                let mut in_ptrs: Vec<*mut f32> =
                    in_planar.iter_mut().map(|v| v.as_mut_ptr()).collect();
                let mut out_ptrs: Vec<*mut f32> =
                    out_planar.iter_mut().map(|v| v.as_mut_ptr()).collect();

                let mut in_buses = bus_buffers(&in_bus_channels, &mut in_ptrs);
                let mut out_buses = bus_buffers(&out_bus_channels, &mut out_ptrs);

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

                unsafe {
                    processor.as_com_ref().process(&mut data as *mut _);
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
                            let src = ch.min(main_out - 1);
                            out[frame * channels + ch] = out_planar[src][frame];
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