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
//! A look-ahead peak limiter: the ceiling a signal meets on its way out.
//!
//! Something has to stop a boosted track from leaving full scale, and the cheap
//! answer — clamping every sample at ±1.0 — is the worst one. A clamp is a
//! nonlinearity applied to single samples, so it does not turn a loud passage
//! down, it flattens the top off every waveform in it and fills the spectrum
//! with what was never played. On speech that lands on the plosives: a "p"
//! boosted ten times has four fifths of its samples clipped, and the harshness
//! that comes back is broadband, above everything else in the mix.
//!
//! This turns the signal down instead, and does it *before* the peak arrives.
//! The stream is cut into chunks of [`LOOK_MS`]; each one is held until the
//! following chunk's peak is known, and then played out under a gain that ramps
//! linearly between boundary values, never above what either neighbouring chunk
//! can take. So the gain is already down when the transient lands — there is no
//! overshoot to clip, and no sample is touched by anything but a smooth,
//! signal-wide level change. Recovery is a slow one-pole ([`RELEASE_MS`]), so
//! one plosive does not audibly duck what follows it.
//!
//! The price is [`Limiter::latency_frames`] of delay, two chunks' worth, which
//! the caller has to account for: the transport adds it to the latency it
//! reports, and an offline render drops it off the front.
//!
//! Below the ceiling the gain is exactly 1.0 and samples pass through
//! unaltered — a mix that never reaches full scale is bit-for-bit what it was
//! before this stood in the path.

use std::collections::VecDeque;

/// How far ahead the limiter looks, in milliseconds. One chunk is also the
/// length of the gain ramp into a peak: long enough not to modulate the body of
/// a voice, short enough that a transient is not turned down long before it.
const LOOK_MS: f64 = 2.0;

/// How long the gain takes to come back, as a one-pole time constant. Slow
/// enough that a single plosive is one dip rather than a stutter through the
/// syllable behind it.
const RELEASE_MS: f64 = 120.0;

/// One held chunk: its samples and the most gain they may be played at.
struct Chunk {
    samples: Vec<f32>,
    /// `min(1, ceiling / peak)` — unity for a chunk that never reaches the
    /// ceiling, which is the common case.
    gain: f32,
}

/// A look-ahead peak limiter over an interleaved buffer. Channels are linked:
/// the gain is computed from the loudest sample of the frame group, so a peak
/// on the left does not pull the image over.
pub struct Limiter {
    channels: usize,
    /// Frames per chunk, the look-ahead.
    chunk: usize,
    ceiling: f32,
    /// Per-frame release coefficient.
    release: f32,
    /// Input not yet forming a whole chunk.
    pending: Vec<f32>,
    /// The chunk waiting for its successor's peak. `None` only for the very
    /// first chunk of the stream.
    held: Option<Chunk>,
    /// Gain at the boundary the next chunk starts from.
    boundary: f32,
    /// The release envelope, carried across chunks.
    env: f32,
    /// Output waiting to be handed back, primed with the latency in silence.
    out: VecDeque<f32>,
}

impl Limiter {
    /// A limiter for `channels` interleaved channels at `sample_rate`, holding
    /// everything at or below `ceiling`.
    pub fn new(channels: usize, sample_rate: f64, ceiling: f32) -> Self {
        let channels = channels.max(1);
        let chunk = ((sample_rate * LOOK_MS / 1000.0).round() as usize).max(1);
        let span = chunk * channels;
        let mut pending = Vec::with_capacity(span);
        pending.clear();
        // Three chunks is the most that is ever queued: the two of latency the
        // stream is primed with, plus the one an input block can complete.
        let mut out = VecDeque::with_capacity(3 * span + 1);
        out.extend(std::iter::repeat_n(0.0, 2 * span));
        Self {
            channels,
            chunk,
            ceiling,
            release: (-1.0 / (sample_rate * RELEASE_MS / 1000.0)).exp() as f32,
            pending,
            held: None,
            boundary: 1.0,
            env: 1.0,
            out,
        }
    }

    /// How far behind its input the output runs, in frames. Constant for the
    /// life of the limiter, and the same for every channel.
    pub fn latency_frames(&self) -> usize {
        2 * self.chunk
    }

    /// Limit `buf` in place. The buffer comes back the same length, holding the
    /// audio [`Self::latency_frames`] earlier in the stream.
    pub fn process(&mut self, buf: &mut [f32]) {
        let span = self.chunk * self.channels;
        // Everything in first — the output written below is older audio, and
        // would otherwise overwrite input still to be read.
        let mut read = 0;
        while read < buf.len() {
            let take = (span - self.pending.len()).min(buf.len() - read);
            self.pending.extend_from_slice(&buf[read..read + take]);
            read += take;
            if self.pending.len() == span {
                self.close_chunk();
            }
        }
        // Never short: each input chunk queues one output chunk, and the two
        // primed at the start cover the two the limiter is holding.
        for s in buf.iter_mut() {
            *s = self.out.pop_front().unwrap_or(0.0);
        }
    }

