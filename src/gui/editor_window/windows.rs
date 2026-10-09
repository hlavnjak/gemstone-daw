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
//! The plugin editor window on Windows: a plain top-level window whose handle
//! is given to a [`PluginEditor`]. Windows plugins pump their GUI on the
//! thread's own message loop, so the loop below dispatches messages and pumps
//! the editor for the idle work some formats want (a VST2's `effEditIdle`, an
//! LV2 UI's idle interface).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;

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
use crate::plugin::{ParentWindow, PluginInstance};

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

/// The window size that gives a client area of `width × height`: the plugin
/// means its own area, so the frame has to be added on top.
unsafe fn frame_size(width: u32, height: u32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width.clamp(1, 8192) as i32,
        bottom: height.clamp(1, 8192) as i32,
    };
    AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, 0);
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// The window's client area, as the editor's size.
unsafe fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);
    (
        (client.right - client.left).max(1) as u32,
        (client.bottom - client.top).max(1) as u32,
    )
}

/// Open the plugin editor in a new thread using a raw Win32 window.
pub fn open_editor_in_thread(plugin: &PluginInstance) -> Result<EditorHandle> {
    // Made here, so a plugin with no GUI this platform can host says so to the
    // caller rather than to a log nobody reads.
    let mut editor = plugin.create_editor()?;
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

        if let Err(e) = editor.open() {
            log::error!("Plugin editor failed to open: {e:#}");
            return;
        }

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

        let (width, height) = editor
            .size()
            .map(|(w, h)| (w.clamp(64, 8192), h.clamp(64, 8192)))
            .unwrap_or((800, 600));
        let (frame_w, frame_h) = frame_size(width, height);

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            frame_w,
            frame_h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null_mut(),
        );

        if hwnd.is_null() {
            log::error!("Failed to create Win32 window");
            UnregisterClassW(class_name.as_ptr(), hinstance);
            return;
        }

        ShowWindow(hwnd, SW_SHOW);

        if let Err(e) = editor.attach(ParentWindow::Win32 { hwnd: hwnd as *mut c_void }) {
            log::error!("Plugin editor refused to attach: {e:#}");
            DestroyWindow(hwnd);
            UnregisterClassW(class_name.as_ptr(), hinstance);
            return;
        }

        // Size the plugin view to the window client area
        let mut size = client_size(hwnd);
        editor.set_size(size.0, size.1);
        log::info!("Plugin editor attached to Win32 window");

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
            editor.pump(&[]);

            // A size the plugin asked for while we were dispatching messages.
            if let Some((w, h)) = editor.take_resize_request() {
                let (frame_w, frame_h) = frame_size(w, h);
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    frame_w,
                    frame_h,
                    SWP_NOMOVE | SWP_NOZORDER,
                );
            }
            // The window changed size, by the plugin's asking or the user's hand.
            let now = client_size(hwnd);
            if now != size {
                size = now;
                editor.set_size(size.0, size.1);
            }

            std::thread::sleep(std::time::Duration::from_millis(editor.timeout_ms().clamp(1, 16) as u64));
        }

        // Cleanup: detach before the window goes, on the thread that drove it.
        editor.detach();
        drop(editor);
        DestroyWindow(hwnd);
        UnregisterClassW(class_name.as_ptr(), hinstance);
        log::info!("Plugin editor window closed");
    });

    Ok(EditorHandle {
        handle,
        close_flag,
        closed,
    })
}
