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
use std::ffi::{c_void, CStr};
use std::mem::zeroed;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use libloading::Library;

use crate::track_format::TrackState;

use vst3::Steinberg::{kResultOk, IPluginFactory, IPluginFactoryTrait, IPluginBaseTrait, PClassInfo};
use vst3::Steinberg::Vst::{
    IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentTrait,
    IEditController, IEditControllerTrait, IComponentHandler,
    ProcessSetup, SpeakerArr,
};
use vst3::{ComPtr, ComWrapper, Interface};

use super::handler::ParamChangeHandler;

type GetPluginFactoryProc = unsafe extern "C" fn() -> *mut IPluginFactory;

// LeSynth Fourier's host-facing analysis C ABI (see lesynth-fourier/src/lib.rs).
// The `contour` (ptr,len) carries the per-position fundamental in absolute Hz,
// uniformly resampled across the subtrack; null/0 means flat (legacy).
// Addressed to the instance carrying `token` (first argument), so the job can't
// be claimed by a different open editor. Returns 0 on success. (The plugin also
// exports an untargeted `lesynth_fourier_push_analysis`, kept for other hosts;
// it is unusable here because we open several editors at once.)
type PushAnalysisToProc =
    unsafe extern "C" fn(u64, *const f32, usize, f32, f32, *const f32, usize) -> i64;
type AnalyzeProc = unsafe extern "C" fn(
    *const f32,    // samples
    usize,         // len
    f32,           // sample_rate
    f32,           // base_freq
    *const f32,    // contour
    usize,         // contour_len
    usize,         // num_buckets
    usize,         // num_harmonics
    *mut f32,      // out_amp
    *mut f32,      // out_phase
) -> i64;
// Full analysis + offline resynthesis, the pair that lets the host reproduce the
// plugin's own grid and playback (see lesynth-fourier/src/lib.rs).
type AnalyzeFullProc = unsafe extern "C" fn(
    *const f32, // samples
    usize,      // len
    f32,        // sample_rate
    f32,        // base_freq
    *const f32, // contour
    usize,      // contour_len
    usize,      // num_buckets (0 = period-synchronous)
    usize,      // num_harmonics
    usize,      // max_buckets
    usize,      // cap_buckets
    *mut f32,   // out_amp
    *mut f32,   // out_phase
    *mut f32,   // out_pitch_ratio
    *mut f32,   // out_bucket_periods
    *mut f32,   // out_display_gain
    *mut f32,   // out_dc
    *mut f32,   // out_nyquist
) -> i64;
/// The exact inverse of the analysis — see `lesynth_fourier_resynthesize_exact`.
type ResynthesizeExactProc = unsafe extern "C" fn(
    usize,      // num_harmonics
    usize,      // num_buckets
    *const f32, // amp
    *const f32, // phase
    *const u32, // bucket_lengths
    *const f32, // dc (may be null)
    *const f32, // nyquist (may be null)
    f32,        // display_gain (0 = render at the grid's own level)
    f32,        // rate_ratio (output_rate / analysis_rate; 1.0 = exact)
    *mut f32,   // out
    usize,      // out_cap
) -> i64;
type ResynthesizeProc = unsafe extern "C" fn(
    usize,      // num_harmonics
    usize,      // num_buckets
    *const f32, // amp
    *const f32, // phase
    *const f32, // pitch_ratio
    f32,        // base_period (fractional — must not be rounded)
    usize,      // max_harmonic
    usize,      // target_samples
    f32,        // display_gain (0 = render at the grid's own level)
    *mut f32,   // out
    usize,      // out_cap
) -> i64;

/// The keyboard's own render path — see `lesynth_fourier_resynthesize_key`.
type ResynthesizeKeyProc = unsafe extern "C" fn(
    usize,       // num_harmonics
    usize,       // num_buckets
    *const f32,  // amp
    *const f32,  // phase
    *const u32,  // bucket_lengths
    *const f32,  // dc
    *const f32,  // nyquist
    *const f32,  // pitch_ratio
    f32,         // base_period (output samples, fractional)
    f32,         // base_freq
    f32,         // analysis_rate
    f32,         // out_rate
    usize,       // max_harmonic
    usize,       // target_samples
    f32,         // display_gain
    *mut f32,    // out
    usize,       // out_cap
) -> i64;