    /// Take the full chunk out of `pending`, play out the one held behind it,
    /// and hold this one in its place.
    fn close_chunk(&mut self) {
        let peak = self.pending.iter().fold(0f32, |m, s| m.max(s.abs()));
        let gain = if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        };
        match self.held.take() {
            Some(mut held) => {
                // The gain the boundary between the two chunks is played at.
                // Both ends of a chunk's ramp are at or below what the chunk
                // itself can take, so the whole ramp is, and nothing
                // overshoots.
                let target = held.gain.min(gain);
                self.emit(&held.samples, target);
                // The vectors are swapped rather than copied: past the first
                // two chunks a stream this way allocates nothing at all.
                std::mem::swap(&mut held.samples, &mut self.pending);
                held.gain = gain;
                self.held = Some(held);
            }
            // The first chunk of the stream has nothing behind it to play out.
            None => {
                let mut samples = Vec::with_capacity(self.pending.len());
                std::mem::swap(&mut samples, &mut self.pending);
                self.held = Some(Chunk { samples, gain });
            }
        }
        self.pending.clear();
    }

    /// Play one held chunk out under a gain ramping from the standing boundary
    /// to `target`, with the release envelope over the top.
    fn emit(&mut self, samples: &[f32], target: f32) {
        let step = (target - self.boundary) / self.chunk as f32;
        let mut ramp = self.boundary;
        for frame in samples.chunks(self.channels) {
            ramp += step;
            // The ramp is the ceiling on the gain; the envelope drops to it at
            // once and comes back slowly. So the release never lets the gain
            // exceed what the chunk can take.
            self.env = if ramp <= self.env {
                ramp
            } else {
                ramp + (self.env - ramp) * self.release
            };
            for s in frame {
                self.out.push_back(s * self.env);
            }
        }
        self.boundary = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 48_000.0;

    fn run(lim: &mut Limiter, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        for chunk in input.chunks(block) {
            let mut buf = chunk.to_vec();
            lim.process(&mut buf);
            out.extend_from_slice(&buf);
        }
        out
    }

    /// A signal that never reaches the ceiling comes back sample for sample,
    /// only later. Anything else would colour every quiet mix in the app.
    #[test]
    fn below_the_ceiling_nothing_is_touched() {
        let mut lim = Limiter::new(2, SR, 1.0);
        let input: Vec<f32> = (0..4000)
            .map(|n| 0.9 * (n as f32 * 0.01).sin())
            .collect();
        let out = run(&mut lim, &input, 173);
        let d = lim.latency_frames() * 2; // frames -> interleaved samples
        for (n, (&got, &want)) in out[d..].iter().zip(input.iter()).enumerate() {
            assert_eq!(got, want, "sample {n} came back changed");
        }
    }

    /// Nothing leaves the limiter above the ceiling — not on the first sample
    /// of a transient, which is exactly what a clamp is left to catch.
    #[test]
    fn a_transient_never_overshoots() {
        let mut lim = Limiter::new(1, SR, 1.0);
        let mut input = vec![0.1f32; 8000];
        // A plosive: nothing, then five times full scale for 3 ms.
        for s in input.iter_mut().skip(4000).take(144) {
            *s = 5.0;
        }
        let out = run(&mut lim, &input, 512);
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 1.0 + 1e-6, "left the limiter at {peak}");
        // And it is a level change, not a flattened top: the burst keeps its
        // shape, so its samples are all the same value as each other.
        let burst: Vec<f32> = out
            .iter()
            .copied()
            .filter(|s| s.abs() > 0.5)
            .collect();
        assert!(burst.len() > 100, "the burst did not survive: {}", burst.len());
    }

    /// The gain comes back after a peak instead of holding the whole track
    /// down — but slowly, over the release rather than at once.
    #[test]
    fn the_gain_returns_after_the_peak() {
        let mut lim = Limiter::new(1, SR, 1.0);
        let mut input = vec![0.5f32; 24_000];
        for s in input.iter_mut().skip(1000).take(96) {
            *s = 4.0;
        }
        let out = run(&mut lim, &input, 256);
        let at = |n: usize| out[n];
        // Right after the burst the level is still down.
        assert!(at(1300) < 0.45, "no gain reduction left at all: {}", at(1300));
        // Half a second later it is back, bar the tail of the one-pole.
        assert!(
            (at(23_000) - 0.5).abs() < 0.5 * 0.03,
            "the gain never came back: {}",
            at(23_000)
        );
    }

    /// Block size is the device's business, not the limiter's: the same input
    /// gives the same output however it is cut up.
    #[test]
    fn the_result_does_not_depend_on_the_block_size() {
        let input: Vec<f32> = (0..6000)
            .map(|n| 2.0 * (n as f32 * 0.013).sin() * (n as f32 * 0.0004).sin())
            .collect();
        let a = run(&mut Limiter::new(2, SR, 1.0), &input, 64);
        let b = run(&mut Limiter::new(2, SR, 1.0), &input, 1024);
        assert_eq!(a.len(), b.len());
        for (n, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert!((x - y).abs() < 1e-9, "sample {n}: {x} vs {y}");
        }
    }
}
