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
//! `.gmstn` — the Gemstone project format.
//!
//! A saved project is a **folder**, not a file: `MySong/` holds `MySong.gmstn`,
//! one `.lsft` per LeSynth Fourier track any row plays, and one `.vststate` per
//! custom VST3 track — the plugin's own state, which is where it keeps the knobs
//! the user set. The folder is the unit you move, copy or hand to someone else,
//! and it carries its own sounds; what is left outside it is a third-party
//! plugin's *binary*, and the audio file behind a wav track — neither is ours to
//! copy, and the second is usually larger than the whole rest of the folder.
//! Both are recorded as the path they were added from.
//!
//! **The manifest is line-oriented text, deliberately.** The grids are already
//! binary (`.lsft`); what is left is a small structure whose main job is to hold
//! *file paths and names*, and whose main failure mode is a source that has gone
//! missing. Text means a user can repair that in an editor. It also means
//! `key = value` needs no quoting or escaping at all — the value is the rest of
//! the line — which is exactly what a format full of paths wants, and what JSON
//! would have made worse.
//!
//! ```text
//! gemstone-project 1
//! name = My Song
//! tempo = 120
//!
//! [row]
//! track = LeSynth Fourier 1
//! source = lesynth voice.lsft
//! state = Dexed.vststate
//! gain = 1
//! lead = 0 none
//! autosave = 1
//! note = 60 0 1/4 0 3/8
//! ```
//!
//! A length is two fields — whole notes, then the fraction, nominator and all:
//! `0 3/8` is three eighths, `2 1/4` is two whole notes and a quarter, `1 none`
//! is one whole note. A length with no nominator writes exactly the token it
//! always did, so files that do not use one are unchanged and load in any build;
//! one that does is refused by a build too old to know `3/8`, which is a message
//! rather than a silent misreading.
//!
//! Unknown keys are skipped, so a field added later loads in an older build
//! rather than failing. The version line is checked, so a *newer* format is
//! refused with a message instead of being half-read. A *source kind* added
//! later — `wav` is the most recent — follows the rule the nominator does: a
//! project that does not use one is unchanged and loads in any build, and one
//! that does is refused by a build too old to know it, which is a message
//! rather than a silent misreading.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{Duration, Fraction, Item};

/// What this writer produces. A reader accepts anything up to it.
const VERSION: u32 = 1;
const MAGIC: &str = "gemstone-project";

/// The file extension, and what a project folder's manifest is named after.
pub const EXTENSION: &str = "gmstn";

/// Where a row's sound comes from, as recorded in the manifest.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackSource {
    /// A LeSynth Fourier grid saved beside the manifest. The name is relative to
    /// the project folder, so the folder stays portable.
    LeSynth { file: String },
    /// A LeSynth Fourier track carrying no grid — the plugin's own synth mode.
    LeSynthDefault,
    /// A custom VST3, by absolute path. Not portable, and cannot be: the plugin
    /// is not ours to copy into the folder. What *is* saved beside the manifest
    /// is `state` — the plugin's own `IComponent` state, named relative to the
    /// project folder. `None` means the project was saved before the plugin had
    /// any state to keep, or by a build that did not save it.
    Vst {
        path: PathBuf,
        class_id: Option<[i8; 16]>,
        state: Option<String>,
    },
    /// An audio file played whole, as one note, by absolute path. Like a VST3's
    /// binary and for the same reason, the file is **not** copied into the
    /// project folder: it is the user's recording, often far larger than
    /// everything else in the folder put together, and it is not ours to
    /// duplicate. What is saved is the path it was first added from.
    Wav { path: PathBuf },
    /// The row had no track assigned when it was saved.
    None,
}

impl TrackSource {
    /// What this source needs from the filesystem, for the caller to check
    /// before trying to load it. `None` for sources that need nothing.
    pub fn required_path(&self, dir: &Path) -> Option<PathBuf> {
        match self {
            Self::LeSynth { file } => Some(dir.join(file)),
            // The plugin itself. A missing `.vststate` is not fatal — the track
            // loads with the plugin's defaults — so it is not required here.
            Self::Vst { path, .. } => Some(path.clone()),
            // The whole sound of the row: without the file there is nothing to
            // play at all.
            Self::Wav { path } => Some(path.clone()),
            Self::LeSynthDefault | Self::None => None,
        }
    }

