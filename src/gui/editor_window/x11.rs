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
//! The plugin editor window on X11.
//!
//! A VST3 editor on Linux is not a window the host can simply park somewhere: the
//! plugin has no event loop of its own. The spec makes the *host* provide one —
//! the object it passes to `IPlugView::setFrame` is expected to answer a
//! `queryInterface` for `Linux::IRunLoop`, and the plugin then registers its file
//! descriptors and timers with it. That is exactly what [`EditorFrame`] is, and
//! why the loop below polls the plugin's descriptors instead of only its own:
//! without it a JUCE or Steinberg-SDK editor attaches, draws nothing, and never
//! responds to a click, because none of its events are ever pumped.
//!
//! The same object is also the `IPlugFrame` a plugin calls `resizeView` on, so a
//! plugin that wants a different size gets one.

use std::ffi::{c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use vst3::Steinberg::Linux::{
    FileDescriptor, IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler,
    ITimerHandlerTrait, TimerInterval,
};
use vst3::Steinberg::{
    kInvalidArgument, kResultOk, IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, ViewRect,
};
use vst3::{Class, ComRef, ComWrapper};
use x11_dl::xlib;

use super::EditorHandle;
use crate::vst::PluginInstance;

/// How long the loop may sleep before it looks at `close_flag` again.
const MAX_POLL_MS: i32 = 16;

/// The host object the plugin's view talks to: its frame *and* its run loop.
#[derive(Default)]
struct EditorFrame {
    /// `(IEventHandler*, fd)` the plugin asked us to watch. Pointers are held as
    /// `usize` so this stays `Send`; they are only ever used on the editor thread.
    handlers: Mutex<Vec<(usize, FileDescriptor)>>,
    timers: Mutex<Vec<Timer>>,
    /// A size the plugin asked for, applied by the event loop.
    pending_resize: Mutex<Option<ViewRect>>,
}

struct Timer {
    handler: usize,
    interval: Duration,
    next: Instant,
}

impl Class for EditorFrame {
    type Interfaces = (IPlugFrame, IRunLoop);
}

impl IPlugFrameTrait for EditorFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> i32 {
        let Some(rect) = new_size.as_ref() else {
            return kInvalidArgument;
        };
        // Do not resize from in here: the plugin is inside its own call stack and
        // will be told the new size by the loop, right after the X server has it.
        *self.pending_resize.lock().unwrap() = Some(*rect);
        kResultOk
    }
}

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

impl EditorFrame {
    /// The descriptors to poll, alongside our own X connection.
    fn watched_fds(&self) -> Vec<FileDescriptor> {
        self.handlers.lock().unwrap().iter().map(|(_, fd)| *fd).collect()
    }

    /// How long the loop may block: the nearest timer, capped.
    fn poll_timeout_ms(&self) -> i32 {
        let now = Instant::now();
        let nearest = self
            .timers
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.next.saturating_duration_since(now))
            .min();
        match nearest {
            Some(d) => (d.as_millis() as i32).clamp(0, MAX_POLL_MS),
            None => MAX_POLL_MS,
        }
    }

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

    fn take_pending_resize(&self) -> Option<ViewRect> {
        self.pending_resize.lock().unwrap().take()
    }
}

