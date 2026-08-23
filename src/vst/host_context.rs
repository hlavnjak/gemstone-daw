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
//! The **host context** a plugin is initialised with, and the small COM objects
//! it is entitled to ask us for.
//!
//! `IComponent::initialize(context)` is where a plugin finds out who is hosting
//! it. Passing null there is legal but poor: an SDK-based plugin uses the context
//! to allocate the `IMessage` / `IAttributeList` objects its processor and its
//! controller talk to each other with, and several refuse to initialise at all
//! without one. [`HostApplication`] is that context.
//!
//! [`MemoryStream`] is the other half of a correct hand-off: a plugin whose
//! controller is a separate object only learns the processor's state when the
//! host pumps it through an `IBStream`.

// The COM method names are fixed by the interfaces being implemented.
#![allow(non_snake_case)]

use std::ffi::{c_void, CStr, CString};
use std::sync::Mutex;

use vst3::Steinberg::Vst::{
    IAttributeList, IAttributeListTrait, IAttributeList_::AttrID, IHostApplication,
    IHostApplicationTrait, IMessage, IMessageTrait, String128,
};
use vst3::Steinberg::Vst::TChar;
use vst3::Steinberg::{
    kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, tresult, FIDString, IBStream,
    IBStreamTrait, IBStream_::IStreamSeekMode_, TUID,
};
use vst3::{Class, ComWrapper, Interface};

/// A VST3 `TUID` (16 `i8`) as the plain bytes an interface `IID` is expressed in.
fn tuid_bytes(tuid: &TUID) -> [u8; 16] {
    std::array::from_fn(|i| tuid[i] as u8)
}

/// Write a Rust string into a VST3 `String128` (UTF-16, NUL-terminated).
fn write_string128(dst: *mut String128, text: &str) {
    if dst.is_null() {
        return;
    }
    unsafe {
        let out = &mut *dst;
        let mut i = 0;
        for unit in text.encode_utf16() {
            if i + 1 >= out.len() {
                break;
            }
            out[i] = unit as TChar;
            i += 1;
        }
        out[i] = 0;
    }
}

/// What this host calls itself, and the factory for the plumbing objects a
/// plugin's two halves use to talk to one another.
pub struct HostApplication;

impl Class for HostApplication {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        write_string128(name, "Gemstone DAW");
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        _iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if cid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        let requested = tuid_bytes(&*cid);

        // The two classes the SDK's own host context offers. A plugin asks for
        // one of them by class id and casts to the matching interface.
        if requested == IMessage::IID {
            let message = ComWrapper::new(HostMessage::default());
            if let Some(ptr) = message.to_com_ptr::<IMessage>() {
                *obj = ptr.into_raw() as *mut c_void;
                return kResultOk;
            }
        }
        if requested == IAttributeList::IID {
            let attrs = ComWrapper::new(HostAttributeList::default());
            if let Some(ptr) = attrs.to_com_ptr::<IAttributeList>() {
                *obj = ptr.into_raw() as *mut c_void;
                return kResultOk;
            }
        }

        *obj = std::ptr::null_mut();
        kNotImplemented
    }
}

impl HostApplication {
    /// A context pointer to hand to `initialize`, kept alive by the returned
    /// wrapper — drop that and the plugin is left with a dangling context.
    pub fn new() -> ComWrapper<HostApplication> {
        ComWrapper::new(HostApplication)
    }
}

/// One value in a [`HostAttributeList`].
enum AttrValue {
    Int(i64),
    Float(f64),
    /// UTF-16, as the API stores it.
    String(Vec<TChar>),
    Binary(Vec<u8>),
}

/// The attribute bag carried by an [`HostMessage`]. Keys are plain C strings.
#[derive(Default)]
pub struct HostAttributeList {
    values: Mutex<Vec<(CString, AttrValue)>>,
}

impl Class for HostAttributeList {
    type Interfaces = (IAttributeList,);
}

impl HostAttributeList {
    fn set(&self, id: AttrID, value: AttrValue) -> tresult {
        if id.is_null() {
            return kInvalidArgument;
        }
        let key = unsafe { CStr::from_ptr(id) }.to_owned();
        let mut values = self.values.lock().unwrap();
        match values.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => values.push((key, value)),
        }
        kResultOk
    }

    fn with<R>(&self, id: AttrID, f: impl FnOnce(&AttrValue) -> R) -> Option<R> {
        if id.is_null() {
            return None;
        }
        let key = unsafe { CStr::from_ptr(id) };
        let values = self.values.lock().unwrap();
        values
            .iter()
            .find(|(k, _)| k.as_c_str() == key)
            .map(|(_, v)| f(v))
    }
}