/// A key rendered through a live engine — see `lesynth_fourier_render_key_live`.
type RenderKeyLiveProc = unsafe extern "C" fn(
    *const f32, // samples
    usize,      // len
    f32,        // sample_rate
    f32,        // out_sample_rate
    f32,        // base_freq
    *const f32, // contour
    usize,      // contour_len
    usize,      // num_buckets
    usize,      // num_harmonics
    usize,      // key
    *mut i32,   // out_used_playback_grid
    *mut f32,   // out
    usize,      // out_cap
) -> i64;

// LeSynth Fourier's state save/load C ABI (see lesynth-fourier/src/lib.rs). The
// host tags an instance with a token before creating it, then exports/imports
// that exact instance's grid by token.
type PrepareInstanceProc = unsafe extern "C" fn(u64);
type ExportDimsProc =
    unsafe extern "C" fn(u64, *mut u32, *mut u32, *mut f32, *mut f32, *mut f32, *mut f32) -> i64;
/// `_export_grid` / `_import_grid` gained the exact inverse's per-bucket state
/// (lengths, DC, Nyquist) alongside the grid — see `.lsft` version 3. The three
/// pointers are nullable; passing them null exports/imports the grid alone.
type ExportGridProc = unsafe extern "C" fn(
    u64,      // token
    u32,      // num_harmonics
    u32,      // num_buckets
    *mut f32, // out_amp
    *mut f32, // out_phase
    *mut f32, // out_pitch_ratio
    *mut u32, // out_bucket_lengths
    *mut f32, // out_dc
    *mut f32, // out_nyquist
) -> i64;
type ImportGridProc = unsafe extern "C" fn(
    u64,        // token
    u32,        // num_harmonics
    u32,        // num_buckets
    f32,        // base_freq
    f32,        // duration_secs
    f32,        // sample_rate
    f32,        // display_gain (0 = unknown; no source-level audition)
    *const f32, // amp
    *const f32, // phase
    *const f32, // pitch_ratio
    *const u32, // bucket_lengths (null = no exact inverse)
    *const f32, // dc
    *const f32, // nyquist
) -> i64;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// A process-unique token used to tag a plugin instance for state export/import.
/// Pass it to [`PluginInstance::load`] right before creating the instance.
pub fn next_instance_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// The complete harmonic grid the plugin extracts from one subtrack — what
/// [`PluginInstance::analyze_full`] returns and [`PluginInstance::resynthesize`]
/// consumes. `amplitude`/`phase` are row-major `[h * num_buckets + b]`.
#[derive(Debug, Clone)]
pub struct AnalysisGrid {
    pub num_harmonics: usize,
    pub num_buckets: usize,
    pub amplitude: Vec<f32>,
    pub phase: Vec<f32>,
    /// Per-bucket fundamental relative to `base_freq` (the vibrato contour).
    pub pitch_ratio: Vec<f32>,
    /// Per-bucket length in **whole source samples** — the length of the inverse
    /// FFT that reproduces that bucket. These sum to the analysed subtrack, which
    /// is what makes [`PluginInstance::resynthesize_exact`] exact.
    pub bucket_periods: Vec<f32>,
    /// Per-bucket DC (bin 0) and Nyquist (bin `N/2`) terms. Neither is a harmonic
    /// so neither has a row in the grid or a curve on the charts, but the inverse
    /// transform needs both: dropping DC alone costs ~120 dB of reconstruction
    /// accuracy, because one pitch period of real audio does not have zero mean.
    pub dc: Vec<f32>,
    pub nyquist: Vec<f32>,
    /// Gain the plugin's display normalisation applied to this grid
    /// (`grid_amplitude = source_amplitude × display_gain`). Pass it to
    /// [`PluginInstance::resynthesize`] to get audio back at the analysed
    /// source's own level; the grid itself is scaled for chart legibility, which
    /// on a quiet recording is a boost of ~19 dB.
    pub display_gain: f32,
    pub base_freq: f32,
    pub sample_rate: f32,
    pub duration_secs: f32,
}

impl AnalysisGrid {
    pub fn amp(&self, harmonic: usize, bucket: usize) -> f32 {
        self.amplitude[harmonic * self.num_buckets + bucket]
    }

    pub fn phase(&self, harmonic: usize, bucket: usize) -> f32 {
        self.phase[harmonic * self.num_buckets + bucket]
    }

