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
//! Hosting a **VST2** plugin.
//!
//! Steinberg stopped licensing the VST2 SDK in 2018, and it is still what a
//! great many installed plugins are. Nothing of the SDK is used here: the ABI is
//! one C struct ([`AEffect`]) with a `dispatcher` taking opcodes, and the few
//! structs and numbers below are the well-known layout every clean-room host
//! (Ardour's VeSTige, Carla, the `vst` crate) declares for itself.
//!
//! A VST2 is a bare library exporting `VSTPluginMain` (older ones `main`, and
//! on macOS `main_macho`, inside a `.vst` bundle). Calling it with the host's
//! callback returns the plugin's `AEffect`; everything after that goes through
//! the dispatcher. One library is one plugin — "shell" plugins that hide
//! several behind one id are not supported.

use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void, CStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{bail, Context, Result};
use libloading::Library;

use super::processor::{AudioScratch, MidiEvent, RealtimeProcess, MAX_EVENTS_PER_BLOCK};
use super::{ParentWindow, PluginEditor, PluginIo};

// ---------------------------------------------------------------------------
// The ABI
// ---------------------------------------------------------------------------

type HostCallback =
    unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
type Dispatcher = unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
type ProcessProc = unsafe extern "C" fn(*mut AEffect, *const *const f32, *mut *mut f32, i32);
type ProcessDoubleProc = unsafe extern "C" fn(*mut AEffect, *const *const f64, *mut *mut f64, i32);
type SetParameterProc = unsafe extern "C" fn(*mut AEffect, i32, f32);
type GetParameterProc = unsafe extern "C" fn(*mut AEffect, i32) -> f32;
type PluginMain = unsafe extern "C" fn(HostCallback) -> *mut AEffect;

/// The plugin's half of the ABI.
#[repr(C)]
struct AEffect {
    magic: i32,
    dispatcher: Option<Dispatcher>,
    process: Option<ProcessProc>,
    set_parameter: Option<SetParameterProc>,
    get_parameter: Option<GetParameterProc>,
    num_programs: i32,
    num_params: i32,
    num_inputs: i32,
    num_outputs: i32,
    flags: i32,
    reserved1: isize,
    reserved2: isize,
    initial_delay: i32,
    real_qualities: i32,
    off_qualities: i32,
    io_ratio: f32,
    object: *mut c_void,
    user: *mut c_void,
    unique_id: i32,
    version: i32,
    process_replacing: Option<ProcessProc>,
    process_double_replacing: Option<ProcessDoubleProc>,
    future: [u8; 56],
}

/// `'VstP'`, the first field of every `AEffect`.
const MAGIC: i32 = i32::from_be_bytes(*b"VstP");

// Plugin flags.
const FLAG_HAS_EDITOR: i32 = 1 << 0;
const FLAG_CAN_REPLACING: i32 = 1 << 4;
const FLAG_PROGRAM_CHUNKS: i32 = 1 << 5;
const FLAG_IS_SYNTH: i32 = 1 << 8;

// Dispatcher opcodes (host → plugin).
const EFF_OPEN: i32 = 0;
const EFF_CLOSE: i32 = 1;
const EFF_SET_SAMPLE_RATE: i32 = 10;
const EFF_SET_BLOCK_SIZE: i32 = 11;
const EFF_MAINS_CHANGED: i32 = 12;
const EFF_EDIT_GET_RECT: i32 = 13;
const EFF_EDIT_OPEN: i32 = 14;
const EFF_EDIT_CLOSE: i32 = 15;
const EFF_EDIT_IDLE: i32 = 19;
const EFF_GET_CHUNK: i32 = 23;
const EFF_SET_CHUNK: i32 = 24;
const EFF_PROCESS_EVENTS: i32 = 25;
const EFF_GET_EFFECT_NAME: i32 = 45;
const EFF_GET_PRODUCT_STRING: i32 = 48;
const EFF_START_PROCESS: i32 = 71;
const EFF_STOP_PROCESS: i32 = 72;
const EFF_SET_PROCESS_PRECISION: i32 = 77;

