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
//! Hosting an **Audio Unit** (the version 2 API), macOS only.
//!
//! An Audio Unit is not loaded from a path: the system keeps a registry of
//! components, and a host finds one by its three four-character codes — type
//! (`aumu` instrument, `aufx` effect, `aumf` music effect), subtype and
//! manufacturer — and asks AudioToolbox for an instance. Those codes are the
//! plugin id this host records (`au:aumu:Dls :appl`); the path is
//! [`super::AU_PATH`], a placeholder.
//!
//! Everything goes through AudioToolbox's C API: the stream format and the
//! largest block are *properties*, notes are `MusicDeviceMIDIEvent` calls made
//! before each render, a render is `AudioUnitRender` into the host's own
//! non-interleaved buffers, and the plugin's saved state is its `ClassInfo`
//! property list, carried as a binary plist.
//!
//! Showing an Audio Unit's own editor (a Cocoa view) needs the main thread,
//! which winit owns in this app, so the editor is not offered yet — the same
//! as for every other format on macOS.

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use anyhow::{bail, Context, Result};

use super::processor::{AudioScratch, MidiEvent, RealtimeProcess};
use super::{FoundPlugin, PluginEditor, PluginFormat, PluginIo, AU_ID_PREFIX, AU_PATH};

type OSStatus = i32;
type AudioComponent = *mut c_void;
type AudioUnit = *mut c_void;
type CFTypeRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmpteTime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    type_: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SmpteTime,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

/// Most channels one buffer list here carries.
const MAX_BUFFERS: usize = 32;

/// `AudioBufferList` with room for [`MAX_BUFFERS`] buffers.
#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; MAX_BUFFERS],
}

type RenderCallback = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> OSStatus;

#[repr(C)]
struct AuRenderCallbackStruct {
    input_proc: RenderCallback,
    input_proc_ref_con: *mut c_void,
}

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioComponentFindNext(component: AudioComponent, desc: *const AudioComponentDescription) -> AudioComponent;
    fn AudioComponentCopyName(component: AudioComponent, name: *mut CFTypeRef) -> OSStatus;
    fn AudioComponentGetDescription(component: AudioComponent, desc: *mut AudioComponentDescription) -> OSStatus;
    fn AudioComponentInstanceNew(component: AudioComponent, instance: *mut AudioUnit) -> OSStatus;
    fn AudioComponentInstanceDispose(instance: AudioUnit) -> OSStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> OSStatus;
    fn AudioUnitGetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32) -> OSStatus;
    fn AudioUnitRender(
        unit: AudioUnit,
        flags: *mut u32,
        time: *const AudioTimeStamp,
        bus: u32,
        frames: u32,
        data: *mut AudioBufferList,
    ) -> OSStatus;
    fn MusicDeviceMIDIEvent(unit: AudioUnit, status: u32, data1: u32, data2: u32, offset: u32) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *mut c_void);
    fn CFStringGetCString(s: CFTypeRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, length: isize) -> CFTypeRef;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFPropertyListCreateData(alloc: CFTypeRef, list: CFTypeRef, format: isize, options: usize, error: *mut CFTypeRef) -> CFTypeRef;
    fn CFPropertyListCreateWithData(
        alloc: CFTypeRef,
        data: CFTypeRef,
        options: usize,
        format: *mut isize,
        error: *mut CFTypeRef,
    ) -> CFTypeRef;
}

const UTF8: u32 = 0x0800_0100;
const PLIST_BINARY: isize = 200;

const PROP_CLASS_INFO: u32 = 0;
const PROP_STREAM_FORMAT: u32 = 8;
const PROP_MAX_FRAMES: u32 = 14;
const PROP_SET_RENDER_CALLBACK: u32 = 23;
const SCOPE_GLOBAL: u32 = 0;
const SCOPE_INPUT: u32 = 1;
const SCOPE_OUTPUT: u32 = 2;

const TYPE_MUSIC_DEVICE: u32 = u32::from_be_bytes(*b"aumu");
const TYPE_EFFECT: u32 = u32::from_be_bytes(*b"aufx");
const TYPE_MUSIC_EFFECT: u32 = u32::from_be_bytes(*b"aumf");
const FORMAT_LPCM: u32 = u32::from_be_bytes(*b"lpcm");
/// Float, packed, non-interleaved.
const FORMAT_FLAGS: u32 = 1 | 8 | (1 << 5);
const TIMESTAMP_SAMPLE_TIME_VALID: u32 = 1;

/// What a saved state starts with.
const STATE_TAG: &[u8; 8] = b"GAUST1\0\0";