    /// Rendered period (samples, **fractional**) for `bucket` at `base_period`,
    /// matching the plugin's `bucket_period`: the base period scaled by the pitch
    /// ratio. Fractional because the renderer carries a fractional phase
    /// accumulator — rounding here would reintroduce the tuning error that
    /// accumulator exists to remove.
    pub fn rendered_period(&self, base_period: f32, bucket: usize) -> f32 {
        let r = self.pitch_ratio.get(bucket).copied().unwrap_or(1.0);
        (base_period / r.max(1e-3)).max(2.0)
    }

    /// The same grid as a [`TrackState`] — the form an instance is loaded from
    /// ([`PluginInstance::import_state`]) and a `.lsft` is written in. Lets an
    /// analysis be turned into a playable track without an editor ever opening.
    ///
    /// `bucket_periods` are whole sample counts here; they are `f32` in the grid
    /// only because that is the ABI's array type.
    pub fn to_track_state(&self) -> TrackState {
        TrackState {
            num_harmonics: self.num_harmonics,
            num_buckets: self.num_buckets,
            base_freq: self.base_freq,
            duration_secs: self.duration_secs,
            sample_rate: self.sample_rate,
            display_gain: self.display_gain,
            amplitude: self.amplitude.clone(),
            phase: self.phase.clone(),
            pitch_ratio: self.pitch_ratio.clone(),
            bucket_lengths: self.bucket_periods.iter().map(|&p| p as u32).collect(),
            dc: self.dc.clone(),
            nyquist: self.nyquist.clone(),
        }
    }
}

/// Represents a loaded and initialized VST3 plugin instance.
pub struct PluginInstance {
    pub component: ComPtr<IComponent>,
    pub processor: ComPtr<IAudioProcessor>,
    pub controller: ComPtr<IEditController>,
    // Must keep library alive as long as plugin is in use
    _library: Arc<Library>,
    /// Token this instance was tagged with (LeSynth only), so its live grid can
    /// be addressed by the state export/import C ABI. `None` for untagged loads.
    token: Option<u64>,
}

impl PluginInstance {
    /// Load a VST3 plugin from a shared library path and initialize it.
    ///
    /// `class_id`: The 16-byte class ID to look for in the plugin factory.
    /// If None, loads the first available class.
    ///
    /// `token`: if `Some`, tag this instance (via the LeSynth `prepare_instance`
    /// C ABI, called before the instance is created) so its live grid can later
    /// be exported/imported by token. Ignored for plugins lacking that symbol.
    pub fn load(
        plugin_path: &Path,
        class_id: Option<&[i8; 16]>,
        token: Option<u64>,
    ) -> Result<Self> {
        unsafe {
            let lib = Arc::new(
                Library::new(plugin_path)
                    .with_context(|| format!("Failed to open {:?}", plugin_path))?,
            );

            // Tag the next-created instance, if requested and supported. Must
            // happen before `createInstance` triggers the plugin's constructor.
            if let Some(tok) = token {
                if let Ok(prepare) =
                    lib.get::<PrepareInstanceProc>(b"lesynth_fourier_prepare_instance\0")
                {
                    prepare(tok);
                }
            }

            let get_factory: libloading::Symbol<GetPluginFactoryProc> =
                lib.get(b"GetPluginFactory\0")?;
            let raw_fac = get_factory();
            let factory: ComPtr<IPluginFactory> =
                ComPtr::from_raw(raw_fac).context("Factory pointer was null")?;
            let factory_ref = factory.as_com_ref();

            // Find the matching class ID
            let num_classes = factory_ref.countClasses();
            let mut found_cid: Option<[i8; 16]> = None;

            for idx in 0..num_classes {
                let mut info: PClassInfo = zeroed();
                factory_ref.getClassInfo(idx, &mut info);
                log::info!("Found plugin class: cid={:?}", info.cid);

                if let Some(target_cid) = class_id {
                    if info.cid == *target_cid {
                        found_cid = Some(info.cid);
                        break;
                    }
                } else {
                    // Take the first class
                    found_cid = Some(info.cid);
                    break;
                }
            }

            let cid = found_cid.context("Plugin class ID not found in factory")?;

            // Instantiate audio component
            let mut comp_ptr: *mut c_void = std::ptr::null_mut();
            let hr = factory_ref.createInstance(
                cid.as_ptr(),
                IComponent::IID.as_ptr() as *const i8,
                &mut comp_ptr,
            );
            anyhow::ensure!(
                hr == kResultOk && !comp_ptr.is_null(),
                "Failed to create IComponent"
            );

            let component: ComPtr<IComponent> =
                ComPtr::from_raw(comp_ptr as *mut IComponent).context("IComponent ptr was null")?;

            // Get controller by casting from component (nih-plug style)
            let controller: ComPtr<IEditController> = component
                .clone()
                .cast::<IEditController>()
                .context("Failed to get IEditController")?;

            let ctrl_ref = controller.as_com_ref();
            ctrl_ref.initialize(std::ptr::null_mut());

            // Set component handler for parameter changes
            let param_handler = ComWrapper::new(ParamChangeHandler);
            let param_handler_ptr = param_handler
                .to_com_ptr::<IComponentHandler>()
                .context("Failed to create component handler")?;
            ctrl_ref.setComponentHandler(param_handler_ptr.as_ptr());

            // Get audio processor
            let processor = component
                .clone()
                .cast::<IAudioProcessor>()
                .context("Not an audio processor")?;

            Ok(PluginInstance {
                component,
                processor,
                controller,
                _library: lib,
                token,
            })
        }
    }

