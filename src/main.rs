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
use gemstone_daw::gui::DawApp;

fn init_logging() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(log::LevelFilter::Info);

    let _ = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}][{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(level)
        .chain(std::io::stdout())
        .chain(fern::log_file("gemstone-daw.log").expect("failed to open log file"))
        .apply();
}

/// The main window's size when it first opens.
const INITIAL_SIZE: [f32; 2] = [900.0, 550.0];

/// How tall the desktop is, when running under Wine; `None` everywhere else.
///
/// Wine's OpenGL surface keeps the height the window had when the surface was
/// made. A window opened at 550 px and then made taller — by the user, by the
/// app, or by a tiling window manager the moment it maps — draws its top 550 px
/// and leaves everything under that black, however tall it gets. egui is laid
/// out at the full height the whole time; what it draws below the line just
/// never reaches the screen. Making the window *smaller* is always fine.
#[cfg(windows)]
fn wine_desktop_height() -> Option<f32> {
    use winapi::um::libloaderapi::{GetModuleHandleA, GetProcAddress};
    use winapi::um::winuser::{GetSystemMetrics, SM_CYVIRTUALSCREEN};

    // Wine's ntdll exports wine_get_version, and Windows' never has.
    unsafe {
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr().cast());
        if ntdll.is_null()
            || GetProcAddress(ntdll, b"wine_get_version\0".as_ptr().cast()).is_null()
        {
            return None;
        }
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        (height > 0).then_some(height as f32)
    }
}

#[cfg(not(windows))]
fn wine_desktop_height() -> Option<f32> {
    None
}

fn main() -> eframe::Result<()> {
    init_logging();

    // Under Wine the window is opened as tall as the desktop, so its surface is
    // made at a height no later size can exceed, and is brought down to its
    // real size once it exists.
    let wine_height = wine_desktop_height();
    let opening_size = match wine_height {
        Some(height) => [INITIAL_SIZE[0], height.max(INITIAL_SIZE[1])],
        None => INITIAL_SIZE,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(opening_size)
            .with_min_inner_size([700.0, 450.0])
            .with_title("Gemstone DAW")
            // Windows only: winit registers an OLE drop target per window, and
            // asserts that RegisterDragDrop succeeded. Under Wine that call
            // comes back E_NOINTERFACE and the process panics before the first
            // frame. Nothing here reads `dropped_files`, so the registration
            // buys us nothing on any platform — turning it off costs no feature
            // and makes the Windows build run under Wine as well as on Windows.
            .with_drag_and_drop(false),
        ..Default::default()
    };

    eframe::run_native(
        "Gemstone DAW",
        options,
        Box::new(move |cc| {
            // egui follows the system theme by default. Linux reports none and
            // gets dark; Windows — and Wine, whose registry says light — would
            // turn the whole DAW white. It is dark everywhere.
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            if wine_height.is_some() {
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::InnerSize(INITIAL_SIZE.into()));
            }
            Ok(Box::new(DawApp::new(cc)))
        }),
    )
}