/// A four-character code as the plugin id writes it: the characters where
/// they are plain ASCII (nearly always), eight hex digits otherwise.
fn code_text(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(|b| (0x20..0x7f).contains(b) && *b != b':') {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("0x{code:08x}")
    }
}

fn parse_code(s: &str) -> Option<u32> {
    if let Some(hex) = s.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).ok();
    }
    let bytes: [u8; 4] = s.as_bytes().try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// `au:<type>:<subtype>:<manufacturer>`.
fn plugin_id(desc: &AudioComponentDescription) -> String {
    format!(
        "{AU_ID_PREFIX}{}:{}:{}",
        code_text(desc.component_type),
        code_text(desc.component_sub_type),
        code_text(desc.component_manufacturer)
    )
}

fn parse_id(id: &str) -> Result<AudioComponentDescription> {
    let rest = id
        .strip_prefix(AU_ID_PREFIX)
        .with_context(|| format!("'{id}' is not an Audio Unit id"))?;
    let parts: Vec<&str> = rest.split(':').collect();
    let [t, s, m] = parts[..] else {
        bail!("'{id}' is not an Audio Unit id (au:type:subtype:manufacturer)");
    };
    Ok(AudioComponentDescription {
        component_type: parse_code(t).context("bad type code")?,
        component_sub_type: parse_code(s).context("bad subtype code")?,
        component_manufacturer: parse_code(m).context("bad manufacturer code")?,
        ..Default::default()
    })
}

unsafe fn cf_string(s: CFTypeRef) -> String {
    if s.is_null() {
        return String::new();
    }
    let mut buf = [0 as c_char; 512];
    let ok = CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, UTF8);
    if ok == 0 {
        return String::new();
    }
    std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
}

unsafe fn component_name(component: AudioComponent) -> String {
    let mut name: CFTypeRef = std::ptr::null();
    if AudioComponentCopyName(component, &mut name) != 0 || name.is_null() {
        return String::new();
    }
    let s = cf_string(name);
    CFRelease(name as *mut c_void);
    s
}

/// Every instrument and effect Audio Unit the system knows, for the picker.
pub fn list_plugins() -> Vec<FoundPlugin> {
    let mut out = Vec::new();
    for component_type in [TYPE_MUSIC_DEVICE, TYPE_MUSIC_EFFECT, TYPE_EFFECT] {
        let wanted = AudioComponentDescription { component_type, ..Default::default() };
        let mut component: AudioComponent = std::ptr::null_mut();
        loop {
            component = unsafe { AudioComponentFindNext(component, &wanted) };
            if component.is_null() {
                break;
            }
            let mut desc = AudioComponentDescription::default();
            if unsafe { AudioComponentGetDescription(component, &mut desc) } != 0 {
                continue;
            }
            // "Manufacturer: Plugin" is how the system names them.
            let name = unsafe { component_name(component) };
            out.push(FoundPlugin {
                name: name.rsplit(": ").next().unwrap_or(&name).to_string(),
                format: PluginFormat::Au,
                path: AU_PATH.into(),
                plugin_id: Some(plugin_id(&desc)),
            });
        }
    }
    out
}

/// A loaded Audio Unit.
pub struct AuInstance {
    unit: AudioUnit,
    desc: AudioComponentDescription,
    name: String,
    io: RwLock<PluginIo>,
    initialized: AtomicBool,
}

// AudioToolbox units may be configured from one thread and rendered from
// another, which is what every Audio Unit host does.
unsafe impl Send for AuInstance {}
unsafe impl Sync for AuInstance {}