    /// How to name this source in a message when it cannot be found.
    pub fn describe(&self) -> String {
        match self {
            Self::LeSynth { file } => file.clone(),
            Self::Vst { path, .. } | Self::Wav { path } => crate::file_label(path),
            Self::LeSynthDefault => "LeSynth Fourier".to_string(),
            Self::None => "no track".to_string(),
        }
    }
}

/// One lane: which sound it plays, how loud, and the chain of frames on it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectRow {
    /// Display name of the track, kept so a row whose source has gone missing
    /// can still say what it was looking for.
    pub track_name: String,
    pub source: TrackSource,
    pub gain: f32,
    pub lead: Duration,
    /// Re-export this row's grid on every save — see [`super::Row::autosave`].
    pub autosave: bool,
    /// Follow the transport in this lane — see [`super::Row::autoscroll`]. A
    /// project written before this existed has no such key, and a row from one
    /// reads as `true`, which is the default a new row gets.
    pub autoscroll: bool,
    pub items: Vec<Item>,
}

/// A whole composition, as it sits in the manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub name: String,
    pub tempo_bpm: f32,
    pub rows: Vec<ProjectRow>,
}

impl Project {
    pub fn to_text(&self) -> String {
        let mut s = format!("{MAGIC} {VERSION}\n");
        s += &format!("name = {}\n", one_line(&self.name));
        s += &format!("tempo = {}\n", num(self.tempo_bpm));
        for row in &self.rows {
            s += "\n[row]\n";
            s += &format!("track = {}\n", one_line(&row.track_name));
            s += &format!("source = {}\n", write_source(&row.source));
            if let TrackSource::Vst { state: Some(file), .. } = &row.source {
                s += &format!("state = {}\n", one_line(file));
            }
            s += &format!("gain = {}\n", num(row.gain));
            s += &format!("lead = {}\n", write_duration(row.lead));
            s += &format!("autosave = {}\n", u8::from(row.autosave));
            s += &format!("autoscroll = {}\n", u8::from(row.autoscroll));
            for item in &row.items {
                s += &format!(
                    "note = {} {} {}\n",
                    item.pitch,
                    write_duration(item.dur),
                    write_duration(item.space)
                );
            }
        }
        s
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
        let header = lines.next().context("empty project file")?;
        let (magic, version) = header.split_once(' ').unwrap_or((header, ""));
        if magic != MAGIC {
            bail!("not a Gemstone project file (first line reads {header:?})");
        }
        let version: u32 = version.trim().parse().context("unreadable format version")?;
        if version > VERSION {
            bail!(
                "project was saved by a newer Gemstone ({version} > {VERSION}); \
                 update before opening it"
            );
        }

        let mut project = Project { name: String::new(), tempo_bpm: 120.0, rows: Vec::new() };
        for line in lines {
            if line == "[row]" {
                project.rows.push(ProjectRow {
                    track_name: String::new(),
                    source: TrackSource::None,
                    gain: 1.0,
                    lead: Duration::new(0, Fraction::None),
                    autosave: true,
                    autoscroll: true,
                    items: Vec::new(),
                });
                continue;
            }
            // The value is the rest of the line, so nothing needs escaping — see
            // the module docs. Unknown keys are skipped for forward compatibility.
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            match (key, project.rows.last_mut()) {
                ("name", None) => project.name = value.to_string(),
                ("tempo", None) => project.tempo_bpm = value.parse().unwrap_or(120.0),
                ("track", Some(row)) => row.track_name = value.to_string(),
                ("source", Some(row)) => row.source = read_source(value)?,
                // Belongs to the source above it, which is where the writer puts
                // it. Ignored for any other kind of source.
                ("state", Some(row)) => {
                    if let TrackSource::Vst { state, .. } = &mut row.source {
                        *state = Some(value.to_string());
                    }
                }
                ("gain", Some(row)) => row.gain = value.parse().unwrap_or(1.0),
                ("lead", Some(row)) => row.lead = read_duration(value)?,
                ("autosave", Some(row)) => row.autosave = value != "0",
                ("autoscroll", Some(row)) => row.autoscroll = value != "0",
                ("note", Some(row)) => row.items.push(read_note(value)?),
                _ => {}
            }
        }
        Ok(project)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_text())
            .with_context(|| format!("write {}", crate::file_label(path)))
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", crate::file_label(path)))?;
        Self::parse(&text).with_context(|| format!("in {}", crate::file_label(path)))
    }
}

