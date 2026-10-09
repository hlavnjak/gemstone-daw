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
//! A VST3 editor view, as a [`PluginEditor`].
//!
//! A VST3 editor on Linux is not a window the host can simply park somewhere: the
//! plugin has no event loop of its own. The spec makes the *host* provide one —
//! the object it passes to `IPlugView::setFrame` is expected to answer a
//! `queryInterface` for `Linux::IRunLoop`, and the plugin then registers its file
//! descriptors and timers with it. That is exactly what [`EditorFrame`] is, and
//! why the editor window polls the plugin's descriptors instead of only its own:
//! without it a JUCE or Steinberg-SDK editor attaches, draws nothing, and never
//! responds to a click, because none of its events are ever pumped.
//!
//! The same object is also the `IPlugFrame` a plugin calls `resizeView` on, so a
//! plugin that wants a different size gets one. Windows plugins pump their GUI
//! on the thread's own message loop, so there the frame is only that.

use std::ffi::{c_void, CStr};
use std::sync::Mutex;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
#[cfg(target_os = "linux")]
use vst3::Steinberg::Linux::{
    FileDescriptor, IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler,
    ITimerHandlerTrait, TimerInterval,
};
use vst3::Steinberg::{
    kInvalidArgument, kResultOk, IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, ViewRect,
};
#[cfg(target_os = "linux")]
use vst3::ComRef;
use vst3::{Class, ComPtr, ComWrapper};

use super::Vst3Instance;
use crate::plugin::{ParentWindow, PluginEditor};

/// How long the loop may sleep before it looks at its own flags again.
const MAX_POLL_MS: i32 = 16;

/// The platform type a view is asked to embed into here.
#[cfg(target_os = "windows")]
const PLATFORM: &[u8] = b"HWND\0";
#[cfg(not(target_os = "windows"))]
const PLATFORM: &[u8] = b"X11EmbedWindowID\0";

/// The host object the plugin's view talks to: its frame *and*, on Linux, its
/// run loop.
#[derive(Default)]
struct EditorFrame {
    /// `(IEventHandler*, fd)` the plugin asked us to watch. Pointers are held as
    /// `usize` so this stays `Send`; they are only ever used on the editor thread.
    #[cfg(target_os = "linux")]
    handlers: Mutex<Vec<(usize, FileDescriptor)>>,
    #[cfg(target_os = "linux")]
    timers: Mutex<Vec<Timer>>,
    /// A size the plugin asked for, applied by the event loop.
    pending_resize: Mutex<Option<ViewRect>>,
}

#[cfg(target_os = "linux")]
struct Timer {
    handler: usize,
    interval: Duration,
    next: Instant,
}

#[cfg(target_os = "linux")]
impl Class for EditorFrame {
    type Interfaces = (IPlugFrame, IRunLoop);
}

#[cfg(not(target_os = "linux"))]
impl Class for EditorFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for EditorFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> i32 {
        let Some(rect) = new_size.as_ref() else {
            return kInvalidArgument;
        };
        // Do not resize from in here: the plugin is inside its own call stack and
        // will be told the new size by the loop, right after the window has it.
        *self.pending_resize.lock().unwrap() = Some(*rect);
        kResultOk
    }
}

#[cfg(target_os = "linux")]
impl IRunLoopTrait for EditorFrame {
    unsafe fn registerEventHandler(&self, handler: *mut IEventHandler, fd: FileDescriptor) -> i32 {
        if handler.is_null() {
            return kInvalidArgument;
        }
        self.handlers.lock().unwrap().push((handler as usize, fd));
        kResultOk
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> i32 {
        self.handlers
            .lock()
            .unwrap()
            .retain(|(h, _)| *h != handler as usize);
        kResultOk
    }

    unsafe fn registerTimer(&self, handler: *mut ITimerHandler, milliseconds: TimerInterval) -> i32 {
        if handler.is_null() {
            return kInvalidArgument;
        }
        // A zero interval means "as often as you can"; clamp it so one plugin
        // cannot spin the editor thread.
        let interval = Duration::from_millis(milliseconds.max(1));
        self.timers.lock().unwrap().push(Timer {
            handler: handler as usize,
            interval,
            next: Instant::now() + interval,
        });
        kResultOk
    }

    unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> i32 {
        self.timers
            .lock()
            .unwrap()
            .retain(|t| t.handler != handler as usize);
        kResultOk
    }
}

#[cfg(target_os = "linux")]
impl EditorFrame {
    /// Hand a ready descriptor to the plugin. Re-checks registration first: a
    /// handler dispatched a moment ago may have unregistered this one.
    fn dispatch_fd(&self, fd: FileDescriptor) {
        let handler = self
            .handlers
            .lock()
            .unwrap()
            .iter()
            .find(|(_, f)| *f == fd)
            .map(|(h, _)| *h);
        if let Some(h) = handler {
            unsafe {
                if let Some(r) = ComRef::<IEventHandler>::from_raw(h as *mut IEventHandler) {
                    r.onFDIsSet(fd);
                }
            }
        }
    }

