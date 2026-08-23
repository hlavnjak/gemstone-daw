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
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use vst3::Steinberg::{kInvalidArgument, kResultOk, IPlugFrame, IPlugFrameTrait, IPlugView,
    IPlugViewTrait, ViewRect};
use vst3::{Class, ComWrapper};

use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
use winapi::shared::windef::{HWND, RECT};
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::winuser::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, PeekMessageW, PostQuitMessage, RegisterClassW, SetWindowPos, ShowWindow,
    TranslateMessage, UnregisterClassW, CW_USEDEFAULT, MSG, PM_REMOVE, SWP_NOMOVE, SWP_NOZORDER,
    SW_SHOW, WM_DESTROY, WM_QUIT, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use super::EditorHandle;
use crate::vst::PluginInstance;

/// The host frame the plugin's view talks to. Windows plugins pump their GUI on
/// the thread's own message loop, so unlike X11 there is no run loop to provide —
/// but a plugin that wants a different size still has to be able to ask.
#[derive(Default)]
struct EditorFrame {
    pending_resize: Mutex<Option<ViewRect>>,
}

impl Class for EditorFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for EditorFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> i32 {
        let Some(rect) = new_size.as_ref() else {
            return kInvalidArgument;
        };
        *self.pending_resize.lock().unwrap() = Some(*rect);
        kResultOk
    }
}

/// Convert a Rust string to a NUL-terminated UTF-16 buffer for the Win32 W APIs.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Open the plugin editor in a new thread using a raw Win32 window.
pub fn open_editor_in_thread(plugin: &PluginInstance) -> Result<EditorHandle> {
    let view = plugin.create_view().context(
        "this plugin has no editor view (it reported no 'editor' GUI for the host to show)",
    )?;

    unsafe {
        let platform = b"HWND\0";
        anyhow::ensure!(
            view.as_com_ref()
                .isPlatformTypeSupported(platform.as_ptr() as *const i8)
                == kResultOk,
            "this plugin's editor does not support an HWND parent"
        );
    }

    let title = format!("{} — Editor", plugin.name());

    let close_flag = Arc::new(AtomicBool::new(false));
    let close_flag_clone = close_flag.clone();
    let closed = Arc::new(AtomicBool::new(false));
    let closed_clone = closed.clone();

    let handle = std::thread::spawn(move || unsafe {
        // Signal the host once this thread returns, no matter which path it took,
        // so a window closed by the user is reaped just like one closed by us.
        struct SignalClosed(Arc<AtomicBool>);
        impl Drop for SignalClosed {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _signal = SignalClosed(closed_clone);

        let class_name = to_wide("GemstoneDawEditorWindow");
        let window_title = to_wide(&title);
        let hinstance = GetModuleHandleW(std::ptr::null());

        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        // The editor's own size, as a window size (the plugin means its client
        // area, so the frame has to be added on top).
        let view_ref = view.as_com_ref();
        let mut size = ViewRect {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 800,
        };
        view_ref.getSize(&mut size);
        let mut frame_rect = RECT {
            left: 0,
            top: 0,
            right: (size.right - size.left).clamp(64, 8192),
            bottom: (size.bottom - size.top).clamp(64, 8192),
        };
        AdjustWindowRect(&mut frame_rect, WS_OVERLAPPEDWINDOW, 0);
        let width = frame_rect.right - frame_rect.left;
        let height = frame_rect.bottom - frame_rect.top;

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null_mut(),
        );

        if hwnd.is_null() {
            eprintln!("Failed to create Win32 window");
            UnregisterClassW(class_name.as_ptr(), hinstance);
            return;
        }

        ShowWindow(hwnd, SW_SHOW);

        // Attach plugin view to the HWND. The frame has to outlive the
        // attachment, so it is dropped only at the end of this thread.
        let frame = ComWrapper::new(EditorFrame::default());
        let frame_ptr = frame
            .as_com_ref::<IPlugFrame>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        view_ref.setFrame(frame_ptr);
        let platform = b"HWND\0";
        let attached = view_ref.attached(hwnd as *mut c_void, platform.as_ptr() as *const i8);
        if attached != kResultOk {
            view_ref.setFrame(std::ptr::null_mut());
            DestroyWindow(hwnd);
            UnregisterClassW(class_name.as_ptr(), hinstance);
            return;
        }

        // Size the plugin view to the window client area
        let mut client = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: client.right,
            bottom: client.bottom,
        };
        view_ref.onSize(&mut rect as *mut _);
        eprintln!("Plugin editor attached to Win32 window");

        // Event loop
        let mut msg: MSG = std::mem::zeroed();
        loop {
            if close_flag_clone.load(Ordering::Relaxed) {
                break;
            }

            let mut got_quit = false;
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) > 0 {
                if msg.message == WM_QUIT {
                    got_quit = true;
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if got_quit {
                break;
            }

            // A size the plugin asked for while we were dispatching messages.
            let pending = frame.pending_resize.lock().unwrap().take();
            if let Some(rect) = pending {
                let mut frame_rect = RECT {
                    left: 0,
                    top: 0,
                    right: (rect.right - rect.left).clamp(1, 8192),
                    bottom: (rect.bottom - rect.top).clamp(1, 8192),
                };
                AdjustWindowRect(&mut frame_rect, WS_OVERLAPPEDWINDOW, 0);
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    frame_rect.right - frame_rect.left,
                    frame_rect.bottom - frame_rect.top,
                    SWP_NOMOVE | SWP_NOZORDER,
                );
                let mut client = std::mem::zeroed();
                GetClientRect(hwnd, &mut client);
                let mut rect = ViewRect {
                    left: 0,
                    top: 0,
                    right: client.right,
                    bottom: client.bottom,
                };
                view_ref.onSize(&mut rect as *mut _);
            }

            std::thread::sleep(std::time::Duration::from_millis(16));
        }

        // Cleanup
        view_ref.removed();
        view_ref.setFrame(std::ptr::null_mut());
        DestroyWindow(hwnd);
        UnregisterClassW(class_name.as_ptr(), hinstance);
        eprintln!("Plugin editor window closed");
    });

    Ok(EditorHandle {
        handle,
        close_flag,
        closed,
    })
}