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
use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use vst3::Steinberg::{IPlugViewTrait, ViewRect};
use x11_dl::xlib;

use crate::vst::PluginInstance;

/// Open the plugin editor in a new thread using raw X11.
/// Returns a handle to stop the editor (set the bool to true to close).
pub fn open_editor_in_thread(
    plugin: &PluginInstance,
) -> Result<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> {
    let view = plugin
        .create_view()
        .context("Plugin has no editor view")?;

    let close_flag = Arc::new(AtomicBool::new(false));
    let close_flag_clone = close_flag.clone();

    let handle = std::thread::spawn(move || {
        unsafe {
            let xlib = xlib::Xlib::open().expect("Failed to open Xlib");

            let display = (xlib.XOpenDisplay)(std::ptr::null());
            if display.is_null() {
                eprintln!("Failed to open X11 display");
                return;
            }

            let screen = (xlib.XDefaultScreen)(display);
            let root = (xlib.XRootWindow)(display, screen);

            let width = 1000u32;
            let height = 800u32;

            let window = (xlib.XCreateSimpleWindow)(
                display,
                root,
                0,
                0,
                width,
                height,
                1,
                (xlib.XBlackPixel)(display, screen),
                (xlib.XWhitePixel)(display, screen),
            );

            // Set window title
            let title = CStr::from_bytes_with_nul(b"LeSynth Fourier - Editor\0").unwrap();
            (xlib.XStoreName)(display, window, title.as_ptr() as *mut _);

            // Subscribe to events
            (xlib.XSelectInput)(
                display,
                window,
                xlib::ExposureMask
                    | xlib::StructureNotifyMask
                    | xlib::KeyPressMask,
            );

            // Handle WM_DELETE_WINDOW
            let wm_delete = (xlib.XInternAtom)(
                display,
                CStr::from_bytes_with_nul(b"WM_DELETE_WINDOW\0")
                    .unwrap()
                    .as_ptr() as *mut _,
                0,
            );
            (xlib.XSetWMProtocols)(display, window, &mut wm_delete.clone(), 1);

            // Show window
            (xlib.XMapWindow)(display, window);
            (xlib.XFlush)(display);

            // Attach plugin view
            let view_ref = view.as_com_ref();
            view_ref.setFrame(std::ptr::null_mut());
            let platform = CStr::from_bytes_with_nul(b"X11EmbedWindowID\0").unwrap();
            view_ref.attached(window as *mut c_void, platform.as_ptr());

            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            view_ref.onSize(&mut rect as *mut _);
            eprintln!("Plugin editor attached to X11 window");

            // Event loop
            let mut event: xlib::XEvent = std::mem::zeroed();
            loop {
                if close_flag_clone.load(Ordering::Relaxed) {
                    break;
                }

                // Non-blocking event check
                while (xlib.XPending)(display) > 0 {
                    (xlib.XNextEvent)(display, &mut event);

                    match event.get_type() {
                        xlib::ConfigureNotify => {
                            let configure = event.configure;
                            let mut rect = ViewRect {
                                left: 0,
                                top: 0,
                                right: configure.width,
                                bottom: configure.height,
                            };
                            view_ref.onSize(&mut rect as *mut _);
                        }
                        xlib::ClientMessage => {
                            let client = event.client_message;
                            if client.data.get_long(0) as u64 == wm_delete {
                                // Window close requested
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                // Check if we broke out of inner loop due to close
                if event.get_type() == xlib::ClientMessage {
                    let client = event.client_message;
                    if client.data.get_long(0) as u64 == wm_delete {
                        break;
                    }
                }

                std::thread::sleep(std::time::Duration::from_millis(16));
            }

            // Cleanup
            view_ref.removed();
            (xlib.XDestroyWindow)(display, window);
            (xlib.XCloseDisplay)(display);
            eprintln!("Plugin editor window closed");
        }
    });

    Ok((handle, close_flag))
}