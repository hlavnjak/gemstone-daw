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
//! Offline render dump — the debugging counterpart to a loopback capture.
//!
//! Decodes a source file, segments it, analyses one subtrack through a real
//! LeSynth Fourier instance and writes the renders straight to disk as 32-bit
//! float wavs. Nothing here touches cpal, the audio engine's ring buffer or the
//! system mixer, so a defect that shows up in these files is the plugin's, and
//! one that shows up only in a loopback capture is the host path's or
//! PulseAudio's resampler.
//!
//! Two renders come out, because they are different code paths and only one of
//! them is supposed to be exact:
//!
//! * `exact.wav` — [`PluginInstance::resynthesize_exact`], one inverse FFT per
//!   bucket at the analysis rate. This is the transform's inverse; against
//!   `source.wav` it should sit at the float noise floor.
//! * `key_<hz>.wav` — [`PluginInstance::resynthesize_key`], the path the
//!   keyboard plays through: the plugin's `PlaybackGrid`, one true fractional
//!   period per bucket, walked on the source's own clock. Not exact by design,
//!   but it must not buzz. **This is the one to scan for a keyboard buzz.**
//! * `contour_<hz>.wav` — [`PluginInstance::resynthesize`], the host bridge's
//!   path: the analysis grid's *rounded* buckets on a uniform time grid. A
//!   different signal, and the one this tool used to dump alone — which is how
//!   a keyboard defect stayed invisible to it.
//!
//! Feed the results to `tools/buzzscan.py`, which reads these float wavs
//! directly.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use gemstone_daw::analysis;
use gemstone_daw::audio::decode_audio_file;
use gemstone_daw::vst::{class_ids, AnalysisGrid, PluginInstance};

const USAGE: &str = "\
dump_render — offline LeSynth Fourier renders for buzz debugging

USAGE:
    dump_render <SOURCE.wav|mp3|m4a> [OPTIONS]

OPTIONS:
    --out DIR         output directory              [default: target/dump]
    --subtrack N      which reasonable subtrack     [default: the longest]
    --list            list the subtracks and exit
    --harmonics N     harmonics to analyse          [default: 256]
    --buckets N       0 = the plugin's own period-synchronous bucketing
                                                    [default: 0]
    --note HZ         render the playback path here [default: subtrack f0]
    --rate HZ         output rate for exact.wav     [default: analysis rate]
    --plugin PATH     plugin .so/.dll   [default: internal_plugins/liblesynth_fourier.so]

    --key N           piano key for the live-engine render  [default: nearest]
    --synth-timeline  one cycle per bucket instead of the source's wall clock
    --max-harmonic N  cap the rendered band (0 = only Nyquist)

