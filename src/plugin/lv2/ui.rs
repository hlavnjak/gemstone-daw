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
//! An LV2 plugin's editor: a UI library of its own, embedded with `ui:parent`.
//!
//! An LV2 UI is a separate object from the plugin — often a separate library —
//! and talks to it the way a remote control would: it writes control port
//! values and atom messages through a function the host gives it, and is told
//! about port values through `port_event`. Many UIs also ask for direct access
//! to the plugin instance (`instance-access`), which JUCE's and DPF's both use
//! when it is offered. Its event loop is the host's, driven through the idle
//! interface.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use libloading::Library;

use super::{
    log_printf, log_vprintf, urid, urid_map, urid_unmap, ExtensionDataFn, LogLog, Lv2Feature,
    OptionsOption, PluginInfo, PortKind, UiInfo, UridMap, UridUnmap, ATOM_FLOAT, LOG_LOG,
    OPTIONS_OPTIONS, PARAM_SAMPLE_RATE, UI_NS, URID_MAP, URID_UNMAP,
};
use crate::plugin::{ParentWindow, PluginEditor};

/// The UI class this platform can embed.
#[cfg(target_os = "windows")]
const UI_CLASS: &str = "http://lv2plug.in/ns/extensions/ui#WindowsUI";
#[cfg(target_os = "macos")]
const UI_CLASS: &str = "http://lv2plug.in/ns/extensions/ui#CocoaUI";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const UI_CLASS: &str = "http://lv2plug.in/ns/extensions/ui#X11UI";

const UI_PARENT: &str = "http://lv2plug.in/ns/extensions/ui#parent";
const UI_RESIZE: &str = "http://lv2plug.in/ns/extensions/ui#resize";
const UI_IDLE_INTERFACE: &str = "http://lv2plug.in/ns/extensions/ui#idleInterface";
const UI_EVENT_TRANSFER: &str = "http://lv2plug.in/ns/extensions/ui#eventTransfer";
const ATOM_EVENT_TRANSFER: &str = "http://lv2plug.in/ns/ext/atom#eventTransfer";
const ATOM_ATOM_TRANSFER: &str = "http://lv2plug.in/ns/ext/atom#atomTransfer";
const INSTANCE_ACCESS: &str = "http://lv2plug.in/ns/ext/instance-access";
const DATA_ACCESS: &str = "http://lv2plug.in/ns/ext/data-access";

type WriteFn = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void);

#[repr(C)]
struct UiDescriptor {
    uri: *const c_char,
    instantiate: Option<
        unsafe extern "C" fn(
            *const UiDescriptor,
            *const c_char,
            *const c_char,
            WriteFn,
            *mut c_void,
            *mut *mut c_void,
            *const *const Lv2Feature,
        ) -> *mut c_void,
    >,
    cleanup: Option<unsafe extern "C" fn(*mut c_void)>,
    port_event: Option<unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void)>,
    extension_data: Option<ExtensionDataFn>,
}

type UiDescriptorFn = unsafe extern "C" fn(u32) -> *const UiDescriptor;

#[repr(C)]
struct UiResize {
    handle: *mut c_void,
    ui_resize: unsafe extern "C" fn(*mut c_void, i32, i32) -> i32,
}

#[repr(C)]
struct UiIdleInterface {
    idle: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
}

#[repr(C)]
struct ExtensionDataFeature {
    data_access: Option<ExtensionDataFn>,
}

/// The UI this platform can show, if the plugin has one.
pub(super) fn pick_ui(info: &PluginInfo) -> Option<&UiInfo> {
    info.uis.iter().find(|ui| ui.class == UI_CLASS && ui.binary.is_file())
}

/// What the UI's calls into the host reach: the plugin's control values and
/// the queue of atoms for its next block. The `controller` of every
/// `write_function` call.
struct Controller {
    controls: Arc<[AtomicU32]>,
    input_controls: Vec<u32>,
    ui_events: Arc<Mutex<Vec<(u32, u32, Vec<u8>)>>>,
    atom_protocols: [u32; 3],
}

