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
//! Loading a VST3 **module** — the shared library behind a plugin — the way the
//! spec asks a host to.
//!
//! Two things here are easy to get wrong and both make a perfectly good
//! third-party plugin look broken:
//!
//!   * **A VST3 is a bundle, not a file.** What a user installs is a directory
//!     `Foo.vst3/` holding `Contents/x86_64-linux/Foo.so`. Only our own internal
//!     plugin is ever a bare `.so`, so [`resolve_module_path`] accepts either and
//!     digs the real library out of a bundle.
//!   * **`ModuleEntry` must be called.** On Linux the entry point is not
//!     `GetPluginFactory` alone: the host must call `ModuleEntry(handle)` after
//!     `dlopen` and `ModuleExit()` before `dlclose`. JUCE initialises its whole
//!     runtime there, so a JUCE plugin whose `ModuleEntry` was skipped hands back
//!     a factory that crashes or an editor that never draws.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use libloading::Library;
use vst3::Steinberg::{
    kResultOk, IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait,
    PClassInfo, PClassInfo2,
};
use vst3::{ComPtr, ComRef};

type GetPluginFactoryProc = unsafe extern "C" fn() -> *mut IPluginFactory;
/// `bool ModuleEntry(void* sharedLibraryHandle)` — Linux; the handle is the
/// `dlopen` one, which SDK plugins use to find their own bundle's resources.
type ModuleEntryProc = unsafe extern "C" fn(*mut c_void) -> bool;
type ModuleExitProc = unsafe extern "C" fn() -> bool;
/// Windows' spelling of the same pair (no handle argument).
#[cfg(target_os = "windows")]
type InitDllProc = unsafe extern "C" fn() -> bool;

/// Architecture subdirectories of `Contents/`, in the order we try them.
#[cfg(target_os = "linux")]
const ARCH_DIRS: &[&str] = &["x86_64-linux", "aarch64-linux", "armv7l-linux", "i386-linux"];
#[cfg(target_os = "windows")]
const ARCH_DIRS: &[&str] = &["x86_64-win", "arm64-win", "x86-win"];
#[cfg(target_os = "macos")]
const ARCH_DIRS: &[&str] = &["MacOS"];
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
const ARCH_DIRS: &[&str] = &[];

/// Extensions a module inside a bundle may carry. Windows bundles name the DLL
/// `.vst3`; a loose `.dll` is accepted too, as is a loose `.so` on Linux.
#[cfg(target_os = "linux")]
const MODULE_EXTS: &[&str] = &["so"];
#[cfg(target_os = "windows")]
const MODULE_EXTS: &[&str] = &["vst3", "dll"];
#[cfg(target_os = "macos")]
const MODULE_EXTS: &[&str] = &["dylib", "so"];
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
const MODULE_EXTS: &[&str] = &["so"];

/// Turn whatever the user picked into the shared library to load.
///
/// Accepts a bare library file (our internal plugin, or a loose `.so`), a
/// `.vst3` bundle directory, or any level inside one (`Contents`, or the
/// architecture directory itself) — a file dialog makes all four easy to land on.
pub fn resolve_module_path(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if !path.exists() {
        bail!("no such file or directory: {}", path.display());
    }

    // A bundle, or something inside one. Walk down as far as we recognise.
    let contents = if path.join("Contents").is_dir() {
        path.join("Contents")
    } else {
        path.to_path_buf()
    };
    for arch in ARCH_DIRS {
        let dir = contents.join(arch);
        if dir.is_dir() {
            if let Some(found) = pick_module_in(&dir, path) {
                return Ok(found);
            }
        }
    }
    // The user may have picked the architecture directory itself.
    if let Some(found) = pick_module_in(path, path) {
        return Ok(found);
    }

    bail!(
        "{} is not a VST3 bundle — expected {}/Contents/{}/*.{} inside it",
        path.display(),
        path.file_name().unwrap_or_default().to_string_lossy(),
        ARCH_DIRS.first().copied().unwrap_or("<arch>"),
        MODULE_EXTS.first().copied().unwrap_or("so"),
    );
}

/// The library file in `dir`, preferring one named after the bundle (bundles may
/// also carry helper libraries beside the module).
fn pick_module_in(dir: &Path, bundle: &Path) -> Option<PathBuf> {
    let wanted = bundle.file_stem().map(|s| s.to_ascii_lowercase());
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.extension()
                .map(|e| MODULE_EXTS.iter().any(|m| e.eq_ignore_ascii_case(m)))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates
        .iter()
        .find(|p| p.file_stem().map(|s| s.to_ascii_lowercase()) == wanted)
        .or_else(|| candidates.first())
        .cloned()
}

