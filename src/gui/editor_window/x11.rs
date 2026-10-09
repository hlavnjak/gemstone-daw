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
//! The window is ours; what goes in it is the plugin's, through a
//! [`PluginEditor`] that hides which format it is. The loop below is the
//! plugin's whole GUI event loop as well as the window's — a Linux plugin has
//! none of its own — so it waits on the X connection *and* on every descriptor
//! the editor wants watched, wakes for the editor's nearest timer, and pumps
//! the editor on every turn. Without that a JUCE editor attaches, draws
//! nothing, and never answers a click.

use std::ffi::{c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use x11_dl::xlib;

use super::EditorHandle;
use crate::plugin::{ParentWindow, PluginInstance};

/// Open the plugin editor in a new thread using raw X11.
pub fn open_editor_in_thread(plugin: &PluginInstance) -> Result<EditorHandle> {
    // Made here, so a plugin with no GUI this platform can host says so to the
    // caller rather than to a log nobody reads.
    let mut editor = plugin.create_editor()?;

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

            // Whatever the format makes before it has a parent.
            if let Err(e) = editor.open() {
                log::error!("Plugin editor failed to open: {e:#}");
                (xlib.XCloseDisplay)(display);
                return;
            }

            // The editor's own idea of how big it is. Only fall back to a fixed
            // size if it will not say — a window sized to something else leaves a
            // JUCE editor letterboxed or cropped.
            let (mut width, mut height) = match editor.size() {
                Some((w, h)) => {
                    log::info!("Editor requested {w}x{h}");
                    (w.clamp(64, 8192), h.clamp(64, 8192))
                }
                None => (800, 600),
            };

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
            let resizable = editor.can_resize();
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

            let parent = ParentWindow::X11 {
                window: window as u64,
                display: display as *mut c_void,
            };
            if let Err(e) = editor.attach(parent) {
                log::error!("Plugin editor refused to attach: {e:#}");
                (xlib.XDestroyWindow)(display, window);
                (xlib.XCloseDisplay)(display);
                return;
            }
            editor.set_size(width, height);
            log::info!("Plugin editor attached to X11 window {window:#X}");

            let x_fd = (xlib.XConnectionNumber)(display);
            let mut event: xlib::XEvent = std::mem::zeroed();
            let mut running = true;
            let mut ready: Vec<i32> = Vec::new();

            while running && !close_flag_clone.load(Ordering::Relaxed) {
                // Wait on our own connection *and* every descriptor the plugin
                // registered, waking early enough for the nearest plugin timer.
                let plugin_fds = editor.poll_fds();
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
                    editor.timeout_ms(),
                );

                // The plugin's descriptors and timers first — that is its GUI
                // thread's work.
                ready.clear();
                ready.extend(
                    poll_fds
                        .iter()
                        .skip(1)
                        .filter(|pfd| pfd.revents != 0)
                        .map(|pfd| pfd.fd),
                );
                editor.pump(&ready);

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
                                editor.set_size(width, height);
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
                if let Some((w, h)) = editor.take_resize_request() {
                    let (w, h) = (w.clamp(1, 8192), h.clamp(1, 8192));
                    if w != width || h != height {
                        width = w;
                        height = h;
                        set_size_hints(w, h);
                        (xlib.XResizeWindow)(display, window, w, h);
                        (xlib.XFlush)(display);
                        editor.set_size(w, h);
                    }
                }
            }

            // Cleanup: detach the editor before the window goes, and drop it
            // here, on the thread that drove it.
            editor.detach();
            drop(editor);
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