/// Turn a typed project name into something safe to use as a folder and file
/// name: one path component, no separators, no surprises.
pub fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            c if c.is_alphanumeric() => c,
            ' ' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim().to_string();
    if cleaned.is_empty() {
        "Untitled".to_string()
    } else {
        cleaned
    }
}

/// A `.lsft` file name for a track, unique within `taken`.
pub fn grid_file_name(track_name: &str, taken: &[String]) -> String {
    unique_file_name(track_name, "lsft", taken)
}

/// A file named after a track, with extension `ext`, unique within `taken`.
pub fn unique_file_name(track_name: &str, ext: &str, taken: &[String]) -> String {
    let stem = sanitize_name(track_name);
    let mut candidate = format!("{stem}.{ext}");
    let mut n = 2;
    while taken.iter().any(|t| t.eq_ignore_ascii_case(&candidate)) {
        candidate = format!("{stem} {n}.{ext}");
        n += 1;
    }
    candidate
}

// ── field encodings ─────────────────────────────────────────────────────────

/// Trim a value to one line: the format's only real constraint on free text.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

/// Compact but exact enough to round-trip a tempo or a gain.
fn num(v: f32) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() { "0".to_string() } else { s.to_string() }
}

fn write_duration(d: Duration) -> String {
    format!("{} {}", d.wholes, frac_token(d.num, d.frac))
}

fn read_duration(v: &str) -> Result<Duration> {
    let (w, f) = v.split_once(' ').unwrap_or((v, "none"));
    let (num, frac) = frac_from_token(f.trim())?;
    Ok(Duration::with_num(w.trim().parse().unwrap_or(0), num, frac))
}

fn read_note(v: &str) -> Result<Item> {
    let parts: Vec<&str> = v.split_whitespace().collect();
    if parts.len() != 5 {
        bail!("a note takes 5 fields (pitch, note length, space length), got {v:?}");
    }
    Ok(Item {
        // Filled in by the loader, which owns row-local ids.
        id: 0,
        pitch: parts[0].parse().context("note pitch")?,
        dur: read_duration(&format!("{} {}", parts[1], parts[2]))?,
        space: read_duration(&format!("{} {}", parts[3], parts[4]))?,
    })
}

/// The fractional part, nominator and all: `1/4`, `3/8`, or `none` where there
/// is no fraction. Plain ASCII, not [`Fraction::label`]'s em dash — this is a
/// file, and it is meant to be typed by hand when a project needs repairing.
///
/// The nominator rides inside the token rather than in a field of its own, so a
/// length that has none writes the token this format has always written and the
/// note line keeps its five fields.
fn frac_token(num: u8, f: Fraction) -> String {
    match f {
        Fraction::None => "none".to_string(),
        f => format!("{num}/{}", f.denom()),
    }
}

/// The nominator and the fraction the token names. A hand-written nominator
/// larger than the select box offers is kept as written: the file says what is
/// playing, and quietly rounding someone's edit down is worse than showing them
/// a value the box did not offer.
fn frac_from_token(t: &str) -> Result<(u8, Fraction)> {
    if matches!(t, "none" | "-" | "0") {
        return Ok((1, Fraction::None));
    }
    let bad = || format!("unknown note fraction {t:?}");
    let (n, d) = t.split_once('/').with_context(bad)?;
    let num: u8 = n.parse().with_context(bad)?;
    if num == 0 {
        bail!(
            "a note fraction is counted at least once: {t:?} — a length with no \
             fraction at all writes \"none\""
        );
    }
    let denom: i64 = d.parse().with_context(bad)?;
    let frac = Fraction::ALL
        .into_iter()
        .find(|f| *f != Fraction::None && f.denom() == denom)
        .with_context(bad)?;
    Ok((num, frac))
}

fn write_source(s: &TrackSource) -> String {
    match s {
        TrackSource::LeSynth { file } => format!("lesynth {}", one_line(file)),
        TrackSource::LeSynthDefault => "lesynth -".to_string(),
        // The state file, if any, rides on its own `state =` line: the path here
        // is the rest of the line, so nothing can follow it.
        TrackSource::Vst { path, class_id, .. } => {
            let mut out = format!("vst {}", one_line(&path.display().to_string()));
            if let Some(id) = class_id {
                out = format!("vst:{} {}", hex_class(id), one_line(&path.display().to_string()));
            }
            out
        }
        TrackSource::Wav { path } => format!("wav {}", one_line(&path.display().to_string())),
        TrackSource::None => "none".to_string(),
    }
}