    /// Export this instance's live harmonic grid into a [`TrackState`]. Requires
    /// the instance to have been loaded with a token (LeSynth only).
    pub fn export_state(&self) -> Result<TrackState> {
        let token = self
            .token
            .context("this instance was not tagged for export")?;
        unsafe {
            let dims: libloading::Symbol<ExportDimsProc> = self
                ._library
                .get(b"lesynth_fourier_export_dims\0")
                .context("plugin does not export lesynth_fourier_export_dims")?;
            let (mut nh, mut nb, mut base, mut dur, mut sr) = (0u32, 0u32, 0f32, 0f32, 0f32);
            let mut gain = 0f32;
            let rc = dims(token, &mut nh, &mut nb, &mut base, &mut dur, &mut sr, &mut gain);
            anyhow::ensure!(rc == 0, "export_dims failed ({rc}); instance not found");
            let (nhz, nbz) = (nh as usize, nb as usize);
            anyhow::ensure!(nhz > 0 && nbz > 0, "instance has an empty grid to export");

            let grid = nhz * nbz;
            let mut amplitude = vec![0f32; grid];
            let mut phase = vec![0f32; grid];
            let mut pitch_ratio = vec![0f32; nbz];
            let mut bucket_lengths = vec![0u32; nbz];
            let mut dc = vec![0f32; nbz];
            let mut nyquist = vec![0f32; nbz];

            let grid_fn: libloading::Symbol<ExportGridProc> = self
                ._library
                .get(b"lesynth_fourier_export_grid\0")
                .context("plugin does not export lesynth_fourier_export_grid")?;
            let rc2 = grid_fn(
                token,
                nh,
                nb,
                amplitude.as_mut_ptr(),
                phase.as_mut_ptr(),
                pitch_ratio.as_mut_ptr(),
                bucket_lengths.as_mut_ptr(),
                dc.as_mut_ptr(),
                nyquist.as_mut_ptr(),
            );
            anyhow::ensure!(rc2 >= 0, "export_grid failed ({rc2})");

            // Zeroed lengths mean this grid was never analysed period-
            // synchronously (a hand-drawn Synth grid), so there is no exact
            // inverse to save and all three are dropped together.
            if bucket_lengths.iter().any(|&n| n < 2) {
                bucket_lengths.clear();
                dc.clear();
                nyquist.clear();
            }

            let state = TrackState {
                num_harmonics: nhz,
                num_buckets: nbz,
                base_freq: base,
                duration_secs: dur,
                sample_rate: sr,
                display_gain: gain,
                amplitude,
                phase,
                pitch_ratio,
                bucket_lengths,
                dc,
                nyquist,
            };
            state.validate()?;
            Ok(state)
        }
    }

    /// Load a saved [`TrackState`] into this instance (Analysis mode). Requires
    /// the instance to have been loaded with a token (LeSynth only).
    pub fn import_state(&self, state: &TrackState) -> Result<()> {
        let token = self
            .token
            .context("this instance was not tagged for import")?;
        state.validate()?;
        unsafe {
            let import: libloading::Symbol<ImportGridProc> = self
                ._library
                .get(b"lesynth_fourier_import_grid\0")
                .context("plugin does not export lesynth_fourier_import_grid")?;
            let rc = import(
                token,
                state.num_harmonics as u32,
                state.num_buckets as u32,
                state.base_freq,
                state.duration_secs,
                state.sample_rate,
                state.display_gain,
                state.amplitude.as_ptr(),
                state.phase.as_ptr(),
                state.pitch_ratio.as_ptr(),
                // A pre-v3 file or a hand-drawn grid has no exact inverse to
                // restore; null tells the plugin to leave that path switched off
                // rather than invert a grid with lengths it never recorded.
                if state.supports_exact_inverse() {
                    state.bucket_lengths.as_ptr()
                } else {
                    std::ptr::null()
                },
                if state.supports_exact_inverse() {
                    state.dc.as_ptr()
                } else {
                    std::ptr::null()
                },
                if state.supports_exact_inverse() {
                    state.nyquist.as_ptr()
                } else {
                    std::ptr::null()
                },
            );
            anyhow::ensure!(rc == 0, "import_grid failed ({rc})");
        }
        Ok(())
    }

