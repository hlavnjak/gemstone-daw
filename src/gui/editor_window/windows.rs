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
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use vst3::Steinberg::{IPlugViewTrait, ViewRect};

use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
use winapi::shared::windef::HWND;
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::winuser::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    PeekMessageW, PostQuitMessage, RegisterClassW, ShowWindow, TranslateMessage,
    UnregisterClassW, CW_USEDEFAULT, MSG, PM_REMOVE, SW_SHOW, WM_DESTROY, WM_QUIT,
    WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use super::EditorHandle;
use crate::vst::PluginInstance;

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
    let view = plugin
        .create_view()
        .context("Plugin has no editor view")?;

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
        let window_title = to_wide("LeSynth Fourier - Editor");
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

        let width = 1000i32;
        let height = 800i32;

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

        // Attach plugin view to the HWND
        let view_ref = view.as_com_ref();
        view_ref.setFrame(std::ptr::null_mut());
        let platform = b"HWND\0";
        view_ref.attached(hwnd as *mut c_void, platform.as_ptr() as *const i8);

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

            std::thread::sleep(std::time::Duration::from_millis(16));
        }

        // Cleanup
        view_ref.removed();
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