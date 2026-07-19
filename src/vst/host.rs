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
type PushAnalysisProc =
    unsafe extern "C" fn(*const f32, usize, f32, f32, *const f32, usize) -> u64;
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

// LeSynth Fourier's state save/load C ABI (see lesynth-fourier/src/lib.rs). The
// host tags an instance with a token before creating it, then exports/imports
// that exact instance's grid by token.
type PrepareInstanceProc = unsafe extern "C" fn(u64);
type ExportDimsProc =
    unsafe extern "C" fn(u64, *mut u32, *mut u32, *mut f32, *mut f32, *mut f32) -> i64;
type ExportGridProc =
    unsafe extern "C" fn(u64, u32, u32, *mut f32, *mut f32, *mut f32) -> i64;
type ImportGridProc = unsafe extern "C" fn(
    u64,        // token
    u32,        // num_harmonics
    u32,        // num_buckets
    f32,        // base_freq
    f32,        // duration_secs
    f32,        // sample_rate
    *const f32, // amp
    *const f32, // phase
    *const f32, // pitch_ratio
) -> i64;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// A process-unique token used to tag a plugin instance for state export/import.
/// Pass it to [`PluginInstance::load`] right before creating the instance.
pub fn next_instance_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
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
            let rc = dims(token, &mut nh, &mut nb, &mut base, &mut dur, &mut sr);
            anyhow::ensure!(rc == 0, "export_dims failed ({rc}); instance not found");
            let (nhz, nbz) = (nh as usize, nb as usize);
            anyhow::ensure!(nhz > 0 && nbz > 0, "instance has an empty grid to export");

            let grid = nhz * nbz;
            let mut amplitude = vec![0f32; grid];
            let mut phase = vec![0f32; grid];
            let mut pitch_ratio = vec![0f32; nbz];

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
            );
            anyhow::ensure!(rc2 >= 0, "export_grid failed ({rc2})");

            let state = TrackState {
                num_harmonics: nhz,
                num_buckets: nbz,
                base_freq: base,
                duration_secs: dur,
                sample_rate: sr,
                amplitude,
                phase,
                pitch_ratio,
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
                state.amplitude.as_ptr(),
                state.phase.as_ptr(),
                state.pitch_ratio.as_ptr(),
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

    /// Push a recorded subtrack to this plugin (same shared object) for
    /// Fourier analysis. The running editor will pick the job up, switch to
    /// Analysis mode and display the extracted amplitude/phase grid.
    ///
    /// Returns the queue depth reported by the plugin. `contour` is the
    /// per-position fundamental (absolute Hz) uniformly resampled across the
    /// subtrack; pass an empty slice for flat (legacy) analysis.
    pub fn push_analysis(
        &self,
        samples: &[f32],
        sample_rate: f32,
        base_freq: f32,
        contour: &[f32],
    ) -> Result<u64> {
        unsafe {
            let func: libloading::Symbol<PushAnalysisProc> = self
                ._library
                .get(b"lesynth_fourier_push_analysis\0")
                .context("plugin does not export lesynth_fourier_push_analysis")?;
            Ok(func(
                samples.as_ptr(),
                samples.len(),
                sample_rate,
                base_freq,
                contour.as_ptr(),
                contour.len(),
            ))
        }
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