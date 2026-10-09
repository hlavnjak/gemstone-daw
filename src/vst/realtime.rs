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
//! A VST3's half of a [`BlockProcessor`](crate::plugin::BlockProcessor): the
//! `ProcessData` a block is rendered through.

use std::sync::Arc;

use anyhow::{Context, Result};
use vst3::Steinberg::Vst::Event_::EventTypes_;
use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, IAudioProcessor, IAudioProcessorTrait, IEventList,
    IParameterChanges, ParamID, ParamValue, ProcessData, SymbolicSampleSizes_,
};
use vst3::{ComPtr, ComWrapper};

use super::{EventList, ParamChanges, ParamEdits, Vst3Instance};
use crate::plugin::processor::{AudioScratch, MidiEvent, RealtimeProcess};

/// Everything one VST3 `process()` call is handed, built once.
struct Vst3Realtime {
    processor: ComPtr<IAudioProcessor>,
    event_impl: Arc<EventList>,
    event_list: ComPtr<IEventList>,
    /// What the plugin's own editor changed since the last block. It reaches
    /// the plugin only here: while it is processing it will not write its own
    /// parameters, so a block that leaves `inputParameterChanges` null is a
    /// block in which nothing the user touched in the editor happened. See
    /// [`crate::vst::param_changes`].
    param_edits: ParamEdits,
    param_changes: ParamChanges,
    param_changes_ptr: ComPtr<IParameterChanges>,
    /// Drained into once a block; kept here so a dragged knob allocates nothing.
    edits_this_block: Vec<(ParamID, ParamValue)>,
    /// The bus descriptors, laid out over the scratch's pointer table once.
    in_buses: Vec<AudioBusBuffers>,
    out_buses: Vec<AudioBusBuffers>,
}

// COM pointers into a plugin that is only ever processed from the one audio
// callback that owns this.
unsafe impl Send for Vst3Realtime {}

/// Build the process state for `plugin` over `scratch`, whose channels are the
/// input buses' then the output buses'.
pub(crate) fn create(
    plugin: &Vst3Instance,
    scratch: &mut AudioScratch,
    in_buses: &[usize],
    out_buses: &[usize],
) -> Result<Box<dyn RealtimeProcess>> {
    let event_impl = Arc::new(EventList::default());
    let event_list = ComWrapper::new((*event_impl).clone())
        .to_com_ptr::<IEventList>()
        .context("Failed to create event list COM ptr")?;
    let param_changes = ParamChanges::default();
    let param_changes_ptr = ComWrapper::new(param_changes.clone())
        .to_com_ptr::<IParameterChanges>()
        .context("Failed to create parameter changes COM ptr")?;
    let in_channels: usize = in_buses.iter().sum();
    let ptrs = scratch.ptrs_mut();
    let in_descs = bus_buffers(in_buses, &mut ptrs[..in_channels]);
    let out_descs = bus_buffers(out_buses, &mut ptrs[in_channels..]);
    Ok(Box::new(Vst3Realtime {
        processor: plugin.processor.clone(),
        event_impl,
        event_list,
        param_edits: plugin.param_edits().clone(),
        param_changes,
        param_changes_ptr,
        edits_this_block: Vec::new(),
        in_buses: in_descs,
        out_buses: out_descs,
    }))
}

impl RealtimeProcess for Vst3Realtime {
    fn process(&mut self, _scratch: &mut AudioScratch, frames: usize, events: &[MidiEvent]) {
        {
            let mut list = self.event_impl.events.write().unwrap();
            list.clear();
            for ev in events {
                if let Some(mut vst_event) = midi_to_vst3_event(ev.data) {
                    vst_event.sampleOffset = ev.offset as i32;
                    list.push(vst_event);
                }
            }
        }

        let mut data = ProcessData {
            numInputs: self.in_buses.len() as i32,
            inputs: if self.in_buses.is_empty() {
                std::ptr::null_mut()
            } else {
                self.in_buses.as_mut_ptr()
            },
            numOutputs: self.out_buses.len() as i32,
            outputs: self.out_buses.as_mut_ptr(),
            numSamples: frames as i32,
            processMode: 0,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            ..unsafe { std::mem::zeroed() }
        };
        data.inputEvents = self.event_list.as_ptr() as *mut _;

        // …and what the plugin's editor changed since the last block. Null when
        // nothing did: an empty list is a list, and a plugin is entitled to walk
        // one it is handed.
        self.param_edits.drain_into(&mut self.edits_this_block);
        if self.param_changes.load(&self.edits_this_block) {
            data.inputParameterChanges = self.param_changes_ptr.as_ptr() as *mut _;
        }

        unsafe {
            self.processor.as_com_ref().process(&mut data as *mut _);
        }
        self.event_impl.events.write().unwrap().clear();
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