    /// Initialize the plugin for audio processing.
    /// Must be called before processing audio.
    pub fn initialize_audio(&self, sample_rate: f64, max_block_size: i32) -> Result<()> {
        unsafe {
            let comp_ref = self.component.as_com_ref();

            // 1) Initialize component
            comp_ref.initialize(std::ptr::null_mut());

            // 2) Set bus arrangements: no inputs, stereo output
            let inputs: [u64; 0] = [];
            let outputs: [u64; 1] = [SpeakerArr::kStereo];

            let res = self.processor.as_com_ref().setBusArrangements(
                inputs.as_ptr() as *mut _,
                inputs.len() as i32,
                outputs.as_ptr() as *mut _,
                outputs.len() as i32,
            );
            log::info!("setBusArrangements returned: {:#X}", res);

            // 3) Setup processing
            let setup = ProcessSetup {
                processMode: 0,
                sampleRate: sample_rate,
                maxSamplesPerBlock: max_block_size,
                symbolicSampleSize:
                    vst3::Steinberg::Vst::SymbolicSampleSizes_::kSample32 as i32,
            };
            self.processor
                .as_com_ref()
                .setupProcessing(&setup as *const _ as *mut _);

            // 4) Activate
            comp_ref.setActive(1);

            log::info!(
                "Plugin initialized: sr={}, block_size={}",
                sample_rate,
                max_block_size
            );
        }
        Ok(())
    }

    /// Push a recorded subtrack to this instance for analysis; its editor picks
    /// the job up, switches to Analysis mode and shows the grid.
    ///
    /// Addressed by token, so it waits for *this* instance's editor. The
    /// untargeted entry point lets an editor already open for another track
    /// claim it first, leaving this one with empty charts.
    ///
    /// `contour` is the per-position fundamental in Hz, uniformly resampled
    /// across the subtrack; empty = flat.
    pub fn push_analysis(
        &self,
        samples: &[f32],
        sample_rate: f32,
        base_freq: f32,
        contour: &[f32],
    ) -> Result<()> {
        let token = self
            .token
            .context("this instance was not tagged; cannot target an analysis push")?;
        unsafe {
            let func: libloading::Symbol<PushAnalysisToProc> = self
                ._library
                .get(b"lesynth_fourier_push_analysis_to\0")
                .context("plugin does not export lesynth_fourier_push_analysis_to")?;
            let rc = func(
                token,
                samples.as_ptr(),
                samples.len(),
                sample_rate,
                base_freq,
                contour.as_ptr(),
                contour.len(),
            );
            anyhow::ensure!(rc == 0, "push_analysis_to failed ({rc})");
        }
        Ok(())
    }

    /// Stateless harmonic analysis via the plugin's exported DSP, for the
    /// host's own preview plotting. Returns `(amp, phase)` grids shaped
    /// `[num_harmonics][num_buckets]`.
    pub fn analyze(
        &self,
        samples: &[f32],
        sample_rate: f32,
        base_freq: f32,
        contour: &[f32],
        num_buckets: usize,
        num_harmonics: usize,
    ) -> Result<(Vec<Vec<f32>>, Vec<Vec<f32>>)> {
        let mut amp_flat = vec![0.0f32; num_harmonics * num_buckets];
        let mut phase_flat = vec![0.0f32; num_harmonics * num_buckets];
        let written = unsafe {
            let func: libloading::Symbol<AnalyzeProc> = self
                ._library
                .get(b"lesynth_fourier_analyze\0")
                .context("plugin does not export lesynth_fourier_analyze")?;
            func(
                samples.as_ptr(),
                samples.len(),
                sample_rate,
                base_freq,
                contour.as_ptr(),
                contour.len(),
                num_buckets,
                num_harmonics,
                amp_flat.as_mut_ptr(),
                phase_flat.as_mut_ptr(),
            )
        };
        anyhow::ensure!(written >= 0, "plugin analyze returned error {}", written);

        let amp = (0..num_harmonics)
            .map(|h| amp_flat[h * num_buckets..(h + 1) * num_buckets].to_vec())
            .collect();
        let phase = (0..num_harmonics)
            .map(|h| phase_flat[h * num_buckets..(h + 1) * num_buckets].to_vec())
            .collect();
        Ok((amp, phase))
    }