Writes source.wav, exact.wav, key_<hz>.wav and contour_<hz>.wav as 32-bit float,
then prints the buzzscan command to run over them.
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    let opt = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let num = |name: &str, default: f32| -> Result<f32> {
        match opt(name) {
            Some(v) => v.parse().with_context(|| format!("{name}: not a number: {v}")),
            None => Ok(default),
        }
    };

    let source = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .context("no source file given")?;
    let out_dir = PathBuf::from(opt("--out").unwrap_or_else(|| "target/dump".into()));
    let num_harmonics = num("--harmonics", 256.0)? as usize;
    let num_buckets = num("--buckets", 0.0)? as usize;
    let plugin_path = opt("--plugin").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("internal_plugins")
            .join("liblesynth_fourier.so")
    });

    if !source.exists() {
        bail!("source not found: {}", source.display());
    }
    if !plugin_path.exists() {
        bail!(
            "internal plugin not built: {} (run `make build`)",
            plugin_path.display()
        );
    }

    // 1) Decode and segment — the same front end the GUI runs.
    let audio = decode_audio_file(&source)
        .with_context(|| format!("decode {}", source.display()))?;
    println!(
        "source    : {} — {:.3}s, {} samples @ {} Hz",
        source.display(),
        audio.duration_secs(),
        audio.samples.len(),
        audio.sample_rate
    );

    let subs = analysis::segment(&audio.samples, audio.sample_rate);
    let reasonable: Vec<_> = subs
        .into_iter()
        .filter(|s| s.is_reasonable(audio.sample_rate))
        .collect();
    if reasonable.is_empty() {
        bail!("no analysable subtrack found in {}", source.display());
    }

    let list = args.iter().any(|a| a == "--list");
    if list {
        println!("\n{} reasonable subtrack(s):", reasonable.len());
    }
    for (i, s) in reasonable.iter().enumerate() {
        if list {
            println!(
                "  [{i}] {:8.2} Hz  {:6.3}s  conf {:.2}  samples {}..{}",
                s.base_freq,
                s.duration_secs(audio.sample_rate),
                s.confidence,
                s.start,
                s.end
            );
        }
    }
    if list {
        return Ok(());
    }

    // Default to the longest subtrack: the most periods, so a per-period defect
    // has the most chances to show up and the recurrence histogram has support.
    let idx = match opt("--subtrack") {
        Some(v) => v.parse::<usize>().context("--subtrack: not an index")?,
        None => reasonable
            .iter()
            .enumerate()
            .max_by_key(|(_, s)| s.len())
            .map(|(i, _)| i)
            .unwrap_or(0),
    };
    let sub = reasonable
        .get(idx)
        .with_context(|| format!("--subtrack {idx} out of range (0..{})", reasonable.len()))?;
    println!(
        "subtrack  : [{idx}] {:.2} Hz, {:.3}s, conf {:.2}, samples {}..{}",
        sub.base_freq,
        sub.duration_secs(audio.sample_rate),
        sub.confidence,
        sub.start,
        sub.end
    );

    // 2) Analyse through a real plugin instance.
    let plugin = PluginInstance::load(&plugin_path, Some(&class_ids::FOURIER_SYNTH), None)
        .with_context(|| format!("load {}", plugin_path.display()))?;

    let end = sub.end.min(audio.samples.len());
    let samples = &audio.samples[sub.start..end];
    let contour = analysis::build_contour(sub);
    let grid: AnalysisGrid = plugin
        .analyze_full(
            samples,
            audio.sample_rate,
            sub.base_freq,
            &contour,
            num_buckets,
            num_harmonics,
        )
        .context("plugin analyze_full")?;
    println!(
        "grid      : {} harmonics x {} buckets, display_gain {:.4}",
        grid.num_harmonics, grid.num_buckets, grid.display_gain
    );
    report_bucket_periods(&grid);

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create {}", out_dir.display()))?;

    // The two per-bucket columns an offline analysis needs to reconstruct the
    // renderer's own clock: the recorded length and the pitch contour. Together
    // they are `true_periods`, and without them a Python reproduction of the
    // grain pipeline has to guess the period and measures its own guess.
    {
        let path = out_dir.join("buckets.txt");
        let mut f = BufWriter::new(File::create(&path)?);
        writeln!(f, "# length pitch_ratio  (base_freq {} rate {})", grid.base_freq, grid.sample_rate)?;
        for b in 0..grid.num_buckets {
            writeln!(
                f,
                "{} {}",
                grid.bucket_periods.get(b).copied().unwrap_or(0.0),
                grid.pitch_ratio.get(b).copied().unwrap_or(1.0)
            )?;
        }
    }

    // 3) The reference: the analysed samples themselves.
    let src_path = out_dir.join("source.wav");
    write_wav_f32(&src_path, samples, audio.sample_rate)?;
    println!("\nwrote {}  ({} samples)", src_path.display(), samples.len());

    // 4) The exact inverse. At the analysis rate this is the bit-exact case; a
    //    different --rate band-limit-resamples the finished reconstruction, so
    //    any difference there is the resampler's, not the transform's.
    let out_rate = num("--rate", grid.sample_rate)?;
    let exact = plugin
        .resynthesize_exact(&grid, true, out_rate)
        .context("resynthesize_exact")?;
    let exact_path = out_dir.join("exact.wav");
    write_wav_f32(&exact_path, &exact, out_rate)?;
    println!(
        "wrote {}   ({} samples @ {:.0} Hz){}",
        exact_path.display(),
        exact.len(),
        out_rate,
        if (out_rate - grid.sample_rate).abs() < 0.5 {
            ""
        } else {
            "  [resampled — not the exact case]"
        }
    );
    if (out_rate - grid.sample_rate).abs() < 0.5 {
        report_diff(samples, &exact);
    }

    // 5) The two transposing paths, which are *not* the same signal.
    //
    // `key_<hz>.wav` is what the keyboard plays: the plugin's `PlaybackGrid`,
    // one true fractional period per bucket, walked on the source's own clock.
    // `contour_<hz>.wav` is the host bridge's path — the analysis grid's rounded
    // buckets on a uniform time grid — which is what this tool used to dump and
    // call "the path a key plays". It is kept because the bridge is real and a
    // regression in it matters, but a keyboard defect is only visible in the
    // first, and comparing the two localises the rounding directly.
    let note = num("--note", sub.base_freq)?;
    // Which piano key the live render presses. Default: the one nearest the
    // source's own pitch, so it is comparable with everything else here.
    let key_index = match opt("--key") {
        Some(v) => v.parse::<usize>().context("--key: not a number")?,
        None => ((12.0 * (sub.base_freq / 27.5).log2()).round() as i64).clamp(0, 87) as usize,
    };
    if note > 0.0 {
        let base_period = grid.sample_rate / note;
        // `--synth-timeline` renders one cycle per bucket instead of walking the
        // source's wall clock. It is a different note (its length follows the
        // key), but a bucket is then exactly one rendered cycle, so a bucket
        // change can only ever land on a cycle boundary. Driven by time the
        // bucket is chosen from the wall clock every sample, and once the key
        // transposes, that lands the change in the middle of a cycle. Comparing
        // the two says whether that matters.
        let synth_timeline = args.iter().any(|a| a == "--synth-timeline");
        // `--max-harmonic N` caps the rendered band. A source is usually
        // band-limited well below Nyquist (a lossy codec lowpasses it), and the
        // analysis fits *noise* into the harmonics above that. Rendered as a
        // periodic waveform that noise repeats identically every cycle and adds
        // coherently, turning hiss into a stable comb at the top of the band.
        let max_harmonic = match opt("--max-harmonic") {
            Some(v) => v.parse::<usize>().context("--max-harmonic: not a number")?,
            None => 0,
        };
        let key = plugin
            .resynthesize_key(
                &grid,
                base_period,
                max_harmonic,
                if synth_timeline { 0 } else { samples.len() },
                grid.sample_rate,
                true,
            )
            .context("resynthesize_key")?;
        let key_path = out_dir.join(format!("key_{note:.0}hz.wav"));
        write_wav_f32(&key_path, &key, grid.sample_rate)?;
        println!(
            "wrote {}  ({} samples, base_period {:.3})",
            key_path.display(),
            key.len(),
            base_period
        );

        // 5b) The same key, but rendered by a live engine inside the plugin —
        // `load_analysis` then `assemble_buffer_for_key`, the calls an editor key
        // press makes. This is the only render here that can show an integration
        // fault: the live path builds its own `PlaybackGrid` from `SharedParams`
        // and falls back to the contour renderer, silently, if anything is
        // missing.
        let key_hz = 27.5f32 * 2f32.powf(key_index as f32 / 12.0);
        match plugin.render_key_live(
            samples,
            grid.sample_rate,
            grid.sample_rate,
            sub.base_freq,
            &contour,
            0,
            grid.num_harmonics,
            key_index,
        ) {
            Ok((live, used_grid)) => {
                let live_path = out_dir.join(format!("live_key{key_index}_{key_hz:.0}hz.wav"));
                write_wav_f32(&live_path, &live, grid.sample_rate)?;
                println!(
                    "wrote {}  ({} samples, live engine, key {} = {:.1} Hz)",
                    live_path.display(),
                    live.len(),
                    key_index,
                    key_hz
                );
                println!(
                    "          PlaybackGrid: {}",
                    if used_grid {
                        "USED"
                    } else {
                        "*** NOT USED — the key fell back to the contour path ***"
                    }
                );
            }
            Err(e) => println!("live-engine render unavailable: {e:#}"),
        }

        let play = plugin
            .resynthesize(&grid, base_period, 0, samples.len(), true)
            .context("resynthesize")?;
        let play_path = out_dir.join(format!("contour_{note:.0}hz.wav"));
        write_wav_f32(&play_path, &play, grid.sample_rate)?;
        println!(
            "wrote {}  ({} samples, the host-bridge contour path)",
            play_path.display(),
            play.len()
        );
    }

    let scan = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/buzzscan.py");
    println!("\nnow scan them:");
    println!("  python3 {} {}/exact.wav", scan.display(), out_dir.display());
    println!("  python3 {} {}/key_*.wav", scan.display(), out_dir.display());
    println!(
        "  python3 {} {}/key_*.wav --repair {}",
        scan.display(),
        out_dir.display(),
        out_dir.display()
    );
    Ok(())
}

