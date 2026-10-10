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
//! The plugin editor window on macOS: an `NSWindow` whose content view is given
//! to a [`PluginEditor`].
//!
//! Unlike the Linux and Windows backends there is no editor thread. AppKit
//! windows can only be made and used on the main thread, and that thread is
//! already running the app's own event loop — which also delivers the plugin
//! view's events and runs its timers, so a plugin needs nothing pumped beyond
//! that. Every open editor sits in a main-thread list instead, and a run-loop
//! timer does what the other backends' loops do: pumps the editor for formats
//! that want idle calls, applies resizes, and notices a window the user closed.
//!
//! [`EditorHandle::handle`] is a thread that has already finished, so joining it
//! never waits. That leaves no thread to detach the view on the way out, which is
//! what [`request_close`] is for: called on the main thread, it detaches the view
//! and closes the window before returning, so the plugin can be dropped right
//! after.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use super::EditorHandle;
use crate::plugin::{ParentWindow, PluginEditor, PluginInstance};

/// How often the timer runs, in seconds — the 16 ms the other backends poll at.
const TICK_SECS: f64 = 0.016;

/// One open editor window.
struct OpenEditor {
    window: Retained<NSWindow>,
    editor: Box<dyn PluginEditor>,
    /// The content size the editor was last told about.
    size: (u32, u32),
    close_flag: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl OpenEditor {
    /// Detach the view, then close the window, then tell the host.
    fn close(mut self) {
        self.editor.detach();
        drop(self.editor);
        self.window.close();
        self.closed.store(true, Ordering::Relaxed);
        log::info!("Plugin editor window closed");
    }
}

thread_local! {
    /// Every open editor. Main thread only, like the windows in it.
    static EDITORS: RefCell<Vec<OpenEditor>> = const { RefCell::new(Vec::new()) };
    /// The timer driving them, while there are any.
    static TIMER: RefCell<Option<CFRunLoopTimerRef>> = const { RefCell::new(None) };
}

/// Open the plugin editor in a new `NSWindow`. Must be called on the main thread.
pub fn open_editor_in_thread(plugin: &PluginInstance) -> Result<EditorHandle> {
    let mtm = MainThreadMarker::new()
        .context("a plugin editor window can only be opened on the main thread on macOS")?;
    // Made here, so a plugin with no GUI this platform can host says so to the
    // caller rather than to a log nobody reads.
    let mut editor = plugin.create_editor()?;
    editor.open().context("the plugin editor failed to open")?;

    let (width, height) = editor
        .size()
        .map(|(w, h)| (w.clamp(64, 8192), h.clamp(64, 8192)))
        .unwrap_or((800, 600));
    let mut style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    if editor.can_resize() {
        style |= NSWindowStyleMask::Resizable;
    }
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width as f64, height as f64));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect,
            style,
            NSBackingStoreType::NSBackingStoreBuffered,
            false,
        )
    };
    // The list below owns the window; AppKit must not free it on close as well.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&format!("{} — Editor", plugin.name())));
    window.center();
    window.makeKeyAndOrderFront(None);

    let view = window.contentView().context("the editor window has no content view")?;
    let view_ptr = Retained::as_ptr(&view) as *mut c_void;
    if let Err(e) = editor.attach(ParentWindow::Cocoa { view: view_ptr }) {
        window.close();
        return Err(e.context("the plugin editor refused to attach"));
    }
    let size = content_size(&window);
    editor.set_size(size.0, size.1);
    log::info!("Plugin editor attached to NSWindow");

    let close_flag = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    EDITORS.with(|e| {
        e.borrow_mut().push(OpenEditor {
            window,
            editor,
            size,
            close_flag: close_flag.clone(),
            closed: closed.clone(),
        })
    });
    start_timer();

    Ok(EditorHandle {
        handle: std::thread::spawn(|| {}),
        close_flag,
        closed,
    })
}

/// Ask the editor behind `close_flag` to close. On the main thread the view is
/// detached and the window closed before this returns; anywhere else the timer
/// does it on its next turn.
pub fn request_close(close_flag: &Arc<AtomicBool>) {
    close_flag.store(true, Ordering::Relaxed);
    if MainThreadMarker::new().is_none() {
        return;
    }
    let found = EDITORS.with(|e| {
        let mut list = e.borrow_mut();
        let i = list.iter().position(|o| Arc::ptr_eq(&o.close_flag, close_flag))?;
        Some(list.swap_remove(i))
    });
    // Outside the borrow: detaching calls into the plugin.
    if let Some(open) = found {
        open.close();
    }
    stop_timer_if_idle();
}

