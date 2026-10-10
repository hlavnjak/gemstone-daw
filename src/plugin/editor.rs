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
//! A plugin's own GUI, as the editor window sees it.
//!
//! Every format embeds its editor the same way in outline — the host makes a
//! native window, hands the plugin its handle, and keeps the plugin's GUI work
//! pumped until the window closes — and differs in every detail: a VST3 view
//! wants an `IPlugFrame` and on Linux a run loop, a CLAP GUI registers timers
//! and file descriptors, a VST2 editor wants `effEditIdle`, an LV2 UI is a
//! separate library with an idle interface. [`PluginEditor`] is the outline;
//! the window code in [`crate::gui::editor_window`] drives it and never learns
//! which format it is talking to.

use std::ffi::c_void;

use anyhow::Result;

/// The native window a plugin editor is embedded into.
#[derive(Clone, Copy, Debug)]
pub enum ParentWindow {
    /// An X11 window id, and the `Display*` it lives on — VST2 editors on
    /// Linux are handed the display as well as the window.
    X11 { window: u64, display: *mut c_void },
    /// A Win32 `HWND`.
    Win32 { hwnd: *mut c_void },
    /// A macOS `NSView*`: the content view of the editor's window.
    Cocoa { view: *mut c_void },
}

/// One plugin editor, from creation to detachment.
///
/// Made on whatever thread asked for it, then moved to the editor window's own
/// thread, which makes every other call: [`open`](Self::open), then
/// [`size`](Self::size) and [`can_resize`](Self::can_resize) to shape the
/// window, [`attach`](Self::attach) once the window exists, and
/// [`pump`](Self::pump) on every turn of its event loop until
/// [`detach`](Self::detach). The plugin instance outlives it — the editor
/// window's thread is joined before the instance is dropped.
pub trait PluginEditor: Send {
    /// Create whatever GUI object the format makes before it has a parent.
    fn open(&mut self) -> Result<()> {
        Ok(())
    }

    /// The size the editor wants, in pixels, if it can say before it is attached.
    fn size(&mut self) -> Option<(u32, u32)>;

    /// Whether the user may resize the window. Most editors are a fixed size,
    /// and a window manager has to be told so.
    fn can_resize(&mut self) -> bool {
        false
    }

    /// Embed the editor in `parent`, which exists and is mapped.
    fn attach(&mut self, parent: ParentWindow) -> Result<()>;

    /// The window's client area is now `width × height`.
    fn set_size(&mut self, _width: u32, _height: u32) {}

    /// A size the plugin asked for since the last call, for the window to adopt.
    fn take_resize_request(&mut self) -> Option<(u32, u32)> {
        None
    }

    /// File descriptors the plugin wants watched (Linux), besides the window's
    /// own connection.
    fn poll_fds(&self) -> Vec<i32> {
        Vec::new()
    }

    /// How long the event loop may sleep before [`Self::pump`] is due again.
    fn timeout_ms(&self) -> i32 {
        16
    }

    /// Run the plugin's GUI work: the descriptors in `ready_fds` became
    /// readable, timers may be due, and an idle callback may want calling.
    fn pump(&mut self, _ready_fds: &[i32]) {}

    /// Take the editor out of the window, which is about to be destroyed.
    fn detach(&mut self);
}
