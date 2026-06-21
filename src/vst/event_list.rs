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
use std::sync::{Arc, RwLock};
use vst3::Steinberg::Vst::{Event, IEventList, IEventListTrait};
use vst3::Steinberg::{kInvalidArgument, kResultFalse, kResultOk, tresult};
use vst3::Class;

#[derive(Default)]
pub struct EventList {
    pub events: Arc<RwLock<Vec<Event>>>,
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl Clone for EventList {
    fn clone(&self) -> Self {
        EventList {
            events: Arc::clone(&self.events),
        }
    }
}

impl IEventListTrait for EventList {
    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if let Some(ev) = e.as_ref() {
            let mut events = self.events.write().unwrap();
            events.push(*ev);
            kResultOk
        } else {
            kInvalidArgument
        }
    }

    unsafe fn getEventCount(&self) -> i32 {
        let events = self.events.read().unwrap();
        events.len() as i32
    }

    unsafe fn getEvent(&self, index: i32, e: *mut Event) -> tresult {
        let events = self.events.read().unwrap();
        if index < 0 || index as usize >= events.len() {
            return kResultFalse;
        }
        *e = events[index as usize];
        kResultOk
    }
}