/// The window's content area, in points — the unit a macOS plugin view sizes in.
fn content_size(window: &NSWindow) -> (u32, u32) {
    let frame = window
        .contentView()
        .map(|v| v.frame())
        .unwrap_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)));
    (
        frame.size.width.round().max(1.0) as u32,
        frame.size.height.round().max(1.0) as u32,
    )
}

/// One turn of what the other backends' event loops do, for every editor.
fn tick() {
    // Taken out of the list while the plugins are called, so a plugin calling
    // back into the host cannot find the list borrowed.
    let editors = EDITORS.with(|e| std::mem::take(&mut *e.borrow_mut()));
    let mut keep = Vec::with_capacity(editors.len());
    for mut open in editors {
        // Asked to close, or closed by the user with the window's own button
        // (a minimised window is not visible either, but is still open).
        let gone = !open.window.isVisible() && !open.window.isMiniaturized();
        if open.close_flag.load(Ordering::Relaxed) || gone {
            open.close();
            continue;
        }
        open.editor.pump(&[]);
        if let Some((w, h)) = open.editor.take_resize_request() {
            open.window.setContentSize(NSSize::new(w as f64, h as f64));
        }
        let now = content_size(&open.window);
        if now != open.size {
            open.size = now;
            open.editor.set_size(now.0, now.1);
        }
        keep.push(open);
    }
    EDITORS.with(|e| {
        let mut list = e.borrow_mut();
        // Anything opened while the plugins were being called goes after.
        keep.append(&mut list);
        *list = keep;
    });
    stop_timer_if_idle();
}

// ── the run-loop timer ──────────────────────────────────────────────────────

type CFRunLoopTimerRef = *mut c_void;

#[repr(C)]
struct CFRunLoopTimerContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: *const c_void;
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFAbsoluteTimeGetCurrent() -> f64;
    fn CFRunLoopTimerCreate(
        allocator: *const c_void,
        fire_date: f64,
        interval: f64,
        flags: usize,
        order: isize,
        callout: extern "C" fn(CFRunLoopTimerRef, *mut c_void),
        context: *mut CFRunLoopTimerContext,
    ) -> CFRunLoopTimerRef;
    fn CFRunLoopAddTimer(run_loop: *mut c_void, timer: CFRunLoopTimerRef, mode: *const c_void);
    fn CFRunLoopTimerInvalidate(timer: CFRunLoopTimerRef);
    fn CFRelease(cf: *mut c_void);
}

extern "C" fn on_timer(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    // A panic must not unwind into CoreFoundation.
    if std::panic::catch_unwind(tick).is_err() {
        log::error!("plugin editor timer panicked");
    }
}

/// Start the timer if it is not running. In the common modes, so it keeps
/// running while a window is being dragged or resized, or a menu is open.
fn start_timer() {
    TIMER.with(|t| {
        let mut timer = t.borrow_mut();
        if timer.is_some() {
            return;
        }
        unsafe {
            let mut context = CFRunLoopTimerContext {
                version: 0,
                info: std::ptr::null_mut(),
                retain: std::ptr::null(),
                release: std::ptr::null(),
                copy_description: std::ptr::null(),
            };
            let created = CFRunLoopTimerCreate(
                std::ptr::null(),
                CFAbsoluteTimeGetCurrent() + TICK_SECS,
                TICK_SECS,
                0,
                0,
                on_timer,
                &mut context,
            );
            if created.is_null() {
                log::error!("could not create the plugin editor timer");
                return;
            }
            CFRunLoopAddTimer(CFRunLoopGetMain(), created, kCFRunLoopCommonModes);
            *timer = Some(created);
        }
    });
}

/// Stop the timer once no editor is open, so an idle app does no work for it.
fn stop_timer_if_idle() {
    if EDITORS.with(|e| !e.borrow().is_empty()) {
        return;
    }
    TIMER.with(|t| {
        if let Some(timer) = t.borrow_mut().take() {
            unsafe {
                CFRunLoopTimerInvalidate(timer);
                CFRelease(timer);
            }
        }
    });
}