    /// Fire every timer that is due. The deadlines are advanced before the
    /// callbacks run, so a slow callback cannot make the loop fire back-to-back.
    fn dispatch_timers(&self) {
        let now = Instant::now();
        let due: Vec<usize> = {
            let mut timers = self.timers.lock().unwrap();
            let mut due = Vec::new();
            for timer in timers.iter_mut() {
                if timer.next <= now {
                    timer.next = now + timer.interval;
                    due.push(timer.handler);
                }
            }
            due
        };
        for handler in due {
            // Still registered? A previous callback may have dropped it.
            if !self.timers.lock().unwrap().iter().any(|t| t.handler == handler) {
                continue;
            }
            unsafe {
                if let Some(r) = ComRef::<ITimerHandler>::from_raw(handler as *mut ITimerHandler) {
                    r.onTimer();
                }
            }
        }
    }
}

/// A VST3 plugin's view, and the frame it was given.
struct Vst3Editor {
    view: ComPtr<IPlugView>,
    /// Doubles as the plugin's run loop, so it has to outlive the attachment.
    frame: ComWrapper<EditorFrame>,
    attached: bool,
}

// The view is created on the GUI thread and from then on used only by the
// editor window's thread, which is what the editor window is for.
unsafe impl Send for Vst3Editor {}

/// The plugin's editor, if it has one that can embed into this platform's
/// windows.
pub fn create(plugin: &Vst3Instance) -> Result<Box<dyn PluginEditor>> {
    let view = plugin.create_view().context(
        "this plugin has no editor view (it reported no 'editor' GUI for the host to show)",
    )?;
    // Ask before attaching: a plugin with, say, only a Wayland or a NSView GUI
    // would otherwise be handed a window it cannot use.
    let platform = CStr::from_bytes_with_nul(PLATFORM).unwrap();
    let supported = unsafe { view.as_com_ref().isPlatformTypeSupported(platform.as_ptr()) };
    if supported != kResultOk {
        bail!(
            "this plugin's editor does not support {} embedding",
            platform.to_string_lossy()
        );
    }
    Ok(Box::new(Vst3Editor {
        view,
        frame: ComWrapper::new(EditorFrame::default()),
        attached: false,
    }))
}

impl PluginEditor for Vst3Editor {
    fn size(&mut self) -> Option<(u32, u32)> {
        let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        let ok = unsafe { self.view.as_com_ref().getSize(&mut rect) } == kResultOk;
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        (ok && w > 0 && h > 0).then_some((w as u32, h as u32))
    }

    fn can_resize(&mut self) -> bool {
        unsafe { self.view.as_com_ref().canResize() == kResultOk }
    }

    fn attach(&mut self, parent: ParentWindow) -> Result<()> {
        let handle = match parent {
            ParentWindow::X11 { window, .. } => window as *mut c_void,
            ParentWindow::Win32 { hwnd } => hwnd,
        };
        let view = self.view.as_com_ref();
        let frame_ptr = self
            .frame
            .as_com_ref::<IPlugFrame>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        unsafe {
            view.setFrame(frame_ptr);
            let platform = CStr::from_bytes_with_nul(PLATFORM).unwrap();
            let attached = view.attached(handle, platform.as_ptr());
            if attached != kResultOk {
                view.setFrame(std::ptr::null_mut());
                bail!("the plugin editor refused to attach ({attached:#X})");
            }
        }
        self.attached = true;
        Ok(())
    }

    fn set_size(&mut self, width: u32, height: u32) {
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe {
            self.view.as_com_ref().onSize(&mut rect);
        }
    }

    fn take_resize_request(&mut self) -> Option<(u32, u32)> {
        let rect = self.frame.pending_resize.lock().unwrap().take()?;
        Some((
            (rect.right - rect.left).clamp(1, 8192) as u32,
            (rect.bottom - rect.top).clamp(1, 8192) as u32,
        ))
    }

    #[cfg(target_os = "linux")]
    fn poll_fds(&self) -> Vec<i32> {
        self.frame
            .handlers
            .lock()
            .unwrap()
            .iter()
            .map(|(_, fd)| *fd)
            .collect()
    }

    fn timeout_ms(&self) -> i32 {
        #[cfg(target_os = "linux")]
        {
            let now = Instant::now();
            let nearest = self
                .frame
                .timers
                .lock()
                .unwrap()
                .iter()
                .map(|t| t.next.saturating_duration_since(now))
                .min();
            if let Some(d) = nearest {
                return (d.as_millis() as i32).clamp(0, MAX_POLL_MS);
            }
        }
        MAX_POLL_MS
    }

    fn pump(&mut self, _ready_fds: &[i32]) {
        #[cfg(target_os = "linux")]
        {
            for &fd in _ready_fds {
                self.frame.dispatch_fd(fd);
            }
            self.frame.dispatch_timers();
        }
    }

    fn detach(&mut self) {
        if !std::mem::take(&mut self.attached) {
            return;
        }
        // Detach the view before the window goes, and clear the frame so the
        // plugin cannot call back into an object about to be dropped.
        unsafe {
            let view = self.view.as_com_ref();
            view.removed();
            view.setFrame(std::ptr::null_mut());
        }
    }
}

impl Drop for Vst3Editor {
    fn drop(&mut self) {
        self.detach();
    }
}