unsafe extern "C" fn ui_write(
    controller: *mut c_void,
    port: u32,
    size: u32,
    protocol: u32,
    buffer: *const c_void,
) {
    let Some(c) = (controller as *const Controller).as_ref() else { return };
    if buffer.is_null() {
        return;
    }
    if protocol == 0 {
        // A float for a control port.
        if size as usize == 4 && c.input_controls.contains(&port) {
            let value = *(buffer as *const f32);
            if let Some(slot) = c.controls.get(port as usize) {
                slot.store(value.to_bits(), Ordering::Relaxed);
            }
        }
    } else if c.atom_protocols.contains(&protocol) && size >= 8 {
        // An atom — `{size, type}` and its body — for an atom input.
        let atom_size = *(buffer as *const u32) as usize;
        let type_ = *(buffer as *const u32).add(1);
        let body_len = atom_size.min(size as usize - 8);
        let body = std::slice::from_raw_parts((buffer as *const u8).add(8), body_len).to_vec();
        if let Ok(mut queue) = c.ui_events.lock() {
            queue.push((port, type_, body));
        }
    }
}

/// Where the UI's own resize requests are left for the window to pick up.
#[derive(Default)]
struct ResizeRequest(Mutex<Option<(u32, u32)>>);

unsafe extern "C" fn host_ui_resize(handle: *mut c_void, width: i32, height: i32) -> i32 {
    let Some(r) = (handle as *const ResizeRequest).as_ref() else { return 1 };
    if width > 0 && height > 0 {
        *r.0.lock().unwrap() = Some((width as u32, height as u32));
    }
    0
}

/// An LV2 UI, not yet or already instantiated.
struct Lv2Editor {
    info: Arc<PluginInfo>,
    ui: UiInfo,
    plugin_handle: *mut c_void,
    sample_rate: Box<f32>,
    controller: Box<Controller>,
    resize: Box<ResizeRequest>,
    host_resize: Box<UiResize>,
    map: Box<UridMap>,
    unmap: Box<UridUnmap>,
    log: Box<LogLog>,
    data_access: Box<ExtensionDataFeature>,
    options: Vec<OptionsOption>,
    /// Values last told to the UI, so only changes are sent.
    sent: Vec<f32>,
    descriptor: *const UiDescriptor,
    handle: *mut c_void,
    idle: *const UiIdleInterface,
    ui_resize: *const UiResize,
    // Dropped last.
    library: Option<Library>,
}

// Made on the GUI thread, then used only by the editor window's thread.
unsafe impl Send for Lv2Editor {}

pub(super) fn create(
    info: Arc<PluginInfo>,
    plugin_handle: *mut c_void,
    plugin_extension_data: Option<ExtensionDataFn>,
    controls: Arc<[AtomicU32]>,
    ui_events: Arc<Mutex<Vec<(u32, u32, Vec<u8>)>>>,
    sample_rate: f32,
) -> Result<Box<dyn PluginEditor>> {
    let ui = pick_ui(&info)
        .cloned()
        .with_context(|| {
            let kind = UI_CLASS.strip_prefix(UI_NS).unwrap_or(UI_CLASS);
            format!("'{}' has no {kind} editor", info.name)
        })?;
    let input_controls = info
        .ports
        .iter()
        .filter(|p| p.kind == PortKind::Control && p.input)
        .map(|p| p.index)
        .collect();
    let sent = vec![f32::NAN; controls.len()];
    let resize = Box::<ResizeRequest>::default();
    let host_resize = Box::new(UiResize {
        handle: &*resize as *const ResizeRequest as *mut c_void,
        ui_resize: host_ui_resize,
    });
    Ok(Box::new(Lv2Editor {
        info,
        ui,
        plugin_handle,
        sample_rate: Box::new(sample_rate),
        controller: Box::new(Controller {
            controls,
            input_controls,
            ui_events,
            atom_protocols: [urid(UI_EVENT_TRANSFER), urid(ATOM_EVENT_TRANSFER), urid(ATOM_ATOM_TRANSFER)],
        }),
        resize,
        host_resize,
        map: Box::new(UridMap { handle: std::ptr::null_mut(), map: urid_map }),
        unmap: Box::new(UridUnmap { handle: std::ptr::null_mut(), unmap: urid_unmap }),
        log: Box::new(LogLog { handle: std::ptr::null_mut(), printf: log_printf, vprintf: log_vprintf }),
        data_access: Box::new(ExtensionDataFeature { data_access: plugin_extension_data }),
        options: Vec::new(),
        sent,
        descriptor: std::ptr::null(),
        handle: std::ptr::null_mut(),
        idle: std::ptr::null(),
        ui_resize: std::ptr::null(),
        library: None,
    }))
}