impl AuInstance {
    pub fn load(plugin_id: &str) -> Result<Self> {
        let desc = parse_id(plugin_id)?;
        let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &desc) };
        anyhow::ensure!(!component.is_null(), "no Audio Unit {plugin_id} is installed");
        let name = unsafe { component_name(component) };
        let mut unit: AudioUnit = std::ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(component, &mut unit) };
        anyhow::ensure!(status == 0 && !unit.is_null(), "'{name}' could not be instantiated ({status})");
        log::info!("Loaded Audio Unit '{name}' ({plugin_id})");
        Ok(Self {
            unit,
            desc,
            name,
            io: RwLock::new(PluginIo::default()),
            initialized: AtomicBool::new(false),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn io(&self) -> PluginIo {
        self.io.read().unwrap().clone()
    }

    fn is_effect(&self) -> bool {
        self.desc.component_type != TYPE_MUSIC_DEVICE
    }

    /// The channel count of a scope's element 0, as the unit offers it.
    fn channels(&self, scope: u32) -> Option<u32> {
        let mut format = AudioStreamBasicDescription::default();
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        let status = unsafe {
            AudioUnitGetProperty(
                self.unit,
                PROP_STREAM_FORMAT,
                scope,
                0,
                &mut format as *mut _ as *mut c_void,
                &mut size,
            )
        };
        (status == 0 && format.channels_per_frame > 0).then_some(format.channels_per_frame)
    }

    fn set_format(&self, scope: u32, sample_rate: f64, channels: u32) -> OSStatus {
        let format = AudioStreamBasicDescription {
            sample_rate,
            format_id: FORMAT_LPCM,
            format_flags: FORMAT_FLAGS,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        };
        unsafe {
            AudioUnitSetProperty(
                self.unit,
                PROP_STREAM_FORMAT,
                scope,
                0,
                &format as *const _ as *const c_void,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            )
        }
    }

    pub fn initialize_audio(&self, sample_rate: f64, max_block_size: i32) -> Result<()> {
        if self.initialized.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let max_block = max_block_size.max(1) as u32;
        let out_channels = self.channels(SCOPE_OUTPUT).unwrap_or(2).clamp(1, MAX_BUFFERS as u32);
        let status = self.set_format(SCOPE_OUTPUT, sample_rate, out_channels);
        if status != 0 {
            log::warn!("'{}' declined the output format ({status})", self.name);
        }
        let mut in_channels = 0;
        if self.is_effect() {
            in_channels = self.channels(SCOPE_INPUT).unwrap_or(out_channels).clamp(1, MAX_BUFFERS as u32);
            self.set_format(SCOPE_INPUT, sample_rate, in_channels);
            // An effect pulls its input through a callback; this host's
            // effects are fed silence.
            let callback = AuRenderCallbackStruct {
                input_proc: silent_input,
                input_proc_ref_con: std::ptr::null_mut(),
            };
            unsafe {
                AudioUnitSetProperty(
                    self.unit,
                    PROP_SET_RENDER_CALLBACK,
                    SCOPE_INPUT,
                    0,
                    &callback as *const _ as *const c_void,
                    std::mem::size_of::<AuRenderCallbackStruct>() as u32,
                );
            }
        }
        unsafe {
            AudioUnitSetProperty(
                self.unit,
                PROP_MAX_FRAMES,
                SCOPE_GLOBAL,
                0,
                &max_block as *const u32 as *const c_void,
                4,
            );
        }
        let status = unsafe { AudioUnitInitialize(self.unit) };
        if status != 0 {
            self.initialized.store(false, Ordering::SeqCst);
            bail!("'{}' refused to initialise ({status})", self.name);
        }
        let io = PluginIo {
            inputs: if in_channels > 0 { vec![in_channels as usize] } else { Vec::new() },
            outputs: vec![out_channels as usize],
            max_block: max_block as usize,
        };
        log::info!(
            "Audio Unit '{}' initialised: sr={sample_rate}, block={max_block}, in={:?}, out={:?}",
            self.name,
            io.inputs,
            io.outputs
        );
        *self.io.write().unwrap() = io;
        Ok(())
    }

    /// The unit's `ClassInfo` property list, as a binary plist.
    pub fn save_state(&self) -> Result<Vec<u8>> {
        unsafe {
            let mut plist: CFTypeRef = std::ptr::null();
            let mut size = std::mem::size_of::<CFTypeRef>() as u32;
            let status = AudioUnitGetProperty(
                self.unit,
                PROP_CLASS_INFO,
                SCOPE_GLOBAL,
                0,
                &mut plist as *mut CFTypeRef as *mut c_void,
                &mut size,
            );
            anyhow::ensure!(status == 0 && !plist.is_null(), "'{}' would not save its state ({status})", self.name);
            let data = CFPropertyListCreateData(std::ptr::null(), plist, PLIST_BINARY, 0, std::ptr::null_mut());
            CFRelease(plist as *mut c_void);
            anyhow::ensure!(!data.is_null(), "'{}' saved a state that is not a property list", self.name);
            let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize);
            let mut out = STATE_TAG.to_vec();
            out.extend_from_slice(bytes);
            CFRelease(data as *mut c_void);
            Ok(out)
        }
    }

    pub fn restore_state(&self, bytes: &[u8]) -> Result<()> {
        let body = bytes
            .strip_prefix(&STATE_TAG[..])
            .with_context(|| format!("'{}' was handed a state that is not an Audio Unit's", self.name))?;
        unsafe {
            let data = CFDataCreate(std::ptr::null(), body.as_ptr(), body.len() as isize);
            anyhow::ensure!(!data.is_null(), "could not hold the state");
            let plist = CFPropertyListCreateWithData(std::ptr::null(), data, 0, std::ptr::null_mut(), std::ptr::null_mut());
            CFRelease(data as *mut c_void);
            anyhow::ensure!(!plist.is_null(), "the saved state is not a property list");
            let status = AudioUnitSetProperty(
                self.unit,
                PROP_CLASS_INFO,
                SCOPE_GLOBAL,
                0,
                &plist as *const CFTypeRef as *const c_void,
                std::mem::size_of::<CFTypeRef>() as u32,
            );
            CFRelease(plist as *mut c_void);
            anyhow::ensure!(status == 0, "'{}' would not load the saved state ({status})", self.name);
        }
        Ok(())
    }

    pub fn create_editor(&self) -> Result<Box<dyn PluginEditor>> {
        bail!("Audio Unit editors cannot be shown by this build yet")
    }

    pub(crate) fn realtime(
        &self,
        _scratch: &mut AudioScratch,
        in_buses: &[usize],
        out_buses: &[usize],
        _max_block: usize,
    ) -> Result<Box<dyn RealtimeProcess>> {
        let in_channels: usize = in_buses.iter().sum();
        let out_channels = out_buses.first().copied().unwrap_or(2).min(MAX_BUFFERS);
        Ok(Box::new(AuRealtime {
            unit: self.unit,
            instrument: !self.is_effect(),
            first_output: in_channels,
            out_channels,
            sample_time: 0.0,
            list: AudioBufferList {
                number_buffers: 0,
                buffers: [AudioBuffer {
                    number_channels: 1,
                    data_byte_size: 0,
                    data: std::ptr::null_mut(),
                }; MAX_BUFFERS],
            },
        }))
    }
}