/// Bucket periods are fractional, and every stage that rounds one is a place a
/// once-per-period seam can be born — so print what the grid actually carries
/// and how much of it a `u32` truncation would throw away.
fn report_bucket_periods(grid: &AnalysisGrid) {
    let p = &grid.bucket_periods;
    if p.is_empty() {
        return;
    }
    let min = p.iter().copied().fold(f32::INFINITY, f32::min);
    let max = p.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let sum: f32 = p.iter().sum();
    let frac: f32 = p.iter().map(|v| (v - v.floor()).abs()).sum();
    let non_integer = p.iter().filter(|v| (*v - v.floor()).abs() > 1e-6).count();
    println!(
        "periods   : {:.3}..{:.3} samples (mean {:.3}), {}/{} non-integer, \
         truncation loses {:.2} samples total",
        min,
        max,
        sum / p.len() as f32,
        non_integer,
        p.len(),
        frac
    );
}

/// Sample-aligned difference over the overlapping span. Reported as a level, not
/// a correlation: correlation stays near 1.0 through defects that are plainly
/// audible, so it is the wrong instrument for this.
fn report_diff(a: &[f32], b: &[f32]) {
    let n = a.len().min(b.len());
    if n == 0 {
        return;
    }
    let (mut se, mut sa, mut peak, mut at) = (0.0f64, 0.0f64, 0.0f32, 0usize);
    for i in 0..n {
        let d = (a[i] - b[i]).abs();
        if d > peak {
            peak = d;
            at = i;
        }
        se += (d as f64) * (d as f64);
        sa += (a[i] as f64) * (a[i] as f64);
    }
    let db = 10.0 * ((se / sa.max(1e-30)).max(1e-30)).log10();
    println!(
        "          vs source: {db:.1} dB residual over {n} samples \
         (len {} vs {}), peak |diff| {peak:.6} at sample {at}",
        a.len(),
        b.len()
    );
}

/// 32-bit float mono WAV. Float so the dump adds no quantisation of its own —
/// a 16-bit dump would put a ±0.00003 floor under every measurement made on it.
fn write_wav_f32(path: &Path, samples: &[f32], sample_rate: f32) -> Result<()> {
    let f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut w = BufWriter::new(f);
    let rate = sample_rate.max(1.0) as u32;
    let data_len = (samples.len() * 4) as u32;
    let byte_rate = rate * 4;

    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    w.write_all(&3u16.to_le_bytes())?; // WAVE_FORMAT_IEEE_FLOAT
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&4u16.to_le_bytes())?; // block align
    w.write_all(&32u16.to_le_bytes())?; // bits per sample
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        w.write_all(&s.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}