impl Lv2Editor {
    /// Tell the UI about every control value that changed since it was last
    /// told — all of them, the first time.
    fn send_port_values(&mut self) {
        let Some(port_event) = (unsafe { self.descriptor.as_ref() }).and_then(|d| d.port_event) else {
            return;
        };
        for port in self.info.ports.iter().filter(|p| p.kind == PortKind::Control) {
            let i = port.index as usize;
            let value = f32::from_bits(self.controller.controls[i].load(Ordering::Relaxed));
            if value.to_bits() != self.sent[i].to_bits() {
                self.sent[i] = value;
                unsafe {
                    port_event(self.handle, port.index, 4, 0, &value as *const f32 as *const c_void);
                }
            }
        }
    }
}

impl PluginEditor for Lv2Editor {
    fn size(&mut self) -> Option<(u32, u32)> {
        // An LV2 UI has no size until it exists; it says what it wants through
        // `ui:resize` as it is made.
        None
    }

    fn attach(&mut self, parent: ParentWindow) -> Result<()> {
        let library = unsafe { Library::new(&self.ui.binary) }
            .with_context(|| format!("Failed to open {}", crate::file_label(&self.ui.binary)))?;
        let descriptor = unsafe {
            let entry = library
                .get::<UiDescriptorFn>(b"lv2ui_descriptor\0")
                .with_context(|| format!("{} exports no lv2ui_descriptor", crate::file_label(&self.ui.binary)))?;
            let mut found = std::ptr::null();
            for i in 0.. {
                let d = entry(i);
                if d.is_null() {
                    break;
                }
                if !(*d).uri.is_null() && CStr::from_ptr((*d).uri).to_string_lossy() == self.ui.uri {
                    found = d;
                    break;
                }
            }
            found
        };
        if descriptor.is_null() {
            bail!("{} does not contain the UI <{}>", crate::file_label(&self.ui.binary), self.ui.uri);
        }
        let instantiate = unsafe { (*descriptor).instantiate }.context("UI has no instantiate")?;

        let parent_ptr = match parent {
            ParentWindow::X11 { window, .. } => window as usize as *mut c_void,
            ParentWindow::Win32 { hwnd } => hwnd,
        };
        self.options = vec![
            OptionsOption {
                context: 0,
                subject: 0,
                key: urid(PARAM_SAMPLE_RATE),
                size: 4,
                type_: urid(ATOM_FLOAT),
                value: &*self.sample_rate as *const f32 as *const c_void,
            },
            OptionsOption { context: 0, subject: 0, key: 0, size: 0, type_: 0, value: std::ptr::null() },
        ];
        let entries: Vec<(&str, *mut c_void)> = vec![
            (UI_PARENT, parent_ptr),
            (UI_RESIZE, &*self.host_resize as *const UiResize as *mut c_void),
            (UI_IDLE_INTERFACE, std::ptr::null_mut()),
            (URID_MAP, &*self.map as *const UridMap as *mut c_void),
            (URID_UNMAP, &*self.unmap as *const UridUnmap as *mut c_void),
            (OPTIONS_OPTIONS, self.options.as_ptr() as *mut c_void),
            (LOG_LOG, &*self.log as *const LogLog as *mut c_void),
        ];
        let mut entries = entries;
        // Direct access only for a UI that names it — see `UiInfo::features`.
        let wants = |f: &str| self.ui.features.iter().any(|x| x == f);
        if wants(INSTANCE_ACCESS) {
            entries.push((INSTANCE_ACCESS, self.plugin_handle));
        }
        if wants(DATA_ACCESS) {
            entries.push((DATA_ACCESS, &*self.data_access as *const ExtensionDataFeature as *mut c_void));
        }
        let uris: Vec<CString> = entries.iter().map(|(u, _)| CString::new(*u).unwrap()).collect();
        let features: Vec<Lv2Feature> = entries
            .iter()
            .zip(&uris)
            .map(|((_, data), uri)| Lv2Feature { uri: uri.as_ptr(), data: *data })
            .collect();
        let list: Vec<*const Lv2Feature> = features
            .iter()
            .map(|f| f as *const Lv2Feature)
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        let plugin_uri = CString::new(self.info.uri.clone()).context("plugin URI contains a NUL byte")?;
        let mut bundle = self
            .ui
            .binary
            .parent()
            .unwrap_or(&self.info.bundle)
            .to_string_lossy()
            .into_owned();
        if !bundle.ends_with(std::path::MAIN_SEPARATOR) {
            bundle.push(std::path::MAIN_SEPARATOR);
        }
        let bundle = CString::new(bundle).context("bundle path contains a NUL byte")?;
        let mut widget: *mut c_void = std::ptr::null_mut();
        let handle = unsafe {
            instantiate(
                descriptor,
                plugin_uri.as_ptr(),
                bundle.as_ptr(),
                ui_write,
                &*self.controller as *const Controller as *mut c_void,
                &mut widget,
                list.as_ptr(),
            )
        };
        anyhow::ensure!(!handle.is_null(), "the plugin's UI refused to instantiate");
        self.descriptor = descriptor;
        self.handle = handle;
        self.library = Some(library);

        let ext = |uri: &str| -> *const c_void {
            let c = CString::new(uri).unwrap();
            match unsafe { (*descriptor).extension_data } {
                Some(f) => unsafe { f(c.as_ptr()) },
                None => std::ptr::null(),
            }
        };
        self.idle = ext(UI_IDLE_INTERFACE) as *const UiIdleInterface;
        self.ui_resize = ext(UI_RESIZE) as *const UiResize;
        self.send_port_values();

        // A UI that never said how big it is: ask the window system instead.
        #[cfg(target_os = "linux")]
        if self.resize.0.lock().unwrap().is_none() {
            if let ParentWindow::X11 { display, .. } = parent {
                if let Some(size) = unsafe { x11_window_size(display, widget as usize as u64) } {
                    *self.resize.0.lock().unwrap() = Some(size);
                }
            }
        }
        Ok(())
    }