impl Drop for AuInstance {
    fn drop(&mut self) {
        unsafe {
            if self.initialized.swap(false, Ordering::SeqCst) {
                AudioUnitUninitialize(self.unit);
            }
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

/// An effect's input: silence.
unsafe extern "C" fn silent_input(
    _ref_con: *mut c_void,
    _flags: *mut u32,
    _time: *const AudioTimeStamp,
    _bus: u32,
    _frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus {
    if let Some(list) = data.as_mut() {
        let n = (list.number_buffers as usize).min(MAX_BUFFERS);
        for buffer in &list.buffers[..n] {
            if !buffer.data.is_null() {
                std::ptr::write_bytes(buffer.data as *mut u8, 0, buffer.data_byte_size as usize);
            }
        }
    }
    0
}

struct AuRealtime {
    unit: AudioUnit,
    instrument: bool,
    /// Where the outputs start in the scratch.
    first_output: usize,
    out_channels: usize,
    sample_time: f64,
    list: AudioBufferList,
}

unsafe impl Send for AuRealtime {}

impl RealtimeProcess for AuRealtime {
    fn process(&mut self, scratch: &mut AudioScratch, frames: usize, events: &[MidiEvent]) {
        if self.instrument {
            for ev in events {
                unsafe {
                    MusicDeviceMIDIEvent(
                        self.unit,
                        ev.data[0] as u32,
                        ev.data[1] as u32,
                        ev.data[2] as u32,
                        ev.offset,
                    );
                }
            }
        }
        let ptrs = scratch.ptrs_mut();
        self.list.number_buffers = self.out_channels as u32;
        for (i, buffer) in self.list.buffers[..self.out_channels].iter_mut().enumerate() {
            buffer.number_channels = 1;
            buffer.data_byte_size = (frames * 4) as u32;
            buffer.data = ptrs[self.first_output + i] as *mut c_void;
        }
        let time = AudioTimeStamp {
            sample_time: self.sample_time,
            flags: TIMESTAMP_SAMPLE_TIME_VALID,
            ..Default::default()
        };
        let mut flags = 0u32;
        unsafe {
            AudioUnitRender(self.unit, &mut flags, &time, 0, frames as u32, &mut self.list);
        }
        self.sample_time += frames as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_audio_unit_id_round_trips() {
        let desc = AudioComponentDescription {
            component_type: TYPE_MUSIC_DEVICE,
            component_sub_type: u32::from_be_bytes(*b"dls "),
            component_manufacturer: u32::from_be_bytes(*b"appl"),
            ..Default::default()
        };
        let id = plugin_id(&desc);
        assert_eq!(id, "au:aumu:dls :appl");
        let back = parse_id(&id).unwrap();
        assert_eq!(back.component_sub_type, desc.component_sub_type);
        assert_eq!(parse_code("0x00000001"), Some(1));
    }
}
