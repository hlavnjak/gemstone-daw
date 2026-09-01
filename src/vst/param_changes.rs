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
//! Carrying a plugin's own knob back to its audio half.
//!
//! When a plugin's editor moves one of its parameters it does not write the new
//! value straight into itself: it calls the *host* back on
//! `IComponentHandler::performEdit`, and waits to be told the value in its next
//! `process()` call, through [`ProcessData::inputParameterChanges`]. A plugin
//! that is processing audio will do nothing else with it — nih-plug, for one,
//! returns early from both its own setter and `setParamNormalized` while
//! `is_processing` is set, precisely because the host is supposed to be the one
//! that closes the loop.
//!
//! So a host that acknowledges `performEdit` and drops it leaves every slider in
//! every plugin editor dead: the editor sets a value, reads the parameter back
//! on the next frame, finds it unchanged, and snaps the handle home. That is
//! what this module exists to prevent.
//!
//! [`ProcessData::inputParameterChanges`]: vst3::Steinberg::Vst::ProcessData

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use vst3::Steinberg::Vst::{
    IParamValueQueue, IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait, ParamID,
    ParamValue,
};
use vst3::Steinberg::{int32, kInvalidArgument, kResultFalse, kResultOk, tresult};
use vst3::{Class, ComPtr, ComWrapper};

/// The edits a plugin's editor has asked for and its audio half has not been
/// told about yet. Written on the GUI thread by
/// [`ParamChangeHandler`](crate::vst::handler::ParamChangeHandler), read on the
/// audio thread once per block.
///
/// One entry per parameter, holding the *latest* value: a slider drag is
/// hundreds of edits to one parameter, and only where it ended up matters by
/// the time the block runs. Collapsing them here, on the GUI thread, is also
/// what keeps the audio side down to one queue per parameter with one point in
/// it, which is the shape VST3 wants anyway (points must be in sample order,
/// and these all land at offset zero).
#[derive(Clone, Default)]
pub struct ParamEdits {
    pending: Arc<Mutex<Vec<(ParamID, ParamValue)>>>,
}

impl ParamEdits {
    /// Record an edit, replacing any earlier one for the same parameter.
    pub fn push(&self, id: ParamID, value: ParamValue) {
        let mut pending = match self.pending.lock() {
            Ok(p) => p,
            // A poisoned lock here would silence the editor for good; the worst
            // a stale entry can do is set a parameter to a value the user asked
            // for a moment earlier.
            Err(p) => p.into_inner(),
        };
        match pending.iter_mut().find(|(pid, _)| *pid == id) {
            Some(slot) => slot.1 = value,
            None => pending.push((id, value)),
        }
    }

    /// Move everything pending into `out`, leaving nothing behind. `out` keeps
    /// its allocation between blocks, so the audio thread does not allocate.
    pub fn drain_into(&self, out: &mut Vec<(ParamID, ParamValue)>) {
        out.clear();
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        out.append(&mut pending);
    }
}

/// One parameter's changes within a block. The state is held apart from the COM
/// object so the host can refill it without going back through the interface.
#[derive(Default)]
struct Queue {
    id: AtomicU32,
    /// `(sample offset, normalised value)`, in offset order — always one point
    /// at offset zero here, since a block is told where a knob ended up.
    points: RwLock<Vec<(int32, ParamValue)>>,
}

/// The COM face of a [`Queue`].
struct ParamValueQueue {
    state: Arc<Queue>,
}

impl Class for ParamValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.state.id.load(Ordering::Acquire)
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.state.points.read().map_or(0, |p| p.len() as int32)
    }

    unsafe fn getPoint(
        &self,
        index: int32,
        sample_offset: *mut int32,
        value: *mut ParamValue,
    ) -> tresult {
        if sample_offset.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        let Ok(points) = self.state.points.read() else {
            return kResultFalse;
        };
        let Some(&(offset, v)) = usize::try_from(index).ok().and_then(|i| points.get(i)) else {
            return kResultFalse;
        };
        *sample_offset = offset;
        *value = v;
        kResultOk
    }

    /// Only a plugin writing to its *output* changes calls this, and nothing
    /// reads those yet — but a plugin is entitled to call it on any queue it is
    /// handed, so it must at least not lie about having stored the point.
    unsafe fn addPoint(
        &self,
        sample_offset: int32,
        value: ParamValue,
        index: *mut int32,
    ) -> tresult {
        let Ok(mut points) = self.state.points.write() else {
            return kResultFalse;
        };
        points.push((sample_offset, value));
        if !index.is_null() {
            *index = points.len() as int32 - 1;
        }
        kResultOk
    }
}

/// The `inputParameterChanges` handed to `process()`: the edits a plugin's own
/// editor made since the last block, in the form the plugin expects to be told
/// about them.
///
/// One instance lives for the whole stream and is refilled each block by
/// [`ParamChanges::load`], so a knob being dragged costs no allocation on the
/// audio thread once the pool of queues has grown to the number of parameters
/// being moved at once — in practice one.
/// Cloned like [`EventList`](crate::vst::EventList): one handle goes into the
/// COM wrapper the plugin is given, the other stays with whoever fills it in.
#[derive(Clone, Default)]
pub struct ParamChanges {
    state: Arc<Changes>,
}

#[derive(Default)]
struct Changes {
    /// Grown on demand and reused; the first `used` of them hold this block.
    queues: RwLock<Vec<(Arc<Queue>, ComPtr<IParamValueQueue>)>>,
    used: AtomicI32,
}

// The COM pointers inside are handed straight back to the plugin on the audio
// thread and are only ever read there; the state behind them is locked.
unsafe impl Send for Changes {}
unsafe impl Sync for Changes {}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl ParamChanges {
    /// Put `edits` up for this block, replacing whatever the last one carried.
    /// Returns whether there is anything to hand over at all, so a block with
    /// no edits can pass a null pointer rather than an empty list.
    pub fn load(&self, edits: &[(ParamID, ParamValue)]) -> bool {
        let Ok(mut queues) = self.state.queues.write() else {
            self.state.used.store(0, Ordering::Release);
            return false;
        };
        for (slot, &(id, value)) in edits.iter().enumerate() {
            if slot == queues.len() {
                let state = Arc::new(Queue::default());
                let Some(com) = ComWrapper::new(ParamValueQueue { state: Arc::clone(&state) })
                    .to_com_ptr::<IParamValueQueue>()
                else {
                    break;
                };
                queues.push((state, com));
            }
            let state = &queues[slot].0;
            state.id.store(id, Ordering::Release);
            if let Ok(mut points) = state.points.write() {
                points.clear();
                points.push((0, value));
            }
        }
        let used = edits.len().min(queues.len());
        self.state.used.store(used as i32, Ordering::Release);
        used > 0
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.state.used.load(Ordering::Acquire)
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index < 0 || index >= self.state.used.load(Ordering::Acquire) {
            return std::ptr::null_mut();
        }
        // Borrowed, not owned: VST3 hands these out without a reference, and
        // this object outlives the `process()` call that reads them.
        self.state
            .queues
            .read()
            .ok()
            .and_then(|q| q.get(index as usize).map(|(_, com)| com.as_ptr()))
            .unwrap_or(std::ptr::null_mut())
    }

    /// A plugin adding to its *input* changes is not something to serve: the
    /// list is the host's account of what the user did.
    unsafe fn addParameterData(
        &self,
        _id: *const ParamID,
        _index: *mut int32,
    ) -> *mut IParamValueQueue {
        std::ptr::null_mut()
    }
}
