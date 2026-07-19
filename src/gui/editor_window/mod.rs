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
//! Plugin editor window — hosts the VST3 plugin's own GUI in a native
//! top-level window and embeds the plugin view via the platform handle.
//!
//! Each backend exposes the same entry point:
//!
//! ```ignore
//! pub fn open_editor_in_thread(plugin: &PluginInstance) -> Result<EditorHandle>;
//! ```
//!
//! The returned [`EditorHandle`] carries two flags:
//!   * `close_flag` (host → thread): set it to `true` to ask the editor to close.
//!   * `closed` (thread → host): the thread sets it to `true` once the window has
//!     gone away — whether the host asked (via `close_flag`) or the user clicked
//!     the window's own close button. The host polls it each frame to reap the
//!     instance and free its resources (audio stream, plugin, thread).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;

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

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub use fallback::open_editor_in_thread;