    /// Full stateless analysis through the plugin's DSP: the same grid
    /// `analyze_and_load` builds when the plugin ingests a subtrack, including
    /// the per-bucket pitch ratio and bucket period that [`Self::analyze`]
    /// leaves out. `num_buckets == 0` selects the plugin's own
    /// period-synchronous bucketing, in which case the count is derived from the
    /// source (hence the two-call probe below).
    pub fn analyze_full(
        &self,
        samples: &[f32],
        sample_rate: f32,
        base_freq: f32,
        contour: &[f32],
        num_buckets: usize,
        num_harmonics: usize,
    ) -> Result<AnalysisGrid> {
        unsafe {
            let func: libloading::Symbol<AnalyzeFullProc> = self
                ._library
                .get(b"lesynth_fourier_analyze_full\0")
                .context("plugin does not export lesynth_fourier_analyze_full")?;

            // Probe for the bucket count the analysis will derive, then allocate
            // exactly that and repeat (the analysis is deterministic).
            let nb = func(
                samples.as_ptr(),
                samples.len(),
                sample_rate,
                base_freq,
                contour.as_ptr(),
                contour.len(),
                num_buckets,
                num_harmonics,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            anyhow::ensure!(nb > 0, "analyze_full probe returned {nb}");
            let nb = nb as usize;

            let mut amp = vec![0.0f32; num_harmonics * nb];
            let mut phase = vec![0.0f32; num_harmonics * nb];
            let mut pitch_ratio = vec![0.0f32; nb];
            let mut bucket_periods = vec![0.0f32; nb];
            let mut dc = vec![0.0f32; nb];
            let mut nyquist = vec![0.0f32; nb];
            let mut display_gain = 0.0f32;
            let rc = func(
                samples.as_ptr(),
                samples.len(),
                sample_rate,
                base_freq,
                contour.as_ptr(),
                contour.len(),
                num_buckets,
                num_harmonics,
                0,
                nb,
                amp.as_mut_ptr(),
                phase.as_mut_ptr(),
                pitch_ratio.as_mut_ptr(),
                bucket_periods.as_mut_ptr(),
                &mut display_gain,
                dc.as_mut_ptr(),
                nyquist.as_mut_ptr(),
            );
            anyhow::ensure!(rc == nb as i64, "analyze_full returned {rc}, expected {nb}");

            Ok(AnalysisGrid {
                num_harmonics,
                num_buckets: nb,
                amplitude: amp,
                phase,
                pitch_ratio,
                bucket_periods,
                dc,
                nyquist,
                display_gain,
                base_freq,
                sample_rate,
                duration_secs: if sample_rate > 0.0 {
                    samples.len() as f32 / sample_rate
                } else {
                    0.0
                },
            })
        }
    }

    /// Reproduce the analysed source from its grid — the exact inverse of
    /// [`Self::analyze_full`]. One inverse FFT per bucket returns that period's
    /// samples, and the concatenation returns the subtrack, exact to float
    /// rounding. Use [`Self::resynthesize`] for anything transposed onto a key,
    /// which must resample.
    ///
    /// `restore_source_level` divides out [`AnalysisGrid::display_gain`], giving
    /// the source file's own absolute level.
    ///
    /// `output_sample_rate` is the rate this will be played at: the grid's own
    /// rate is the bit-exact case, any other band-limit-resamples the finished
    /// reconstruction.
    pub fn resynthesize_exact(
        &self,
        grid: &AnalysisGrid,
        restore_source_level: bool,
        output_sample_rate: f32,
    ) -> Result<Vec<f32>> {
        unsafe {
            let func: libloading::Symbol<ResynthesizeExactProc> = self
                ._library
                .get(b"lesynth_fourier_resynthesize_exact\0")
                .context("plugin does not export lesynth_fourier_resynthesize_exact")?;

            let lengths: Vec<u32> = grid.bucket_periods.iter().map(|&p| p as u32).collect();
            let gain = if restore_source_level { grid.display_gain } else { 0.0 };
            // Bucket lengths are in the sample rate the audio was analysed at.
            // Rendering them into a stream at any other rate has to scale, or the
            // note plays at `out/analysis` times its pitch.
            let ratio = if grid.sample_rate > 0.0 && output_sample_rate > 0.0 {
                output_sample_rate / grid.sample_rate
            } else {
                1.0
            };
            let n = func(
                grid.num_harmonics,
                grid.num_buckets,
                grid.amplitude.as_ptr(),
                grid.phase.as_ptr(),
                lengths.as_ptr(),
                grid.dc.as_ptr(),
                grid.nyquist.as_ptr(),
                gain,
                ratio,
                std::ptr::null_mut(),
                0,
            );
            anyhow::ensure!(n > 0, "resynthesize_exact returned {n}");
            let mut out = vec![0.0f32; n as usize];
            let rc = func(
                grid.num_harmonics,
                grid.num_buckets,
                grid.amplitude.as_ptr(),
                grid.phase.as_ptr(),
                lengths.as_ptr(),
                grid.dc.as_ptr(),
                grid.nyquist.as_ptr(),
                gain,
                ratio,
                out.as_mut_ptr(),
                out.len(),
            );
            anyhow::ensure!(rc == n, "resynthesize_exact returned {rc}, expected {n}");
            Ok(out)
        }
    }

    /// Render a grid back to audio through the plugin's own playback path, with
    /// no plugin state involved. `base_period` is the **fractional** rendered
    /// fundamental period in samples (`sample_rate / freq`, unrounded),
    /// `max_harmonic` an anti-alias cap (`0` = none beyond Nyquist), and
    /// `target_samples` the Analysis-mode "preserve seconds" length (`0` = one
    /// cycle per bucket).
    ///
    /// `restore_source_level` divides out [`AnalysisGrid::display_gain`], giving
    /// the source's own absolute level — the only form comparable against the
    /// file. `false` renders at the grid's level, which is what a key plays.
    pub fn resynthesize(
        &self,
        grid: &AnalysisGrid,
        base_period: f32,
        max_harmonic: usize,
        target_samples: usize,
        restore_source_level: bool,
    ) -> Result<Vec<f32>> {
        unsafe {
            let func: libloading::Symbol<ResynthesizeProc> = self
                ._library
                .get(b"lesynth_fourier_resynthesize\0")
                .context("plugin does not export lesynth_fourier_resynthesize")?;
            let call = |out: *mut f32, cap: usize| {
                func(
                    grid.num_harmonics,
                    grid.num_buckets,
                    grid.amplitude.as_ptr(),
                    grid.phase.as_ptr(),
                    grid.pitch_ratio.as_ptr(),
                    base_period,
                    max_harmonic,
                    target_samples,
                    if restore_source_level { grid.display_gain } else { 0.0 },
                    out,
                    cap,
                )
            };
            let n = call(std::ptr::null_mut(), 0);
            anyhow::ensure!(n >= 0, "resynthesize returned error {n}");
            let mut out = vec![0.0f32; n as usize];
            let written = call(out.as_mut_ptr(), out.len());
            anyhow::ensure!(written == n, "resynthesize length changed ({written} vs {n})");
            Ok(out)
        }
    }

    /// Render a key **the way the keyboard does** — through the plugin's
    /// `PlaybackGrid` and the source's own two clocks.
    ///
    /// [`Self::resynthesize`] is not that path: it passes a pitch contour and no
    /// bucket lengths, so the plugin renders the analysis grid's *rounded*
    /// buckets on a uniform time grid. A key does neither, so a dump made
    /// through it cannot show a keyboard defect. Use this one to ask why a key
    /// buzzes.
    ///
    /// `base_period` is the key's period in output samples, fractional.
    #[allow(clippy::too_many_arguments)]
    pub fn resynthesize_key(
        &self,
        grid: &AnalysisGrid,
        base_period: f32,
        max_harmonic: usize,
        target_samples: usize,
        output_sample_rate: f32,
        restore_source_level: bool,
    ) -> Result<Vec<f32>> {
        unsafe {
            let func: libloading::Symbol<ResynthesizeKeyProc> = self
                ._library
                .get(b"lesynth_fourier_resynthesize_key\0")
                .context("plugin does not export lesynth_fourier_resynthesize_key")?;
            let lengths: Vec<u32> = grid.bucket_periods.iter().map(|&p| p as u32).collect();
            let call = |out: *mut f32, cap: usize| {
                func(
                    grid.num_harmonics,
                    grid.num_buckets,
                    grid.amplitude.as_ptr(),
                    grid.phase.as_ptr(),
                    lengths.as_ptr(),
                    grid.dc.as_ptr(),
                    grid.nyquist.as_ptr(),
                    grid.pitch_ratio.as_ptr(),
                    base_period,
                    grid.base_freq,
                    grid.sample_rate,
                    output_sample_rate,
                    max_harmonic,
                    target_samples,
                    if restore_source_level { grid.display_gain } else { 0.0 },
                    out,
                    cap,
                )
            };
            let n = call(std::ptr::null_mut(), 0);
            anyhow::ensure!(n >= 0, "resynthesize_key returned error {n}");
            let mut out = vec![0.0f32; n as usize];
            let written = call(out.as_mut_ptr(), out.len());
            anyhow::ensure!(written == n, "resynthesize_key length changed ({written} vs {n})");
            Ok(out)
        }
    }

    /// Render a key through a **live engine** inside the plugin — the same
    /// `load_analysis` + `assemble_buffer_for_key` an editor key press makes.
    ///
    /// [`Self::resynthesize_key`] calls the renderer with a `PlaybackGrid` built
    /// for it, so it can only ever prove the renderer sound. The live path
    /// builds that grid from the plugin's own `SharedParams` and falls back to
    /// the contour renderer whenever anything is missing — silently, same call,
    /// different signal. `used_playback_grid` in the result is `false` when that
    /// happened, which is the answer to "the offline dump is clean but the
    /// plugin still buzzes".
    #[allow(clippy::too_many_arguments)]
    pub fn render_key_live(
        &self,
        samples: &[f32],
        sample_rate: f32,
        out_sample_rate: f32,
        base_freq: f32,
        contour: &[f32],
        num_buckets: usize,
        num_harmonics: usize,
        key: usize,
    ) -> Result<(Vec<f32>, bool)> {
        unsafe {
            let func: libloading::Symbol<RenderKeyLiveProc> = self
                ._library
                .get(b"lesynth_fourier_render_key_live\0")
                .context("plugin does not export lesynth_fourier_render_key_live")?;
            let mut used: i32 = 0;
            let call = |out: *mut f32, cap: usize, used: *mut i32| {
                func(
                    samples.as_ptr(),
                    samples.len(),
                    sample_rate,
                    out_sample_rate,
                    base_freq,
                    // The pitch contour, which this used to pass as null. Without
                    // it the engine analyses a *flat* source: every bucket gets
                    // the same true period, the render tracks none of the
                    // recording's vibrato, and it comes out 52 dB from the same
                    // key rendered through `resynthesize_key` — which reads as
                    // "the keyboard does not play what the offline dump
                    // measures" when in fact the probe was asking for a
                    // different note.
                    if contour.is_empty() { std::ptr::null() } else { contour.as_ptr() },
                    contour.len(),
                    num_buckets,
                    num_harmonics,
                    key,
                    used,
                    out,
                    cap,
                )
            };
            let n = call(std::ptr::null_mut(), 0, std::ptr::null_mut());
            anyhow::ensure!(n >= 0, "render_key_live returned error {n}");
            let mut out = vec![0.0f32; n as usize];
            let written = call(out.as_mut_ptr(), out.len(), &mut used);
            anyhow::ensure!(written == n, "render_key_live length changed ({written} vs {n})");
            Ok((out, used != 0))
        }
    }

    /// Create plugin editor view (returns raw pointer for window embedding).
    /// Returns None if the plugin has no editor.
    pub fn create_view(&self) -> Option<ComPtr<vst3::Steinberg::IPlugView>> {
        unsafe {
            let ctrl_ref = self.controller.as_com_ref();
            let raw_view =
                ctrl_ref.createView(CStr::from_bytes_with_nul(b"editor\0").unwrap().as_ptr());
            if raw_view.is_null() {
                None
            } else {
                ComPtr::from_raw(raw_view as *mut vst3::Steinberg::IPlugView)
            }
        }
    }
}

/// Well-known class IDs
pub mod class_ids {
    /// LeSynth Fourier: ASCII bytes of "LeSynthFourier01"
    pub const FOURIER_SYNTH: [i8; 16] = [
        76, 101, 83, 121, 110, 116, 104, 70, 111, 117, 114, 105, 101, 114, 48, 49,
    ];
}