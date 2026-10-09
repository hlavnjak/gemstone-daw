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
//! Finding the plugins installed on this machine, in every format.
//!
//! Each format has its own conventional directories and its own environment
//! variable to add more (`VST3_PATH`, `CLAP_PATH`, `VST_PATH`, `LV2_PATH`).
//! What a scan costs differs too: a VST3 or a VST2 is listed by its file
//! alone, an LV2 bundle by reading its Turtle, a CLAP file by asking its
//! factory which plugins it holds — which the spec makes cheap on purpose —
//! and an Audio Unit by asking the system's component registry.

use std::path::{Path, PathBuf};

use super::{display_stem, existing_dirs, PluginFormat};

/// One plugin the scan found.
#[derive(Clone, Debug, PartialEq)]
pub struct FoundPlugin {
    /// What the picker lists it as.
    pub name: String,
    pub format: PluginFormat,
    /// The file or bundle to load. An Audio Unit's is [`super::AU_PATH`].
    pub path: PathBuf,
    /// Which plugin inside it, where the format can hold several. See
    /// [`super::PluginInstance::load`].
    pub plugin_id: Option<String>,
}

/// How deep below a plugin directory a scan looks. Windows and macOS installers
/// put plugins in vendor folders (`VST3/Vendor/Foo.vst3`); deeper than this is
/// not a plugin directory any more.
const MAX_DEPTH: usize = 3;

/// The directories plugins of `format` are installed into, most specific
/// first, keeping only those that exist. The format's environment variable adds
/// to the list rather than replacing it, as it does for other hosts.
pub fn search_paths(format: PluginFormat) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let env_var = match format {
        PluginFormat::Vst3 => Some("VST3_PATH"),
        PluginFormat::Clap => Some("CLAP_PATH"),
        PluginFormat::Vst2 => Some("VST_PATH"),
        PluginFormat::Lv2 => Some("LV2_PATH"),
        PluginFormat::Au => None,
    };
    if let Some(extra) = env_var.and_then(std::env::var_os) {
        dirs.extend(std::env::split_paths(&extra));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);

    #[cfg(target_os = "linux")]
    {
        let (user, system): (&[&str], &[&str]) = match format {
            PluginFormat::Vst3 => (&[".vst3"], &["vst3"]),
            PluginFormat::Clap => (&[".clap"], &["clap"]),
            PluginFormat::Vst2 => (&[".vst", ".lxvst"], &["vst", "lxvst"]),
            PluginFormat::Lv2 => (&[".lv2"], &["lv2"]),
            PluginFormat::Au => (&[], &[]),
        };
        if let Some(home) = &home {
            dirs.extend(user.iter().map(|d| home.join(d)));
        }
        for root in ["/usr/lib", "/usr/local/lib", "/usr/lib64", "/usr/local/lib64"] {
            dirs.extend(system.iter().map(|d| Path::new(root).join(d)));
        }
    }

    #[cfg(target_os = "windows")]
    {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let common = var("CommonProgramFiles");
        let program_files = var("ProgramFiles");
        match format {
            PluginFormat::Vst3 => dirs.extend(common.map(|c| c.join("VST3"))),
            PluginFormat::Clap => {
                dirs.extend(common.map(|c| c.join("CLAP")));
                dirs.extend(var("LOCALAPPDATA").map(|l| l.join("Programs/Common/CLAP")));
            }
            PluginFormat::Vst2 => {
                if let Some(pf) = &program_files {
                    dirs.push(pf.join("VSTPlugins"));
                    dirs.push(pf.join("Steinberg/VSTPlugins"));
                    dirs.push(pf.join("Common Files/VST2"));
                }
                if let Some(c) = &common {
                    dirs.push(c.join("VST2"));
                    dirs.push(c.join("Steinberg/VST2"));
                }
            }
            PluginFormat::Lv2 => {
                dirs.extend(var("APPDATA").map(|a| a.join("LV2")));
                dirs.extend(common.map(|c| c.join("LV2")));
            }
            PluginFormat::Au => {}
        }
        let _ = home;
    }

    #[cfg(target_os = "macos")]
    {
        let sub = match format {
            PluginFormat::Vst3 => Some("VST3"),
            PluginFormat::Clap => Some("CLAP"),
            PluginFormat::Vst2 => Some("VST"),
            PluginFormat::Lv2 => Some("LV2"),
            PluginFormat::Au => Some("Components"),
        };
        if let Some(sub) = sub {
            if let Some(home) = &home {
                dirs.push(home.join("Library/Audio/Plug-Ins").join(sub));
            }
            dirs.push(Path::new("/Library/Audio/Plug-Ins").join(sub));
        }
    }

    existing_dirs(dirs)
}