// Host callback opcodes (plugin → host).
const AM_AUTOMATE: i32 = 0;
const AM_VERSION: i32 = 1;
const AM_CURRENT_ID: i32 = 2;
const AM_IDLE: i32 = 3;
const AM_GET_TIME: i32 = 7;
const AM_PROCESS_EVENTS: i32 = 8;
const AM_IO_CHANGED: i32 = 13;
const AM_SIZE_WINDOW: i32 = 15;
const AM_GET_SAMPLE_RATE: i32 = 16;
const AM_GET_BLOCK_SIZE: i32 = 17;
const AM_GET_CURRENT_PROCESS_LEVEL: i32 = 23;
const AM_GET_AUTOMATION_STATE: i32 = 24;
const AM_GET_VENDOR_STRING: i32 = 32;
const AM_GET_PRODUCT_STRING: i32 = 33;
const AM_GET_VENDOR_VERSION: i32 = 34;
const AM_CAN_DO: i32 = 37;
const AM_GET_LANGUAGE: i32 = 38;
const AM_UPDATE_DISPLAY: i32 = 42;
const AM_BEGIN_EDIT: i32 = 43;
const AM_END_EDIT: i32 = 44;

/// `kVstMidiType`.
const MIDI_TYPE: i32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct VstMidiEvent {
    type_: i32,
    byte_size: i32,
    delta_frames: i32,
    flags: i32,
    note_length: i32,
    note_offset: i32,
    midi_data: [u8; 4],
    detune: i8,
    note_off_velocity: i8,
    reserved1: i8,
    reserved2: i8,
}