/// Open the plugin editor in a new thread using raw X11.
pub fn open_editor_in_thread(plugin: &PluginInstance) -> Result<EditorHandle> {
    let view = plugin.create_view().context(
        "this plugin has no editor view (it reported no 'editor' GUI for the host to show)",
    )?;

    // Ask before attaching: a plugin with, say, only a Wayland or a NSView GUI
    // would otherwise be handed a window id it cannot use.
    unsafe {
        let platform = CStr::from_bytes_with_nul(b"X11EmbedWindowID\0").unwrap();
        let supported = view.as_com_ref().isPlatformTypeSupported(platform.as_ptr());
        anyhow::ensure!(
            supported == kResultOk,
            "this plugin's editor does not support X11 embedding"
        );
    }

    let title = CString::new(format!("{} — Editor", plugin.name()))
        .unwrap_or_else(|_| CString::new("Plugin Editor").unwrap());

    let close_flag = Arc::new(AtomicBool::new(false));
    let close_flag_clone = close_flag.clone();
    let closed = Arc::new(AtomicBool::new(false));
    let closed_clone = closed.clone();

    let handle = std::thread::spawn(move || {
        // Signal the host once this thread returns, no matter which path it took,
        // so a window closed by the user is reaped just like one closed by us.
        struct SignalClosed(Arc<AtomicBool>);
        impl Drop for SignalClosed {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _signal = SignalClosed(closed_clone);

        unsafe {
            let xlib = match xlib::Xlib::open() {
                Ok(x) => x,
                Err(e) => {
                    log::error!("Failed to open Xlib: {e}");
                    return;
                }
            };

            let display = (xlib.XOpenDisplay)(std::ptr::null());
            if display.is_null() {
                log::error!("Failed to open X11 display");
                return;
            }

            let screen = (xlib.XDefaultScreen)(display);
            let root = (xlib.XRootWindow)(display, screen);

            let view_ref = view.as_com_ref();

            // The editor's own idea of how big it is. Only fall back to a fixed
            // size if it will not say — a window sized to something else leaves a
            // JUCE editor letterboxed or cropped.
            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: 1000,
                bottom: 800,
            };
            if view_ref.getSize(&mut rect) == kResultOk {
                log::info!(
                    "Editor requested {}x{}",
                    rect.right - rect.left,
                    rect.bottom - rect.top
                );
            }
            let mut width = (rect.right - rect.left).clamp(64, 8192) as u32;
            let mut height = (rect.bottom - rect.top).clamp(64, 8192) as u32;

            let window = (xlib.XCreateSimpleWindow)(
                display,
                root,
                0,
                0,
                width,
                height,
                0,
                (xlib.XBlackPixel)(display, screen),
                (xlib.XBlackPixel)(display, screen),
            );

            (xlib.XStoreName)(display, window, title.as_ptr() as *mut _);

            // Most editors are a fixed size. Say so, or a window manager that
            // sizes windows itself (a tiling one, say) leaves the editor drawn
            // small in the corner of a window it never asked for.
            let resizable = view_ref.canResize() == kResultOk;
            let set_size_hints = |w: u32, h: u32| {
                let mut hints: xlib::XSizeHints = std::mem::zeroed();
                hints.flags = xlib::PMinSize | xlib::PBaseSize;
                hints.base_width = w as i32;
                hints.base_height = h as i32;
                hints.min_width = if resizable { 64 } else { w as i32 };
                hints.min_height = if resizable { 64 } else { h as i32 };
                if !resizable {
                    hints.flags |= xlib::PMaxSize;
                    hints.max_width = w as i32;
                    hints.max_height = h as i32;
                }
                (xlib.XSetWMNormalHints)(display, window, &mut hints);
            };
            set_size_hints(width, height);

            // Subscribe to events
            (xlib.XSelectInput)(
                display,
                window,
                xlib::ExposureMask | xlib::StructureNotifyMask | xlib::FocusChangeMask,
            );

            // Handle WM_DELETE_WINDOW
            let mut wm_delete = (xlib.XInternAtom)(
                display,
                CStr::from_bytes_with_nul(b"WM_DELETE_WINDOW\0")
                    .unwrap()
                    .as_ptr() as *mut _,
                0,
            );
            (xlib.XSetWMProtocols)(display, window, &mut wm_delete, 1);

            // Show the window and make sure the server knows about it *before* the
            // plugin reparents its own window into it.
            (xlib.XMapWindow)(display, window);
            (xlib.XSync)(display, 0);

            // The frame doubles as the plugin's run loop, so it has to outlive the
            // attachment; it is dropped at the end of this scope, after `removed`.
            let frame = ComWrapper::new(EditorFrame::default());
            let frame_ptr = frame
                .as_com_ref::<IPlugFrame>()
                .map(|r| r.as_ptr())
                .unwrap_or(std::ptr::null_mut());
            view_ref.setFrame(frame_ptr);

            let platform = CStr::from_bytes_with_nul(b"X11EmbedWindowID\0").unwrap();
            let attached = view_ref.attached(window as *mut c_void, platform.as_ptr());
            if attached != kResultOk {
                log::error!("Plugin editor refused to attach ({attached:#X})");
                view_ref.setFrame(std::ptr::null_mut());
                (xlib.XDestroyWindow)(display, window);
                (xlib.XCloseDisplay)(display);
                return;
            }

            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            view_ref.onSize(&mut rect);
            log::info!("Plugin editor attached to X11 window {window:#X}");

            let x_fd = (xlib.XConnectionNumber)(display);
            let mut event: xlib::XEvent = std::mem::zeroed();
            let mut running = true;

            while running && !close_flag_clone.load(Ordering::Relaxed) {
                // Wait on our own connection *and* every descriptor the plugin
                // registered, waking early enough for the nearest plugin timer.
                let plugin_fds = frame.watched_fds();
                let mut poll_fds: Vec<libc::pollfd> = std::iter::once(x_fd)
                    .chain(plugin_fds.iter().copied())
                    .map(|fd| libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    })
                    .collect();
                // Anything queued locally must go out before we block.
                (xlib.XFlush)(display);
                libc::poll(
                    poll_fds.as_mut_ptr(),
                    poll_fds.len() as libc::nfds_t,
                    frame.poll_timeout_ms(),
                );

                // The plugin's descriptors first — that is its GUI thread's work.
                for pfd in poll_fds.iter().skip(1) {
                    if pfd.revents != 0 {
                        frame.dispatch_fd(pfd.fd);
                    }
                }
                frame.dispatch_timers();

                while (xlib.XPending)(display) > 0 {
                    (xlib.XNextEvent)(display, &mut event);
                    match event.get_type() {
                        xlib::ConfigureNotify => {
                            let configure = event.configure;
                            if configure.width as u32 != width
                                || configure.height as u32 != height
                            {
                                width = configure.width as u32;
                                height = configure.height as u32;
                                let mut rect = ViewRect {
                                    left: 0,
                                    top: 0,
                                    right: configure.width,
                                    bottom: configure.height,
                                };
                                view_ref.onSize(&mut rect);
                            }
                        }
                        xlib::ClientMessage => {
                            let client = event.client_message;
                            if client.data.get_long(0) as u64 == wm_delete {
                                running = false;
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                // A size the plugin asked for while we were dispatching.
                if let Some(rect) = frame.take_pending_resize() {
                    let w = (rect.right - rect.left).clamp(1, 8192) as u32;
                    let h = (rect.bottom - rect.top).clamp(1, 8192) as u32;
                    if w != width || h != height {
                        width = w;
                        height = h;
                        set_size_hints(w, h);
                        (xlib.XResizeWindow)(display, window, w, h);
                        (xlib.XFlush)(display);
                        let mut rect = ViewRect {
                            left: 0,
                            top: 0,
                            right: w as i32,
                            bottom: h as i32,
                        };
                        view_ref.onSize(&mut rect);
                    }
                }
            }

            // Cleanup: detach the view before the window goes, and clear the frame
            // so the plugin cannot call back into an object we are about to drop.
            view_ref.removed();
            view_ref.setFrame(std::ptr::null_mut());
            (xlib.XDestroyWindow)(display, window);
            (xlib.XCloseDisplay)(display);
            log::info!("Plugin editor window closed");
        }
    });

    Ok(EditorHandle {
        handle,
        close_flag,
        closed,
    })
}
