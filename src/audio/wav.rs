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
//! Writing `.wav` files — the one place in the app that lays out a RIFF header.
//!
//! Two sample formats, for two different jobs. [`write_wav_i16`] is the
//! deliverable: 16-bit PCM is what every player, phone and upload form accepts,
//! and its −96 dBFS floor is inaudible under music. [`write_wav_f32`] is for
//! measurement — the offline dumps a fidelity number is computed from, where
//! 16-bit quantisation would put a ±0.00003 floor under the very residual being
//! measured.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};

/// Header sizes: `fmt ` is 16 bytes, and the two chunk headers plus the RIFF
/// form type add the rest.
const HEADER_LEN: u32 = 36;
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// Interleaved 16-bit PCM. Samples outside ±1.0 are clamped rather than allowed
/// to wrap, which is the difference between a loud passage and a burst of noise.
pub fn write_wav_i16(
    path: &Path,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
) -> Result<()> {
    let mut w = header(path, channels, sample_rate, WAVE_FORMAT_PCM, 16, samples.len() * 2)?;
    for &s in samples {
        // i16::MIN..=i16::MAX is asymmetric; scaling by 32767 keeps +1.0 and
        // −1.0 the same distance from silence.
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        w.write_all(&v.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}

/// Interleaved 32-bit float — lossless, so a render can be differenced against
/// its source without the file format contributing to the residual.
pub fn write_wav_f32(
    path: &Path,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
) -> Result<()> {
    let mut w = header(
        path,
        channels,
        sample_rate,
        WAVE_FORMAT_IEEE_FLOAT,
        32,
        samples.len() * 4,
    )?;
    for &s in samples {
        w.write_all(&s.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}

/// Create the file and write everything up to (and including) the `data` chunk
/// header, leaving the writer positioned for `data_len` bytes of samples.
fn header(
    path: &Path,
    channels: u16,
    sample_rate: u32,
    format: u16,
    bits: u16,
    data_len: usize,
) -> Result<BufWriter<File>> {
    let channels = channels.max(1);
    let rate = sample_rate.max(1);
    let block_align = channels * (bits / 8);
    let data_len = data_len as u32;

    let f = File::create(path).with_context(|| format!("create {}", crate::file_label(path)))?;
    let mut w = BufWriter::new(f);
    w.write_all(b"RIFF")?;
    w.write_all(&(HEADER_LEN + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    w.write_all(&format.to_le_bytes())?;
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * block_align as u32).to_le_bytes())?; // byte rate
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&bits.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("gmst_wav_{}_{}.wav", std::process::id(), name))
    }

    /// The header has to describe the samples that follow it, or a player reads
    /// the file at the wrong rate, the wrong width, or not at all.
    #[test]
    fn a_stereo_file_declares_what_it_contains() {
        let path = temp("stereo");
        let samples = vec![0.0f32, 0.5, -0.5, 1.0]; // 2 frames of stereo
        write_wav_i16(&path, &samples, 2, 48_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), WAVE_FORMAT_PCM);
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 2, "channels");
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000,
            "sample rate"
        );
        assert_eq!(u16::from_le_bytes([bytes[32], bytes[33]]), 4, "block align");
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 16, "bits");
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize,
            samples.len() * 2,
            "data length"
        );
        assert_eq!(bytes.len(), 44 + samples.len() * 2);
    }

    /// A mix that overshoots must come out loud, not wrapped: a sample past
    /// +1.0 taken modulo would flip to full negative and crack.
    #[test]
    fn samples_past_full_scale_clamp_instead_of_wrapping() {
        let path = temp("clamp");
        write_wav_i16(&path, &[2.0, -2.0], 1, 44_100).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(i16::from_le_bytes([bytes[44], bytes[45]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), -i16::MAX);
    }

    /// The float writer stores the sample as it was, which is what makes it
    /// usable as a measurement reference.
    #[test]
    fn the_float_writer_stores_the_sample_verbatim() {
        let path = temp("float");
        let s = 0.123_456_79f32;
        write_wav_f32(&path, &[s], 1, 44_100).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            u16::from_le_bytes([bytes[20], bytes[21]]),
            WAVE_FORMAT_IEEE_FLOAT
        );
        assert_eq!(f32::from_le_bytes(bytes[44..48].try_into().unwrap()), s);
    }
}