fn read_source(v: &str) -> Result<TrackSource> {
    let (kind, rest) = v.split_once(' ').unwrap_or((v, ""));
    let rest = rest.trim();
    if kind == "none" {
        return Ok(TrackSource::None);
    }
    if kind == "lesynth" {
        return Ok(if rest.is_empty() || rest == "-" {
            TrackSource::LeSynthDefault
        } else {
            TrackSource::LeSynth { file: rest.to_string() }
        });
    }
    if let Some(hex) = kind.strip_prefix("vst:") {
        return Ok(TrackSource::Vst {
            path: PathBuf::from(rest),
            class_id: Some(class_from_hex(hex)?),
            state: None,
        });
    }
    if kind == "wav" {
        return Ok(TrackSource::Wav { path: PathBuf::from(rest) });
    }
    if kind == "vst" {
        return Ok(TrackSource::Vst {
            path: PathBuf::from(rest),
            class_id: None,
            state: None,
        });
    }
    bail!("unknown track source {v:?}")
}

fn hex_class(id: &[i8; 16]) -> String {
    id.iter().map(|b| format!("{:02x}", *b as u8)).collect()
}

fn class_from_hex(hex: &str) -> Result<[i8; 16]> {
    if hex.len() != 32 {
        bail!("a VST3 class id is 32 hex digits, got {}", hex.len());
    }
    let mut out = [0i8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).context("class id")? as i8;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::composer::UNITS_PER_WHOLE;

    fn sample() -> Project {
        Project {
            name: "My Song".to_string(),
            tempo_bpm: 132.5,
            rows: vec![
                ProjectRow {
                    track_name: "LeSynth Fourier 1".to_string(),
                    source: TrackSource::LeSynth { file: "voice.lsft".to_string() },
                    gain: 0.75,
                    lead: Duration::new(0, Fraction::Eighth),
                    autosave: true,
                    autoscroll: true,
                    items: vec![
                        Item {
                            id: 0,
                            pitch: 60,
                            dur: Duration::new(0, Fraction::Quarter),
                            space: Duration::new(0, Fraction::Eighth),
                        },
                        Item {
                            id: 0,
                            pitch: 67,
                            // A nominator on the note and none on the space, so
                            // the round trip proves the field travels rather
                            // than that both ends happen to say `1`.
                            dur: Duration::with_num(2, 3, Fraction::Eighth),
                            space: Duration::new(0, Fraction::None),
                        },
                    ],
                },
                ProjectRow {
                    track_name: "some-plugin.so".to_string(),
                    source: TrackSource::Vst {
                        path: PathBuf::from("/opt/vst3/some plugin.so"),
                        class_id: Some([1, 2, 3, -4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, -16]),
                        state: Some("some plugin.vststate".to_string()),
                    },
                    gain: 1.0,
                    lead: Duration::new(1, Fraction::Sixteenth),
                    autosave: false,
                    // Both per-row switches off on this row, so the round trip
                    // proves each carries rather than that `true` survives.
                    autoscroll: false,
                    items: vec![],
                },
            ],
        }
    }

    /// A wav track is a path and nothing else: no file beside the manifest, no
    /// state, no class id. The row that plays one must come back naming the same
    /// file — losing it would leave the row silent with nothing to say why.
    #[test]
    fn a_wav_row_round_trips_as_its_path() {
        let mut p = sample();
        p.rows[1].source = TrackSource::Wav {
            path: PathBuf::from("/home/kuba/Music/my voice.wav"),
        };
        let text = p.to_text();
        assert!(
            text.contains("source = wav /home/kuba/Music/my voice.wav"),
            "the path is the whole source line: {text}"
        );
        // No `state =` rides behind it: there is nothing beside the manifest to
        // point at.
        assert!(!text.contains("state ="), "a wav track saves no state file: {text}");
        assert_eq!(Project::parse(&text).expect("parses"), p);
    }

    /// The whole point of a save format: what comes back is what went in.
    #[test]
    fn a_project_round_trips_through_the_file() {
        let p = sample();
        let back = Project::parse(&p.to_text()).expect("parses");
        assert_eq!(back, p);
    }

    /// Paths are the one thing this format exists to carry, and they contain
    /// spaces, `=` and other characters a quoted format would have to escape.
    /// The value is the rest of the line, so none of them need it.
    #[test]
    fn a_path_needs_no_escaping() {
        let mut p = sample();
        p.rows[1].source = TrackSource::Vst {
            path: PathBuf::from("/opt/vst3/weird = name (v2) [x86].so"),
            class_id: None,
            state: Some("weird = name.vststate".to_string()),
        };
        p.name = "Song = 2".to_string();
        let back = Project::parse(&p.to_text()).expect("parses");
        assert_eq!(back, p);
    }

    /// The nominator rides in the fraction token. A length without one writes
    /// the token this format has always written — that is what keeps old files
    /// and old builds working — and one with it writes the fraction whole.
    #[test]
    fn a_length_writes_its_nominator_into_the_fraction() {
        assert_eq!(write_duration(Duration::new(0, Fraction::Quarter)), "0 1/4");
        assert_eq!(write_duration(Duration::with_num(2, 3, Fraction::Eighth)), "2 3/8");
        assert_eq!(write_duration(Duration::new(1, Fraction::None)), "1 none");

        // And back, including the tokens written before nominators existed.
        for (text, units) in [
            ("0 1/4", UNITS_PER_WHOLE / 4),
            ("0 3/8", UNITS_PER_WHOLE * 3 / 8),
            ("2 3/8", UNITS_PER_WHOLE * 2 + UNITS_PER_WHOLE * 3 / 8),
            ("1 none", UNITS_PER_WHOLE),
            ("0 none", 0),
            ("0 16/256", UNITS_PER_WHOLE / 16),
        ] {
            let d = read_duration(text).unwrap_or_else(|e| panic!("{text:?}: {e}"));
            assert_eq!(d.units(), units, "{text:?}");
        }

        // A fraction that is no fraction at all is `none`, not a zero nominator:
        // the file is refused rather than read as some other length.
        assert!(read_duration("0 0/8").is_err());
        assert!(read_duration("0 1/7").is_err());
        assert!(read_duration("0 3").is_err());
    }

    /// A row saved before a per-row switch existed reads as the default a new
    /// row gets, not as `false`. The format skips keys it does not know, which
    /// is exactly how a missing one arrives.
    #[test]
    fn a_row_without_the_newer_switches_takes_their_defaults() {
        let text = "gemstone-project 1\nname = X\n\n\
                    [row]\nsource = none\ngain = 1\nnote = 60 0 1/4 0 none\n";
        let p = Project::parse(text).expect("parses");
        assert!(p.rows[0].autosave, "autosave should default on");
        assert!(p.rows[0].autoscroll, "a lane should follow the transport by default");
    }

    /// A field this build does not know must not stop it loading — that is what
    /// lets an older Gemstone open a newer project's rows.
    #[test]
    fn an_unknown_field_is_skipped_not_fatal() {
        let text = "gemstone-project 1\nname = X\nsomething_new = 4\n\n\
                    [row]\nsource = none\nfuture_field = yes\nnote = 60 0 1/4 0 none\n";
        let p = Project::parse(text).expect("parses");
        assert_eq!(p.name, "X");
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].items.len(), 1);
    }

    /// A newer format is refused outright rather than half-read, because a row
    /// silently missing its notes is worse than a message.
    #[test]
    fn a_newer_format_is_refused() {
        let err = Project::parse("gemstone-project 99\nname = X\n").unwrap_err().to_string();
        assert!(err.contains("newer Gemstone"), "{err}");
        assert!(Project::parse("something else\n").is_err());
    }

    #[test]
    fn every_source_kind_round_trips() {
        for src in [
            TrackSource::None,
            TrackSource::LeSynthDefault,
            TrackSource::LeSynth { file: "a b.lsft".to_string() },
            TrackSource::Vst { path: PathBuf::from("/x/y.so"), class_id: None, state: None },
            TrackSource::Vst {
                path: PathBuf::from("/x/y.so"),
                class_id: Some([-1i8; 16]),
                state: None,
            },
            TrackSource::Vst {
                path: PathBuf::from("/x/y.so"),
                class_id: None,
                state: Some("y.vststate".to_string()),
            },
            TrackSource::Wav { path: PathBuf::from("/x/a whole take.wav") },
        ] {
            let mut p = sample();
            p.rows.truncate(1);
            p.rows[0].source = src.clone();
            let back = Project::parse(&p.to_text()).expect("parses");
            assert_eq!(back.rows[0].source, src);
        }
    }

    /// A project name is a folder name, so it must survive being typed.
    #[test]
    fn a_name_becomes_one_safe_path_component() {
        assert_eq!(sanitize_name("My Song"), "My Song");
        assert_eq!(sanitize_name("a/b"), "a_b");
        assert_eq!(sanitize_name("  "), "Untitled");
        assert_eq!(sanitize_name(""), "Untitled");
        // Nothing that escapes the folder, starts a hidden one, or ends the
        // string somewhere a filesystem will not follow.
        for hostile in ["../etc", "..", "/", ".hidden", "a\0b", "c:\\x", "  ..  "] {
            let got = sanitize_name(hostile);
            assert!(!got.is_empty(), "{hostile:?} sanitised to nothing");
            assert!(!got.starts_with('.'), "{hostile:?} -> {got:?} is hidden");
            assert!(
                !got.contains(['/', '\\', '\0']),
                "{hostile:?} -> {got:?} still has a separator"
            );
            assert_eq!(std::path::Path::new(&got).components().count(), 1, "{got:?}");
        }
    }

    /// Two tracks may share a name; their grids may not share a file.
    #[test]
    fn grid_file_names_do_not_collide() {
        let mut taken = Vec::new();
        for want in ["Voice.lsft", "Voice 2.lsft", "Voice 3.lsft"] {
            let got = grid_file_name("Voice", &taken);
            assert_eq!(got, want);
            taken.push(got);
        }
    }
}