/// Check that `path` really is a loadable VST3, without starting the plugin up.
///
/// This is what the Tracks panel calls the moment a plugin is picked, so the two
/// ways a choice can be wrong — a library the loader cannot satisfy, and a file
/// that is not a VST3 at all — are reported there and then rather than surfacing
/// much later as "Open editor failed". Returns the resolved module path.
pub fn validate_module(path: &Path) -> Result<PathBuf> {
    let resolved = resolve_module_path(path)?;
    let (library, _handle) = open_library(&resolved)
        .with_context(|| format!("Failed to open {}", resolved.display()))?;
    // Deliberately no `ModuleEntry`: this only asks whether the entry point is
    // there, so nothing of the plugin actually runs.
    unsafe {
        if library
            .get::<GetPluginFactoryProc>(b"GetPluginFactory\0")
            .is_ok()
        {
            return Ok(resolved);
        }
        Err(not_a_vst3(&library, &resolved))
    }
}

/// What a factory says about one of its classes.
#[derive(Clone, Debug, Default)]
pub struct ModuleClass {
    pub cid: [i8; 16],
    /// The plugin's own display name.
    pub name: String,
    /// `Audio Module Class` for something that makes sound.
    pub category: String,
    /// `Instrument|Drum`, `Fx|Delay`, … — the plugin's declaration of what it
    /// *is*, and the only precise way to know a drum kit from a synth. Empty
    /// when the factory is too old to be asked (`IPluginFactory2`).
    pub subcategories: String,
}

/// Ask a plugin what it is, without creating an instance of it.
///
/// This is a scan, not a load: the module is opened, its factory read, and the
/// module closed again. It costs one `dlopen` and the plugin's `ModuleEntry`,
/// which is what any host's plugin scan costs.
pub fn scan_classes(path: &Path) -> Result<Vec<ModuleClass>> {
    let module = Vst3Module::open(path)?;
    let factory = module.factory()?;
    Ok(classes(factory.as_com_ref()))
}

/// Every class in a factory, with the richer `getClassInfo2` fields where the
/// factory supports them.
pub fn classes(factory: ComRef<'_, IPluginFactory>) -> Vec<ModuleClass> {
    unsafe {
        let factory2 = factory.cast::<IPluginFactory2>();
        (0..factory.countClasses())
            .filter_map(|idx| {
                if let Some(f2) = &factory2 {
                    let mut info: PClassInfo2 = std::mem::zeroed();
                    if f2.as_com_ref().getClassInfo2(idx, &mut info) == kResultOk {
                        return Some(ModuleClass {
                            cid: info.cid,
                            name: c_string(&info.name),
                            category: c_string(&info.category),
                            subcategories: c_string(&info.subCategories),
                        });
                    }
                }
                let mut info: PClassInfo = std::mem::zeroed();
                if factory.getClassInfo(idx, &mut info) == kResultOk {
                    return Some(ModuleClass {
                        cid: info.cid,
                        name: c_string(&info.name),
                        category: c_string(&info.category),
                        subcategories: String::new(),
                    });
                }
                None
            })
            .collect()
    }
}

