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
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::midi::MidiEventQueue;
use crate::plugin::{BlockProcessor, PluginInstance};

/// Audio engine configuration derived from the system audio device.
pub struct AudioConfig {
    pub sample_rate: f64,
    pub max_buffer_size: u32,
    pub channels: usize,
}

/// The audio engine manages the CPAL stream and routes audio through VST3 plugins.
pub struct AudioEngine {
    _stream: Option<cpal::Stream>,
    pub config: AudioConfig,
}

impl AudioEngine {
    /// Query the default audio device and return its configuration.
    pub fn query_device_config() -> Result<AudioConfig> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No audio output device found"))?;
        let cfg = device.default_output_config()?;

        let sample_rate = cfg.sample_rate().0 as f64;
        let max_buffer_size = declared_block_size(cfg.buffer_size());
        let channels = cfg.channels() as usize;

        Ok(AudioConfig {
            sample_rate,
            max_buffer_size,
            channels,
        })
    }

    /// Start audio processing for `plugin`, fed by `midi_events`.
    ///
    /// The whole instance is taken, not just its processor, and the callback
    /// holds it: while a stream exists the plugin cannot be terminated and its
    /// library cannot be unloaded, whatever order its owner drops things in. That
    /// is not defensive — tearing the plugin down under a running stream is a
    /// crash on the audio thread, and it was one.
    ///
    /// The bus layout comes from the plugin's own negotiation in
    /// [`crate::plugin::PluginInstance::initialize_audio`]. It is not decoration:
    /// `process()` reads `numInputs` buses' worth of channel pointers whether or
    /// not the host meant to send any, so an effect (or any plugin with a
    /// side-chain) handed a hard-coded "no inputs, one stereo output"
    /// dereferences pointers that were never provided.
    pub fn start(plugin: Arc<PluginInstance>, midi_events: MidiEventQueue) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No audio output device found"))?;
        let cfg = device.default_output_config()?;

        let sample_rate = cfg.sample_rate().0 as f64;
        let max_buffer_size = declared_block_size(cfg.buffer_size());
        let channels = cfg.channels() as usize;
        let stream_cfg: cpal::StreamConfig = cfg.into();

        // Every buffer the plugin will be handed, and whatever its format's
        // `process()` needs pointed at them, built once: the audio callback must
        // not allocate. What the plugin was set up for wins over what the device
        // says now — the device is asked for its format twice, once to
        // initialise the plugin and once here, and if those two answers ever
        // differ, the plugin is the one that gets a block it never sized for.
        let mut processor = BlockProcessor::new(plugin, max_buffer_size as usize)?;
        let plugin_max_block = processor.max_block();

        static WARNED_SHORT: AtomicBool = AtomicBool::new(false);
        WARNED_SHORT.store(false, AtomicOrdering::Relaxed);

        let stream = device.build_output_stream(
            &stream_cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Never ask a plugin for more than the block size it was set up
                // for. A device callback bigger than that is not expected; the
                // tail is left silent rather than written past the plugin's own
                // buffers, which is what a mismatch here actually costs.
                let frames = (out.len() / channels).min(plugin_max_block);
                if frames * channels < out.len() && !WARNED_SHORT.swap(true, AtomicOrdering::Relaxed)
                {
                    log::warn!(
                        "device asked for {} frames but the plugin was set up for {plugin_max_block}; \
                         the rest of each block is silence",
                        out.len() / channels
                    );
                }

                // The keyboard's notes, all due now.
                {
                    let mut queue = midi_events.lock().unwrap();
                    while let Some(msg) = queue.pop_front() {
                        processor.push_event(0, msg);
                    }
                }
                let frames = processor.process(frames);

                // Main output bus → the device, interleaved. The two channel
                // counts need not match: a mono plugin on a stereo device repeats
                // its last channel, a wider plugin gets its extra channels dropped.
                if frames * channels < out.len() {
                    out[frames * channels..].fill(0.0);
                }
                if processor.main_out() == 0 {
                    out.fill(0.0);
                } else {
                    for ch in 0..channels {
                        let src = processor.output(ch);
                        for frame in 0..frames {
                            out[frame * channels + ch] = src[frame];
                        }
                    }
                }
            },
            |e| log::error!("Audio error: {}", e),
            None,
        )?;

        stream.play()?;
        log::info!("Audio stream started");

        Ok(AudioEngine {
            _stream: Some(stream),
            config: AudioConfig {
                sample_rate,
                max_buffer_size,
                channels,
            },
        })
    }
}

/// Upper bound on the block size declared to a plugin.
///
/// A device's *supported* range can top out in the millions of frames — this
/// machine's reports 4 194 304 — and `setupProcessing` is a promise: a plugin
/// sizes its internal buffers for the maximum it is told. Hand over the raw
/// range maximum and a plugin with seventeen stereo buses reserves half a
/// gigabyte it will never use, which is seconds of stall on the thread that
/// asked. Real callbacks are three orders of magnitude below this cap.
pub(crate) const MAX_DECLARED_BLOCK: u32 = 16_384;

/// The block size to set a plugin up for, given what the device says it can
/// deliver. Every path that starts a plugin goes through this: the number is a
/// promise the plugin allocates against, and two copies of it drift.
pub(crate) fn declared_block_size(buffer_size: &cpal::SupportedBufferSize) -> u32 {
    match buffer_size {
        cpal::SupportedBufferSize::Range { max, .. } => (*max).min(MAX_DECLARED_BLOCK),
        _ => 512,
    }
}
