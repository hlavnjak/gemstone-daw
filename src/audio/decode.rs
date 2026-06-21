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
//! Decode arbitrary `.wav` / `.mp3` / `.m4a` files into mono `f32` PCM.
//!
//! Used by the resynthesis feature: the decoded signal is segmented into
//! pitched "subtracks" which are then handed to LeSynth Fourier for analysis.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// A decoded audio clip, downmixed to mono.
pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: f32,
}

impl DecodedAudio {
    pub fn duration_secs(&self) -> f32 {
        if self.sample_rate > 0.0 {
            self.samples.len() as f32 / self.sample_rate
        } else {
            0.0
        }
    }
}

/// Decode a `.wav`, `.mp3` or `.m4a` file to mono f32 PCM at its native sample rate.
pub fn decode_audio_file(path: &Path) -> Result<DecodedAudio> {
    let file = File::open(path).with_context(|| format!("Failed to open {:?}", path))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("Unsupported or unrecognised audio format")?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .context("No decodable audio track found")?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("No decoder for this codec")?;

    let mut sample_rate = track.codec_params.sample_rate.unwrap_or(44_100) as f32;
    let mut mono: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // Clean EOF / end of stream.
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let spec = *audio_buf.spec();
                sample_rate = spec.rate as f32;
                let channels = spec.channels.count().max(1);

                if sample_buf.is_none() {
                    sample_buf =
                        Some(SampleBuffer::<f32>::new(audio_buf.capacity() as u64, spec));
                }
                let buf = sample_buf.as_mut().unwrap();
                buf.copy_interleaved_ref(audio_buf);
                let interleaved = buf.samples();

                // Downmix to mono.
                for frame in interleaved.chunks(channels) {
                    let sum: f32 = frame.iter().copied().sum();
                    mono.push(sum / channels as f32);
                }
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    anyhow::ensure!(!mono.is_empty(), "Decoded zero samples from {:?}", path);
    Ok(DecodedAudio {
        samples: mono,
        sample_rate,
    })
}