/// A NUL-terminated `char8` array from class info as a Rust string.
fn c_string(bytes: &[i8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes[..end].iter().map(|&b| b as u8 as char).collect()
}

/// A loaded VST3 module: the library, kept alive, plus the `ModuleEntry` /
/// `ModuleExit` pair the spec puts around its lifetime.
pub struct Vst3Module {
    library: Library,
    /// The raw `dlopen` handle, which is what `ModuleEntry` wants. `libloading`
    /// only surfaces it by consuming the library, so it is captured at load.
    handle: *mut c_void,
    /// The resolved library path (not the bundle the user picked).
    path: PathBuf,
    /// Whether `ModuleEntry` succeeded, so `Drop` knows to call `ModuleExit`.
    entered: bool,
}

// The library and everything reached through it is used from the GUI thread, the
// editor thread and the audio thread alike; `Library` itself is `Send + Sync`.
unsafe impl Send for Vst3Module {}
unsafe impl Sync for Vst3Module {}

impl Vst3Module {
    /// Load the module at `path` (a bundle or a bare library) and run its entry
    /// point.
    pub fn open(path: &Path) -> Result<Self> {
        let resolved = resolve_module_path(path)
            .with_context(|| format!("Cannot load {}", path.display()))?;

        // Match the reference host: RTLD_LAZY | RTLD_LOCAL. Binding lazily keeps
        // a plugin that carries unresolved host-API symbols (several JUCE builds
        // do) loadable, and LOCAL stops its symbols leaking into ours.
        let (library, handle) = open_library(&resolved)
            .with_context(|| format!("Failed to open {}", resolved.display()))?;

        let mut module = Vst3Module {
            library,
            handle,
            path: resolved,
            entered: false,
        };
        module.enter()?;
        module.check_is_vst3()?;
        Ok(module)
    }

    /// The resolved library path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The library, for the LeSynth-specific C ABI symbols in `host.rs`.
    pub fn library(&self) -> &Library {
        &self.library
    }

    /// The plugin's factory. Every call re-enters `GetPluginFactory`, which the
    /// spec requires to hand back the same singleton with a fresh reference.
    pub fn factory(&self) -> Result<ComPtr<IPluginFactory>> {
        unsafe {
            let get_factory: libloading::Symbol<GetPluginFactoryProc> = self
                .library
                .get(b"GetPluginFactory\0")
                .context("not a VST3 module: no GetPluginFactory symbol")?;
            ComPtr::from_raw(get_factory()).context("GetPluginFactory returned null")
        }
    }

    /// Call `ModuleEntry`/`InitDll`. Absent is not an error — plugins built
    /// without one (including ours) simply have nothing to initialise — but a
    /// present one returning `false` means the plugin refused to start.
    fn enter(&mut self) -> Result<()> {
        unsafe {
            #[cfg(target_os = "windows")]
            if let Ok(init) = self.library.get::<InitDllProc>(b"InitDll\0") {
                anyhow::ensure!(init(), "the plugin's InitDll() returned false");
                self.entered = true;
                return Ok(());
            }

            if let Ok(entry) = self.library.get::<ModuleEntryProc>(b"ModuleEntry\0") {
                anyhow::ensure!(
                    entry(self.handle),
                    "the plugin's ModuleEntry() returned false — it declined to load"
                );
                self.entered = true;
            } else {
                log::debug!("{} has no ModuleEntry", self.path.display());
            }
        }
        Ok(())
    }

    /// Fail early, and legibly, on a library that is not a VST3 at all — by far
    /// the most common way "open the plugin" goes wrong, since a VST2 `.so` and a
    /// VST3 `.so` look identical in a file dialog.
    fn check_is_vst3(&self) -> Result<()> {
        unsafe {
            if self
                .library
                .get::<GetPluginFactoryProc>(b"GetPluginFactory\0")
                .is_ok()
            {
                return Ok(());
            }
            Err(not_a_vst3(&self.library, &self.path))
        }
    }
}

impl Drop for Vst3Module {
    fn drop(&mut self) {
        if !self.entered {
            return;
        }
        unsafe {
            #[cfg(target_os = "windows")]
            if let Ok(exit) = self.library.get::<ModuleExitProc>(b"ExitDll\0") {
                exit();
                return;
            }
            if let Ok(exit) = self.library.get::<ModuleExitProc>(b"ModuleExit\0") {
                exit();
            }
        }
    }
}

/// The error for a library with no VST3 entry point, naming the likely reason.
unsafe fn not_a_vst3(library: &Library, path: &Path) -> anyhow::Error {
    let vst2 = library.get::<*mut c_void>(b"VSTPluginMain\0").is_ok()
        || library.get::<*mut c_void>(b"main_plugin\0").is_ok();
    anyhow!(
        "{} is not a VST3 plugin — it has no GetPluginFactory entry point{}",
        path.display(),
        if vst2 {
            " (it exports VSTPluginMain, so it is a VST2 plugin, which this host cannot load)"
        } else {
            ""
        }
    )
}

/// Open the library and keep its raw handle: `into_raw` / `from_raw` is the only
/// way `libloading` lets a caller see it, and the round-trip is a no-op.
#[cfg(unix)]
fn open_library(path: &Path) -> Result<(Library, *mut c_void)> {
    use libloading::os::unix::{Library as UnixLibrary, RTLD_LAZY, RTLD_LOCAL};
    let lib = unsafe { UnixLibrary::open(Some(path), RTLD_LAZY | RTLD_LOCAL)? };
    let handle = lib.into_raw();
    let lib = unsafe { UnixLibrary::from_raw(handle) };
    Ok((Library::from(lib), handle))
}

#[cfg(not(unix))]
fn open_library(path: &Path) -> Result<(Library, *mut c_void)> {
    Ok((unsafe { Library::new(path)? }, std::ptr::null_mut()))
}
