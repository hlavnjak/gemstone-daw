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
use crate::vst::param_changes::ParamEdits;
use vst3::Steinberg::Vst::{IComponentHandler, IComponentHandlerTrait, ParamID};
use vst3::Steinberg::{kResultOk, tresult};
use vst3::Class;

/// What a plugin's editor calls when the user moves one of its controls.
///
/// `performEdit` is not a notification: it is how the value gets *anywhere*.
/// A plugin that is processing audio will not write its own parameter — it
/// waits to be handed the change back in `process()` — so an edit acknowledged
/// here and forgotten is an edit that never happened, and the control it came
/// from springs back on the editor's next frame.
pub struct ParamChangeHandler {
    /// Where the edits are left for the audio thread to pick up. Shared with
    /// the [`PluginInstance`](crate::vst::PluginInstance) the handler was made
    /// for, and from there with whoever drives `process()`.
    pub edits: ParamEdits,
}

impl Class for ParamChangeHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ParamChangeHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: f64) -> tresult {
        self.edits.push(id, value);
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, _flags: i32) -> tresult {
        kResultOk
    }
}