#[cfg(test)]
mod folder_tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gmstn-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A project folder is the unit that moves: the manifest names its grids by
    /// a *relative* file, so copying the folder somewhere else still resolves.
    #[test]
    fn a_saved_folder_carries_its_own_grids() {
        let dir = tmp("portable");
        let p = Project {
            name: "Song".to_string(),
            tempo_bpm: 100.0,
            rows: vec![ProjectRow {
                track_name: "Voice".to_string(),
                source: TrackSource::LeSynth { file: "Voice.lsft".to_string() },
                gain: 1.0,
                lead: Duration::new(0, Fraction::None),
                autosave: true,
                autoscroll: true,
                items: vec![],
            }],
        };
        let manifest = dir.join(format!("Song.{EXTENSION}"));
        p.write(&manifest).unwrap();
        fs::write(dir.join("Voice.lsft"), b"not a real grid").unwrap();

        let moved = tmp("portable-moved");
        for f in fs::read_dir(&dir).unwrap() {
            let f = f.unwrap();
            fs::copy(f.path(), moved.join(f.file_name())).unwrap();
        }
        let back = Project::read(&moved.join(format!("Song.{EXTENSION}"))).unwrap();
        let want = back.rows[0].source.required_path(&moved).unwrap();
        assert!(want.exists(), "{} did not follow the folder", want.display());
        assert_eq!(back, p);

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&moved);
    }

    /// The requirement: a grid that has been deleted must be *detectable*, and
    /// the manifest must still say what it was, so the row can ask for a
    /// replacement instead of silently going quiet.
    #[test]
    fn a_deleted_grid_is_detected_and_still_named() {
        let dir = tmp("missing");
        let p = Project {
            name: "Song".to_string(),
            tempo_bpm: 120.0,
            rows: vec![ProjectRow {
                track_name: "Voice".to_string(),
                source: TrackSource::LeSynth { file: "Voice.lsft".to_string() },
                gain: 1.0,
                lead: Duration::new(0, Fraction::None),
                autosave: true,
                autoscroll: true,
                items: vec![],
            }],
        };
        p.write(&dir.join(format!("Song.{EXTENSION}"))).unwrap();
        // The grid was never written — the same state as a user deleting it.
        let back = Project::read(&dir.join(format!("Song.{EXTENSION}"))).unwrap();
        let wanted = back.rows[0].source.required_path(&dir).unwrap();
        assert!(!wanted.exists());
        assert_eq!(back.rows[0].source.describe(), "Voice.lsft");
        assert_eq!(back.rows[0].track_name, "Voice");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A missing custom VST names its path, which is the only thing that can
    /// help — the plugin is not ours to have copied into the folder.
    #[test]
    fn a_missing_vst_names_its_path() {
        let src = TrackSource::Vst {
            path: PathBuf::from("/nowhere/plugin.so"),
            class_id: None,
            state: Some("plugin.vststate".to_string()),
        };
        assert_eq!(src.required_path(Path::new("/x")).unwrap(), PathBuf::from("/nowhere/plugin.so"));
        assert!(src.describe().contains("plugin.so"));
    }
}
