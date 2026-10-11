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
//! Plugin editor window — hosts a plugin's own GUI, of any format, in a native
//! top-level window and embeds the plugin view via the platform handle.
//!
//! Every backend exposes `open_editor_in_thread(&PluginInstance) ->
//! Result<EditorHandle>`. The handle carries two flags: `close_flag` asks the
//! editor to close, and `closed` is set by the thread once the window has gone —
//! whether the host asked or the user clicked its own close button. The host
//! checks `closed` each frame to reap the instance and free its resources, and
//! [`signal_closed`] wakes it to do so — the GUI only draws when something happens.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

/// The host window's context, woken when an editor window closes.
static HOST_CTX: OnceLock<egui::Context> = OnceLock::new();

/// Remember the host window's context, so a closing editor can wake it.
pub fn set_host_context(ctx: &egui::Context) {
    let _ = HOST_CTX.set(ctx.clone());
}

/// Mark an editor window closed and wake the host to reap it. Without the wake
/// a window the user closed directly would linger as "open" until the next
/// input, so the host would have to poll for it.
pub(crate) fn signal_closed(closed: &AtomicBool) {
    closed.store(true, Ordering::Relaxed);
    if let Some(ctx) = HOST_CTX.get() {
        ctx.request_repaint();
    }
}

/// A running editor window and the flags used to coordinate its lifecycle.
pub struct EditorHandle {
    /// The window / event-loop thread.
    pub handle: JoinHandle<()>,
    /// Host → thread: set to `true` to ask the editor thread to close.
    pub close_flag: Arc<AtomicBool>,
    /// Thread → host: set to `true` once the window has closed, by whatever means.
    pub closed: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
pub use x11::open_editor_in_thread;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::open_editor_in_thread;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    open_editor_in_thread, remember_main_gl_context, request_close, restore_main_gl_context,
};

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub use fallback::open_editor_in_thread;

/// Ask the editor behind `close_flag` to close. Its own thread notices the flag
/// and detaches the view as it unwinds, so joining [`EditorHandle::handle`]
/// after this waits for exactly that. (macOS has no editor thread and does the
/// detaching inside this call instead.)
#[cfg(not(target_os = "macos"))]
pub fn request_close(close_flag: &Arc<AtomicBool>) {
    close_flag.store(true, std::sync::atomic::Ordering::Relaxed);
}