impl IAttributeListTrait for HostAttributeList {
    unsafe fn setInt(&self, id: AttrID, value: i64) -> tresult {
        self.set(id, AttrValue::Int(value))
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut i64) -> tresult {
        if value.is_null() {
            return kInvalidArgument;
        }
        match self.with(id, |v| match v {
            AttrValue::Int(i) => Some(*i),
            _ => None,
        }) {
            Some(Some(i)) => {
                *value = i;
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.set(id, AttrValue::Float(value))
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        if value.is_null() {
            return kInvalidArgument;
        }
        match self.with(id, |v| match v {
            AttrValue::Float(f) => Some(*f),
            _ => None,
        }) {
            Some(Some(f)) => {
                *value = f;
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut text = Vec::new();
        let mut p = string;
        while *p != 0 {
            text.push(*p);
            p = p.add(1);
        }
        text.push(0);
        self.set(id, AttrValue::String(text))
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, sizeInBytes: u32) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let cap = (sizeInBytes as usize) / std::mem::size_of::<TChar>();
        if cap == 0 {
            return kInvalidArgument;
        }
        match self.with(id, |v| match v {
            AttrValue::String(s) => Some(s.clone()),
            _ => None,
        }) {
            Some(Some(text)) => {
                let n = text.len().min(cap - 1);
                std::ptr::copy_nonoverlapping(text.as_ptr(), string, n);
                *string.add(n) = 0;
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, sizeInBytes: u32) -> tresult {
        if data.is_null() {
            return kInvalidArgument;
        }
        let bytes = std::slice::from_raw_parts(data as *const u8, sizeInBytes as usize).to_vec();
        self.set(id, AttrValue::Binary(bytes))
    }

    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        sizeInBytes: *mut u32,
    ) -> tresult {
        if data.is_null() || sizeInBytes.is_null() {
            return kInvalidArgument;
        }
        // The pointer points into the list's own storage, which stays valid
        // until the same key is overwritten — the SDK's own contract.
        match self.with(id, |v| match v {
            AttrValue::Binary(b) => Some((b.as_ptr(), b.len())),
            _ => None,
        }) {
            Some(Some((ptr, len))) => {
                *data = ptr as *const c_void;
                *sizeInBytes = len as u32;
                kResultOk
            }
            _ => {
                *data = std::ptr::null();
                *sizeInBytes = 0;
                kResultFalse
            }
        }
    }
}

/// A message passed between a plugin's processor and its controller. The host
/// owns the class; the plugin only fills it in and hands it to the other side.
pub struct HostMessage {
    id: Mutex<CString>,
    attributes: ComWrapper<HostAttributeList>,
}

impl Default for HostMessage {
    fn default() -> Self {
        HostMessage {
            id: Mutex::new(CString::default()),
            attributes: ComWrapper::new(HostAttributeList::default()),
        }
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        // Points into the `CString` this message owns; it stays valid until the
        // next `setMessageID`, which is exactly the SDK's own lifetime rule.
        self.id.lock().unwrap().as_ptr()
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let new = if id.is_null() {
            CString::default()
        } else {
            CStr::from_ptr(id).to_owned()
        };
        *self.id.lock().unwrap() = new;
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut())
    }
}

/// A plain in-memory `IBStream`, which is how component state is carried over to
/// the edit controller (and how a plugin's own state could be saved).
#[derive(Default)]
pub struct MemoryStream {
    inner: Mutex<StreamInner>,
}

#[derive(Default)]
struct StreamInner {
    data: Vec<u8>,
    pos: usize,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl MemoryStream {
    pub fn new() -> ComWrapper<MemoryStream> {
        ComWrapper::new(MemoryStream::default())
    }

    /// Rewind to the start — what the host does between writing a component's
    /// state and handing the same stream to the controller.
    pub fn rewind(&self) {
        self.inner.lock().unwrap().pos = 0;
    }

    /// How much has been written, for logging a state hand-off.
    pub fn byte_len(&self) -> usize {
        self.inner.lock().unwrap().data.len()
    }
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, numBytes: i32, numBytesRead: *mut i32) -> tresult {
        if buffer.is_null() || numBytes < 0 {
            return kInvalidArgument;
        }
        let mut inner = self.inner.lock().unwrap();
        let n = (inner.data.len() - inner.pos).min(numBytes as usize);
        std::ptr::copy_nonoverlapping(inner.data[inner.pos..].as_ptr(), buffer as *mut u8, n);
        inner.pos += n;
        if !numBytesRead.is_null() {
            *numBytesRead = n as i32;
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        numBytes: i32,
        numBytesWritten: *mut i32,
    ) -> tresult {
        if buffer.is_null() || numBytes < 0 {
            return kInvalidArgument;
        }
        let n = numBytes as usize;
        let mut inner = self.inner.lock().unwrap();
        let pos = inner.pos;
        if inner.data.len() < pos + n {
            inner.data.resize(pos + n, 0);
        }
        std::ptr::copy_nonoverlapping(buffer as *const u8, inner.data[pos..].as_mut_ptr(), n);
        inner.pos += n;
        if !numBytesWritten.is_null() {
            *numBytesWritten = n as i32;
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> tresult {
        let mut inner = self.inner.lock().unwrap();
        let base = match mode {
            m if m == IStreamSeekMode_::kIBSeekSet as i32 => 0i64,
            m if m == IStreamSeekMode_::kIBSeekCur as i32 => inner.pos as i64,
            m if m == IStreamSeekMode_::kIBSeekEnd as i32 => inner.data.len() as i64,
            _ => return kInvalidArgument,
        };
        let target = (base + pos).clamp(0, inner.data.len() as i64);
        inner.pos = target as usize;
        if !result.is_null() {
            *result = target;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        *pos = self.inner.lock().unwrap().pos as i64;
        kResultOk
    }
}