/// Every plugin installed in the standard places, in every format this build
/// loads, sorted by name — and every directory that was searched, for the
/// picker to show when it found nothing.
pub fn scan_installed() -> (Vec<FoundPlugin>, Vec<PathBuf>) {
    let mut found: Vec<FoundPlugin> = Vec::new();
    let mut searched: Vec<PathBuf> = Vec::new();
    for format in PluginFormat::ALL.into_iter().filter(|f| f.is_supported()) {
        let dirs = search_paths(format);
        match format {
            PluginFormat::Vst3 => {
                for path in walk(&dirs, &["vst3"]) {
                    found.push(FoundPlugin {
                        name: display_stem(&path),
                        format,
                        path,
                        plugin_id: None,
                    });
                }
            }
            PluginFormat::Clap => {
                for path in walk(&dirs, &["clap"]) {
                    found.extend(super::clap::list_plugins(&path));
                }
            }
            PluginFormat::Vst2 => {
                for path in walk(&dirs, VST2_EXTS) {
                    found.push(FoundPlugin {
                        name: display_stem(&path),
                        format,
                        path,
                        plugin_id: None,
                    });
                }
            }
            PluginFormat::Lv2 => {
                for path in walk(&dirs, &["lv2"]) {
                    found.extend(super::lv2::list_plugins(&path));
                }
            }
            PluginFormat::Au => {
                #[cfg(target_os = "macos")]
                found.extend(super::au::list_plugins());
            }
        }
        searched.extend(dirs);
    }

    found.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(PluginFormat::ALL.iter().position(|f| *f == a.format)
                .cmp(&PluginFormat::ALL.iter().position(|f| *f == b.format)))
    });
    found.dedup_by(|a, b| a.path == b.path && a.plugin_id == b.plugin_id);
    // The same plugin installed in two places (a user copy and a system one)
    // would otherwise show as two identical rows; name each by its directory.
    // Named by the folder rather than the whole path: "Dexed — vst3" and
    // "Dexed — VST3" tell them apart, and the whole path would be most of the
    // window.
    let keys: Vec<(String, PluginFormat)> =
        found.iter().map(|p| (p.name.clone(), p.format)).collect();
    for (idx, entry) in found.iter_mut().enumerate() {
        let twin = keys
            .iter()
            .enumerate()
            .any(|(i, k)| i != idx && k.0 == entry.name && k.1 == entry.format);
        if twin && super::has_file(&entry.path, entry.plugin_id.as_deref()) {
            if let Some(dir) = entry.path.parent() {
                entry.name = format!("{}  —  {}", entry.name, crate::file_label(dir));
            }
        }
    }
    (found, searched)
}

/// Extensions a VST2 library is installed under on this platform.
#[cfg(target_os = "linux")]
const VST2_EXTS: &[&str] = &["so"];
#[cfg(target_os = "windows")]
const VST2_EXTS: &[&str] = &["dll"];
#[cfg(target_os = "macos")]
const VST2_EXTS: &[&str] = &["vst"];
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
const VST2_EXTS: &[&str] = &["so"];

/// Whether `path` is some plugin format's bundle. A walk never looks inside
/// one: the libraries in an `.lv2` or a `.vst3` bundle are that bundle's, and
/// a VST2 scan that went in would list each of them as a plugin of its own.
fn is_bundle(path: &Path) -> bool {
    path.extension().is_some_and(|e| {
        ["vst3", "clap", "lv2", "vst", "component"]
            .iter()
            .any(|b| e.eq_ignore_ascii_case(b))
    })
}

/// Every file or bundle under `dirs` whose extension is one of `exts`, looking
/// at most [`MAX_DEPTH`] levels down. A match is not descended into: a bundle
/// is one plugin, whatever it holds.
fn walk(dirs: &[PathBuf], exts: &[&str]) -> Vec<PathBuf> {
    fn visit(dir: &Path, exts: &[&str], depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            let matches = path
                .extension()
                .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)));
            if matches {
                out.push(path);
            } else if depth + 1 < MAX_DEPTH && path.is_dir() && !is_bundle(&path) {
                visit(&path, exts, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for dir in dirs {
        visit(dir, exts, 0, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A VST2 scan of a folder holding every format (as a vendor's download
    /// does) finds the VST2 and nothing from inside the other formats'
    /// bundles.
    #[test]
    fn a_walk_never_goes_inside_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Synth.lv2")).unwrap();
        std::fs::write(root.join("Synth.lv2/Synth_dsp.so"), b"").unwrap();
        std::fs::create_dir_all(root.join("Synth.vst3/Contents/x86_64-linux")).unwrap();
        std::fs::write(root.join("Synth.vst3/Contents/x86_64-linux/Synth.so"), b"").unwrap();
        std::fs::create_dir_all(root.join("Vendor")).unwrap();
        std::fs::write(root.join("Vendor/Synth-vst.so"), b"").unwrap();
        std::fs::write(root.join("Synth-vst.so"), b"").unwrap();
        let found = walk(&[root.to_path_buf()], &["so"]);
        assert_eq!(found, vec![root.join("Synth-vst.so"), root.join("Vendor/Synth-vst.so")]);
        assert_eq!(walk(&[root.to_path_buf()], &["lv2"]), vec![root.join("Synth.lv2")]);
    }
}