    fn set_size(&mut self, width: u32, height: u32) {
        if let Some(r) = unsafe { self.ui_resize.as_ref() } {
            unsafe { (r.ui_resize)(self.handle, width as i32, height as i32) };
        }
    }

    fn take_resize_request(&mut self) -> Option<(u32, u32)> {
        self.resize.0.lock().unwrap().take()
    }

    fn pump(&mut self, _ready_fds: &[i32]) {
        if self.handle.is_null() {
            return;
        }
        self.send_port_values();
        if let Some(idle) = unsafe { self.idle.as_ref() }.and_then(|i| i.idle) {
            // Non-zero means the UI would like to be closed; an embedded one
            // closes with its window instead.
            unsafe { idle(self.handle) };
        }
    }

    fn detach(&mut self) {
        if self.handle.is_null() {
            return;
        }
        unsafe {
            if let Some(cleanup) = (*self.descriptor).cleanup {
                cleanup(self.handle);
            }
        }
        self.handle = std::ptr::null_mut();
    }
}

impl Drop for Lv2Editor {
    fn drop(&mut self) {
        self.detach();
    }
}

/// The size of an X11 window, for a UI that made one without saying how big.
#[cfg(target_os = "linux")]
unsafe fn x11_window_size(display: *mut c_void, window: u64) -> Option<(u32, u32)> {
    if display.is_null() || window == 0 {
        return None;
    }
    let xlib = x11_dl::xlib::Xlib::open().ok()?;
    let mut attrs: x11_dl::xlib::XWindowAttributes = std::mem::zeroed();
    if (xlib.XGetWindowAttributes)(display as *mut _, window, &mut attrs) == 0 {
        return None;
    }
    (attrs.width > 1 && attrs.height > 1).then_some((attrs.width as u32, attrs.height as u32))
}