/// `VstEvents`, sized for a whole block's worth of events: the SDK declares
/// the pointer array as two long and expects the host to allocate more.
#[repr(C)]
struct VstEvents {
    num_events: i32,
    reserved: isize,
    events: [*mut VstMidiEvent; MAX_EVENTS_PER_BLOCK],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VstTimeInfo {
    sample_pos: f64,
    sample_rate: f64,
    nano_seconds: f64,
    ppq_pos: f64,
    tempo: f64,
    bar_start_pos: f64,
    cycle_start_pos: f64,
    cycle_end_pos: f64,
    time_sig_numerator: i32,
    time_sig_denominator: i32,
    smpte_offset: i32,
    smpte_frame_rate: i32,
    samples_to_next_clock: i32,
    flags: i32,
}

const TRANSPORT_PLAYING: i32 = 1 << 1;
const PPQ_POS_VALID: i32 = 1 << 9;
const TEMPO_VALID: i32 = 1 << 10;
const TIME_SIG_VALID: i32 = 1 << 13;

#[repr(C)]
struct ERect {
    top: i16,
    left: i16,
    bottom: i16,
    right: i16,
}

/// What a saved state starts with, so a chunk and a parameter dump can be told
/// apart — and so neither is mistaken for some other format's state.
const CHUNK_TAG: &[u8; 8] = b"GVST2CK\0";
const PARAMS_TAG: &[u8; 8] = b"GVST2PM\0";

// ---------------------------------------------------------------------------
// The host side
// ---------------------------------------------------------------------------

/// What the plugin can ask the host about, per instance.
struct HostShared {
    sample_rate: Mutex<f64>,
    block_size: Mutex<i32>,
    /// What `audioMasterGetTime` points the plugin at. Written by the audio
    /// thread before each block and read by the plugin inside that block.
    time: UnsafeCell<VstTimeInfo>,
    resize_request: Mutex<Option<(u32, u32)>>,
}

unsafe impl Send for HostShared {}
unsafe impl Sync for HostShared {}

/// Which instance an `AEffect` belongs to, for the host callback — which is
/// handed nothing but the `AEffect`.
static INSTANCES: RwLock<Vec<(usize, Arc<HostShared>)>> = RwLock::new(Vec::new());

fn shared_for(effect: *mut AEffect) -> Option<Arc<HostShared>> {
    INSTANCES
        .read()
        .ok()?
        .iter()
        .find(|(e, _)| *e == effect as usize)
        .map(|(_, s)| s.clone())
}

unsafe fn write_c_string(ptr: *mut c_void, s: &str, cap: usize) {
    if ptr.is_null() {
        return;
    }
    let bytes = s.as_bytes();
    let n = bytes.len().min(cap - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, n);
    *(ptr as *mut u8).add(n) = 0;
}

unsafe extern "C" fn host_callback(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    _opt: f32,
) -> isize {
    match opcode {
        // Called from inside `VSTPluginMain`, before there is any instance.
        AM_VERSION => 2400,
        AM_CURRENT_ID => 0,
        AM_IDLE | AM_UPDATE_DISPLAY | AM_IO_CHANGED => 0,
        AM_AUTOMATE | AM_BEGIN_EDIT | AM_END_EDIT => 0,
        AM_GET_TIME => match shared_for(effect) {
            Some(s) => s.time.get() as isize,
            None => 0,
        },
        AM_PROCESS_EVENTS => 0,
        AM_SIZE_WINDOW => {
            if let Some(s) = shared_for(effect) {
                *s.resize_request.lock().unwrap() = Some((index.max(1) as u32, value.max(1) as u32));
                1
            } else {
                0
            }
        }
        AM_GET_SAMPLE_RATE => shared_for(effect).map_or(44_100, |s| *s.sample_rate.lock().unwrap() as isize),
        AM_GET_BLOCK_SIZE => shared_for(effect).map_or(512, |s| *s.block_size.lock().unwrap() as isize),
        // kVstProcessLevelRealtime: every block is rendered for listening.
        AM_GET_CURRENT_PROCESS_LEVEL => 2,
        // kVstAutomationOff.
        AM_GET_AUTOMATION_STATE => 1,
        AM_GET_VENDOR_STRING => {
            write_c_string(ptr, "Jakub Hlavnicka", 64);
            1
        }
        AM_GET_PRODUCT_STRING => {
            write_c_string(ptr, "Gemstone DAW", 64);
            1
        }
        AM_GET_VENDOR_VERSION => 100,
        // kVstLangEnglish.
        AM_GET_LANGUAGE => 1,
        AM_CAN_DO => {
            if ptr.is_null() {
                return 0;
            }
            let what = CStr::from_ptr(ptr as *const c_char).to_string_lossy();
            match what.as_ref() {
                "sendVstEvents" | "sendVstMidiEvent" | "sendVstTimeInfo" | "receiveVstEvents"
                | "receiveVstMidiEvent" | "sizeWindow" | "supplyIdle" => 1,
                _ => 0,
            }
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// The library inside whatever was picked: the file itself, or the executable
/// inside a macOS `.vst` bundle.
fn binary_path(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let macos = path.join("Contents").join("MacOS");
    if macos.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&macos)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        let stem = path.file_stem().map(|s| s.to_os_string());
        if let Some(found) = files
            .iter()
            .find(|p| p.file_stem().map(|s| s.to_os_string()) == stem)
            .or_else(|| files.first())
        {
            return Ok(found.clone());
        }
    }
    bail!("{} is not a VST2 plugin library or bundle", crate::file_label(path))
}

/// The entry point, under whichever of its names the library exports.
unsafe fn entry_point(library: &Library) -> Option<PluginMain> {
    for name in [&b"VSTPluginMain\0"[..], b"main_macho\0", b"main\0"] {
        if let Ok(sym) = library.get::<PluginMain>(name) {
            return Some(*sym);
        }
    }
    None
}

/// Check that `path` is a library a VST2 can be loaded from, without running it.
pub fn validate(path: &Path) -> Result<()> {
    let binary = binary_path(path)?;
    let library = unsafe { Library::new(&binary) }
        .with_context(|| format!("Failed to open {}", crate::file_label(&binary)))?;
    anyhow::ensure!(
        unsafe { entry_point(&library) }.is_some(),
        "{} is not a VST2 plugin — it exports no VSTPluginMain",
        crate::file_label(path)
    );
    Ok(())
}

/// A loaded VST2 plugin instance.
pub struct Vst2Instance {
    effect: *mut AEffect,
    shared: Arc<HostShared>,
    name: String,
    io: RwLock<PluginIo>,
    active: AtomicBool,
    // Dropped last: `effClose` has to run while the code is still mapped.
    _library: Library,
}

// The dispatcher is called from the GUI thread, the editor's thread and the
// audio thread, as every VST2 host does; the effect is only touched while the
// instance is alive.
unsafe impl Send for Vst2Instance {}
unsafe impl Sync for Vst2Instance {}

impl Vst2Instance {
    pub fn load(path: &Path) -> Result<Self> {
        let binary = binary_path(path)?;
        let library = unsafe { Library::new(&binary) }
            .with_context(|| format!("Failed to open {}", crate::file_label(&binary)))?;
        let main = unsafe { entry_point(&library) }.with_context(|| {
            format!("{} is not a VST2 plugin — it exports no VSTPluginMain", crate::file_label(path))
        })?;
        let effect = unsafe { main(host_callback) };
        anyhow::ensure!(!effect.is_null(), "{} declined to create a plugin", crate::file_label(path));
        anyhow::ensure!(
            unsafe { (*effect).magic } == MAGIC,
            "{} returned something that is not a VST2 plugin",
            crate::file_label(path)
        );
        anyhow::ensure!(
            unsafe { (*effect).dispatcher.is_some() },
            "{} has no dispatcher",
            crate::file_label(path)
        );

        let shared = Arc::new(HostShared {
            sample_rate: Mutex::new(44_100.0),
            block_size: Mutex::new(512),
            time: UnsafeCell::new(VstTimeInfo::default()),
            resize_request: Mutex::new(None),
        });
        INSTANCES
            .write()
            .unwrap()
            .push((effect as usize, shared.clone()));

        let mut instance = Vst2Instance {
            effect,
            shared,
            name: String::new(),
            io: RwLock::new(PluginIo::default()),
            active: AtomicBool::new(false),
            _library: library,
        };
        instance.dispatch(EFF_OPEN, 0, 0, std::ptr::null_mut(), 0.0);
        instance.name = instance
            .string(EFF_GET_EFFECT_NAME)
            .or_else(|| instance.string(EFF_GET_PRODUCT_STRING))
            .unwrap_or_else(|| super::display_stem(path));
        let flags = unsafe { (*effect).flags };
        anyhow::ensure!(
            flags & FLAG_CAN_REPLACING != 0 || unsafe { (*effect).process_replacing.is_some() },
            "'{}' only offers the accumulating process call, which this host does not use",
            instance.name
        );
        log::info!(
            "Loaded VST2 '{}' from {} ({}{} in, {} out, {} params)",
            instance.name,
            path.display(),
            if flags & FLAG_IS_SYNTH != 0 { "synth, " } else { "" },
            unsafe { (*effect).num_inputs },
            unsafe { (*effect).num_outputs },
            unsafe { (*effect).num_params },
        );
        Ok(instance)
    }

    fn dispatch(&self, opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
        unsafe {
            match (*self.effect).dispatcher {
                Some(d) => d(self.effect, opcode, index, value, ptr, opt),
                None => 0,
            }
        }
    }

    /// A string the plugin writes into a buffer for `opcode`.
    fn string(&self, opcode: i32) -> Option<String> {
        // The SDK's limits are 32 or 64 bytes; plugins overrun them often
        // enough that the buffer is generous.
        let mut buf = [0u8; 256];
        self.dispatch(opcode, 0, 0, buf.as_mut_ptr() as *mut c_void, 0.0);
        let s = CStr::from_bytes_until_nul(&buf).ok()?.to_string_lossy().trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    fn flags(&self) -> i32 {
        unsafe { (*self.effect).flags }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn io(&self) -> PluginIo {
        self.io.read().unwrap().clone()
    }

    pub fn initialize_audio(&self, sample_rate: f64, max_block_size: i32) -> Result<()> {
        if self.active.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let max_block = max_block_size.max(1);
        *self.shared.sample_rate.lock().unwrap() = sample_rate;
        *self.shared.block_size.lock().unwrap() = max_block;
        self.dispatch(EFF_SET_SAMPLE_RATE, 0, 0, std::ptr::null_mut(), sample_rate as f32);
        self.dispatch(EFF_SET_BLOCK_SIZE, 0, max_block as isize, std::ptr::null_mut(), 0.0);
        // kVstProcessPrecision32.
        self.dispatch(EFF_SET_PROCESS_PRECISION, 0, 0, std::ptr::null_mut(), 0.0);
        self.dispatch(EFF_MAINS_CHANGED, 0, 1, std::ptr::null_mut(), 0.0);
        self.dispatch(EFF_START_PROCESS, 0, 0, std::ptr::null_mut(), 0.0);

        let (ins, outs) = unsafe { ((*self.effect).num_inputs, (*self.effect).num_outputs) };
        let io = PluginIo {
            // A VST2 has one flat list of channels each way; it is one bus.
            inputs: if ins > 0 { vec![ins as usize] } else { Vec::new() },
            outputs: if outs > 0 { vec![outs as usize] } else { Vec::new() },
            max_block: max_block as usize,
        };
        unsafe {
            let time = &mut *self.shared.time.get();
            time.sample_rate = sample_rate;
            time.tempo = 120.0;
            time.time_sig_numerator = 4;
            time.time_sig_denominator = 4;
            time.flags = TRANSPORT_PLAYING | TEMPO_VALID | PPQ_POS_VALID | TIME_SIG_VALID;
        }
        log::info!(
            "VST2 '{}' started: sr={sample_rate}, block={max_block}, in={:?}, out={:?}",
            self.name,
            io.inputs,
            io.outputs
        );
        *self.io.write().unwrap() = io;
        Ok(())
    }

    /// The plugin's own chunk where it keeps one, else every parameter's value.
    pub fn save_state(&self) -> Result<Vec<u8>> {
        if self.flags() & FLAG_PROGRAM_CHUNKS != 0 {
            let mut data: *mut c_void = std::ptr::null_mut();
            // Index 0: the whole bank, not one program.
            let len = self.dispatch(
                EFF_GET_CHUNK,
                0,
                0,
                &mut data as *mut *mut c_void as *mut c_void,
                0.0,
            );
            if len > 0 && !data.is_null() {
                let mut out = CHUNK_TAG.to_vec();
                out.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(data as *const u8, len as usize)
                });
                return Ok(out);
            }
        }
        let get = unsafe { (*self.effect).get_parameter }
            .with_context(|| format!("'{}' has no state to save", self.name))?;
        let count = unsafe { (*self.effect).num_params }.max(0);
        let mut out = PARAMS_TAG.to_vec();
        for i in 0..count {
            out.extend_from_slice(&unsafe { get(self.effect, i) }.to_le_bytes());
        }
        Ok(out)
    }

    pub fn restore_state(&self, bytes: &[u8]) -> Result<()> {
        if let Some(chunk) = bytes.strip_prefix(&CHUNK_TAG[..]) {
            self.dispatch(
                EFF_SET_CHUNK,
                0,
                chunk.len() as isize,
                chunk.as_ptr() as *mut c_void,
                0.0,
            );
            return Ok(());
        }
        if let Some(values) = bytes.strip_prefix(&PARAMS_TAG[..]) {
            let set = unsafe { (*self.effect).set_parameter }
                .with_context(|| format!("'{}' cannot take parameter values", self.name))?;
            let count = unsafe { (*self.effect).num_params }.max(0) as usize;
            for (i, v) in values.chunks_exact(4).take(count).enumerate() {
                let v = f32::from_le_bytes([v[0], v[1], v[2], v[3]]);
                unsafe { set(self.effect, i as i32, v) };
            }
            return Ok(());
        }
        bail!("'{}' was handed a state that is not a VST2 plugin's", self.name)
    }

    pub fn has_editor(&self) -> bool {
        self.flags() & FLAG_HAS_EDITOR != 0
    }

    pub fn create_editor(&self) -> Result<Box<dyn PluginEditor>> {
        anyhow::ensure!(self.has_editor(), "'{}' has no editor", self.name);
        Ok(Box::new(Vst2Editor {
            effect: self.effect,
            shared: self.shared.clone(),
            open: false,
        }))
    }

    pub(crate) fn realtime(
        &self,
        scratch: &mut AudioScratch,
        in_buses: &[usize],
        out_buses: &[usize],
    ) -> Result<Box<dyn RealtimeProcess>> {
        let process = unsafe { (*self.effect).process_replacing }
            .with_context(|| format!("'{}' has no processReplacing", self.name))?;
        let in_channels: usize = in_buses.iter().sum();
        let out_channels: usize = out_buses.iter().sum();
        let ptrs = scratch.ptrs_mut().to_vec();
        let mut events = Box::new(VstEvents {
            num_events: 0,
            reserved: 0,
            events: [std::ptr::null_mut(); MAX_EVENTS_PER_BLOCK],
        });
        let mut storage = vec![
            VstMidiEvent {
                type_: MIDI_TYPE,
                byte_size: std::mem::size_of::<VstMidiEvent>() as i32,
                delta_frames: 0,
                flags: 0,
                note_length: 0,
                note_offset: 0,
                midi_data: [0; 4],
                detune: 0,
                note_off_velocity: 0,
                reserved1: 0,
                reserved2: 0,
            };
            MAX_EVENTS_PER_BLOCK
        ];
        for (slot, ev) in events.events.iter_mut().zip(storage.iter_mut()) {
            *slot = ev as *mut VstMidiEvent;
        }
        Ok(Box::new(Vst2Realtime {
            effect: self.effect,
            process,
            inputs: ptrs[..in_channels].to_vec(),
            outputs: ptrs[in_channels..in_channels + out_channels].to_vec(),
            events,
            storage,
            shared: self.shared.clone(),
        }))
    }
}

impl Drop for Vst2Instance {
    fn drop(&mut self) {
        if self.active.swap(false, Ordering::SeqCst) {
            self.dispatch(EFF_STOP_PROCESS, 0, 0, std::ptr::null_mut(), 0.0);
            self.dispatch(EFF_MAINS_CHANGED, 0, 0, std::ptr::null_mut(), 0.0);
        }
        // The plugin frees itself here; the effect pointer is dead after it.
        self.dispatch(EFF_CLOSE, 0, 0, std::ptr::null_mut(), 0.0);
        if let Ok(mut instances) = INSTANCES.write() {
            instances.retain(|(e, _)| *e != self.effect as usize);
        }
    }
}

// ---------------------------------------------------------------------------
// Processing
// ---------------------------------------------------------------------------

struct Vst2Realtime {
    effect: *mut AEffect,
    process: ProcessProc,
    inputs: Vec<*mut f32>,
    outputs: Vec<*mut f32>,
    /// Points into `storage`; both boxed or heap-allocated so neither moves.
    events: Box<VstEvents>,
    storage: Vec<VstMidiEvent>,
    shared: Arc<HostShared>,
}

unsafe impl Send for Vst2Realtime {}

impl RealtimeProcess for Vst2Realtime {
    fn process(&mut self, scratch: &mut AudioScratch, frames: usize, events: &[MidiEvent]) {
        // The pointer table was re-derived by the scratch's reset; take it again.
        let ptrs = scratch.ptrs_mut();
        let n_in = self.inputs.len();
        self.inputs.copy_from_slice(&ptrs[..n_in]);
        let n_out = self.outputs.len();
        self.outputs.copy_from_slice(&ptrs[n_in..n_in + n_out]);

        let count = events.len().min(self.storage.len());
        for (slot, ev) in self.storage.iter_mut().zip(&events[..count]) {
            slot.delta_frames = ev.offset as i32;
            slot.midi_data = [ev.data[0], ev.data[1], ev.data[2], 0];
            slot.note_off_velocity = if ev.data[0] & 0xF0 == 0x80 { ev.data[2] as i8 } else { 0 };
        }
        unsafe {
            if count > 0 {
                self.events.num_events = count as i32;
                if let Some(d) = (*self.effect).dispatcher {
                    d(
                        self.effect,
                        EFF_PROCESS_EVENTS,
                        0,
                        0,
                        &mut *self.events as *mut VstEvents as *mut c_void,
                        0.0,
                    );
                }
            }
            (self.process)(
                self.effect,
                self.inputs.as_ptr() as *const *const f32,
                self.outputs.as_mut_ptr(),
                frames as i32,
            );
            let time = &mut *self.shared.time.get();
            time.sample_pos += frames as f64;
            time.ppq_pos = time.sample_pos / time.sample_rate.max(1.0) * time.tempo / 60.0;
        }
    }
}

// ---------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------

struct Vst2Editor {
    effect: *mut AEffect,
    shared: Arc<HostShared>,
    open: bool,
}

// Made on the GUI thread, then used only by the editor window's thread.
unsafe impl Send for Vst2Editor {}

impl Vst2Editor {
    fn dispatch(&self, opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
        unsafe {
            match (*self.effect).dispatcher {
                Some(d) => d(self.effect, opcode, index, value, ptr, opt),
                None => 0,
            }
        }
    }

    fn rect(&self) -> Option<(u32, u32)> {
        let mut rect: *mut ERect = std::ptr::null_mut();
        self.dispatch(
            EFF_EDIT_GET_RECT,
            0,
            0,
            &mut rect as *mut *mut ERect as *mut c_void,
            0.0,
        );
        let rect = unsafe { rect.as_ref()? };
        let (w, h) = ((rect.right - rect.left) as i32, (rect.bottom - rect.top) as i32);
        (w > 0 && h > 0).then_some((w as u32, h as u32))
    }
}

impl PluginEditor for Vst2Editor {
    fn size(&mut self) -> Option<(u32, u32)> {
        self.rect()
    }

    fn attach(&mut self, parent: ParentWindow) -> Result<()> {
        let before = self.rect();
        // On Linux the window id goes in `ptr` and the display in `value`.
        let (ptr, value) = match parent {
            ParentWindow::X11 { window, display } => (window as *mut c_void, display as isize),
            ParentWindow::Win32 { hwnd } => (hwnd, 0),
        };
        self.dispatch(EFF_EDIT_OPEN, 0, value, ptr, 0.0);
        self.open = true;
        // Plenty of plugins only know their size once the editor exists.
        if let Some(after) = self.rect() {
            if Some(after) != before {
                *self.shared.resize_request.lock().unwrap() = Some(after);
            }
        }
        Ok(())
    }

    fn take_resize_request(&mut self) -> Option<(u32, u32)> {
        self.shared.resize_request.lock().unwrap().take()
    }

    fn pump(&mut self, _ready_fds: &[i32]) {
        if self.open {
            self.dispatch(EFF_EDIT_IDLE, 0, 0, std::ptr::null_mut(), 0.0);
        }
    }

    fn detach(&mut self) {
        if std::mem::take(&mut self.open) {
            self.dispatch(EFF_EDIT_CLOSE, 0, 0, std::ptr::null_mut(), 0.0);
        }
    }
}

impl Drop for Vst2Editor {
    fn drop(&mut self) {
        self.detach();
    }
}
