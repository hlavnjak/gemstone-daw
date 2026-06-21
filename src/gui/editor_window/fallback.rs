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
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::vst::PluginInstance;

/// Editor embedding is not implemented for this platform.
pub fn open_editor_in_thread(
    _plugin: &PluginInstance,
) -> Result<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> {
    bail!("Plugin editor embedding is not supported on this platform");
}