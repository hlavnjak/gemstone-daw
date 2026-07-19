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
//! `.lsft` — the custom LeSynth Fourier Track format.
//!
//! A saved track is the full harmonic grid a user built/edited in a LeSynth
//! editor: `amplitude[h][b]` and `phase[h][b]` for every harmonic and bucket,
//! plus the per-bucket pitch (`pitch_ratio[b]`, i.e. `f_local / base_freq`) and
//! the reference `base_freq`, so the per-bucket absolute frequency is
//! `base_freq * pitch_ratio[b]`. The host reads/writes this file; the grid is
//! transferred to/from a live plugin instance over the C ABI (see
//! `PluginInstance::export_state` / `import_state`).
//!
//! Layout (little-endian):
//! ```text
//! "LSFT" (4) | version u32 | num_harmonics u32 | num_buckets u32 |
//! base_freq f32 | duration_secs f32 | sample_rate f32 |
//! amplitude[nh*nb] f32 (row-major, h*nb + b) | phase[nh*nb] f32 |
//! pitch_ratio[nb] f32
//! ```

use std::fs;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};

const MAGIC: [u8; 4] = *b"LSFT";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 4 + 4 + 4 + 4 + 4 + 4 + 4; // magic..sample_rate

/// The full state of one LeSynth Fourier track.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackState {
    pub num_harmonics: usize,
    pub num_buckets: usize,
    /// Reference fundamental (Hz); per-bucket freq = `base_freq * pitch_ratio[b]`.
    pub base_freq: f32,
    /// Source wall-clock duration (s) a note renders for in Analysis mode.
    pub duration_secs: f32,
    /// Sample rate the grid was captured at (informational; not applied on load).
    pub sample_rate: f32,
    /// `amplitude`/`phase` are row-major `[h * num_buckets + b]`, `nh*nb` long.
    pub amplitude: Vec<f32>,
    pub phase: Vec<f32>,
    /// Per-bucket pitch ratio (`f_local / base_freq`), `num_buckets` long.
    pub pitch_ratio: Vec<f32>,
}

impl TrackState {
    /// Serialize to the `.lsft` byte layout.
    pub fn to_bytes(&self) -> Vec<u8> {
        let grid = self.num_harmonics * self.num_buckets;
        let mut out = Vec::with_capacity(HEADER_LEN + (grid * 2 + self.num_buckets) * 4);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.num_harmonics as u32).to_le_bytes());
        out.extend_from_slice(&(self.num_buckets as u32).to_le_bytes());
        out.extend_from_slice(&self.base_freq.to_le_bytes());
        out.extend_from_slice(&self.duration_secs.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        for &v in self.amplitude.iter().chain(&self.phase).chain(&self.pitch_ratio) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Parse from the `.lsft` byte layout, validating magic, version and that the
    /// declared grid size matches the actual byte length exactly (which also
    /// bounds allocation to the input length).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() >= HEADER_LEN, "file too short to be a LeSynth track");
        ensure!(bytes[0..4] == MAGIC, "not a LeSynth track (bad magic)");
        let mut cur = 4;
        let next_u32 = |cur: &mut usize| -> u32 {
            let v = u32::from_le_bytes(bytes[*cur..*cur + 4].try_into().unwrap());
            *cur += 4;
            v
        };
        let version = next_u32(&mut cur);
        ensure!(version == VERSION, "unsupported .lsft version {version}");
        let num_harmonics = next_u32(&mut cur) as usize;
        let num_buckets = next_u32(&mut cur) as usize;
        let base_freq = f32::from_le_bytes(bytes[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let duration_secs = f32::from_le_bytes(bytes[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let sample_rate = f32::from_le_bytes(bytes[cur..cur + 4].try_into().unwrap());
        cur += 4;

        let grid = num_harmonics
            .checked_mul(num_buckets)
            .context("grid size overflow")?;
        let floats = grid
            .checked_mul(2)
            .and_then(|g| g.checked_add(num_buckets))
            .context("payload size overflow")?;
        let expected = HEADER_LEN + floats * 4;
        ensure!(
            bytes.len() == expected,
            "corrupt .lsft: expected {expected} bytes, got {}",
            bytes.len()
        );

        let read_floats = |cur: &mut usize, n: usize| -> Vec<f32> {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(f32::from_le_bytes(bytes[*cur..*cur + 4].try_into().unwrap()));
                *cur += 4;
            }
            v
        };
        let amplitude = read_floats(&mut cur, grid);
        let phase = read_floats(&mut cur, grid);
        let pitch_ratio = read_floats(&mut cur, num_buckets);

        Ok(Self {
            num_harmonics,
            num_buckets,
            base_freq,
            duration_secs,
            sample_rate,
            amplitude,
            phase,
            pitch_ratio,
        })
    }

    /// Write the track to `path` (creating/truncating it).
    pub fn write(&self, path: &Path) -> Result<()> {
        fs::write(path, self.to_bytes())
            .with_context(|| format!("writing track to {}", path.display()))
    }

    /// Read a track from `path`.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("reading track from {}", path.display()))?;
        Self::from_bytes(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    /// Basic shape check: the grids match the declared dimensions. Used before
    /// handing a freshly parsed/exported state to the plugin.
    pub fn validate(&self) -> Result<()> {
        let grid = self.num_harmonics * self.num_buckets;
        if self.amplitude.len() != grid || self.phase.len() != grid {
            bail!("grid length mismatch");
        }
        if self.pitch_ratio.len() != self.num_buckets {
            bail!("pitch_ratio length mismatch");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> TrackState {
        let (nh, nb) = (3usize, 4usize);
        let amplitude: Vec<f32> = (0..nh * nb).map(|i| i as f32 * 0.01).collect();
        let phase: Vec<f32> = (0..nh * nb).map(|i| (i as f32).sin()).collect();
        let pitch_ratio = vec![1.0, 1.01, 0.99, 1.0];
        TrackState {
            num_harmonics: nh,
            num_buckets: nb,
            base_freq: 220.0,
            duration_secs: 0.75,
            sample_rate: 44_100.0,
            amplitude,
            phase,
            pitch_ratio,
        }
    }

    #[test]
    fn bytes_round_trip() {
        let s = sample_state();
        let parsed = TrackState::from_bytes(&s.to_bytes()).expect("parse");
        assert_eq!(parsed, s);
        s.validate().unwrap();
    }

    #[test]
    fn file_round_trip() {
        let s = sample_state();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("lsft_test_{}.lsft", std::process::id()));
        s.write(&path).unwrap();
        let back = TrackState::read(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(back, s);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample_state().to_bytes();
        bytes[0] = b'X';
        assert!(TrackState::from_bytes(&bytes).is_err());
    }

    #[test]
    fn rejects_truncated() {
        let bytes = sample_state().to_bytes();
        assert!(TrackState::from_bytes(&bytes[..bytes.len() - 8]).is_err());
        assert!(TrackState::from_bytes(&bytes[..3]).is_err());
    }

    #[test]
    fn rejects_wrong_declared_size() {
        // Tamper num_buckets so the declared payload no longer matches the bytes.
        let mut bytes = sample_state().to_bytes();
        bytes[12..16].copy_from_slice(&999u32.to_le_bytes()); // num_buckets field
        assert!(TrackState::from_bytes(&bytes).is_err());
    }
}
