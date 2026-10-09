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
//! Hosting an **LV2** plugin.
//!
//! An LV2 plugin is a bundle — `Foo.lv2/` — holding a shared library and the
//! Turtle that describes it: `manifest.ttl` names each plugin by URI and points
//! at its binary and its data files, and those list its ports. Nothing about a
//! plugin is learnt from the binary except the `lv2_descriptor` entry point; the
//! ports, their kinds and defaults, and the features the plugin requires all
//! come from the Turtle, read by [`turtle`].
//!
//! Everything a plugin needs from a host is a *feature* it is handed at
//! instantiation: URID mapping, options (the sample rate and block sizes),
//! a worker for jobs too slow for the audio thread, a log. A plugin that
//! requires a feature this host does not offer is refused with its name.
//!
//! Because the sample rate is fixed at instantiation, the plugin is
//! instantiated in [`Lv2Instance::initialize_audio`], not when it is loaded; a
//! state handed over before then is kept and restored straight after.
//!
//! Ports are the whole audio interface: audio ports become the instance's
//! buses, control ports hold one float each (the defaults, or whatever the
//! editor or a saved state sets), and notes arrive as MIDI events in an atom
//! sequence on the first atom input that supports them.

pub mod turtle;
mod ui;

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{bail, Context, Result};
use libloading::Library;

use self::turtle::{percent_decode, Graph, Node};
use super::processor::{AudioScratch, MidiEvent, RealtimeProcess};
use super::{FoundPlugin, PluginEditor, PluginFormat, PluginIo};

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

pub(crate) const LV2: &str = "http://lv2plug.in/ns/lv2core#";
const ATOM: &str = "http://lv2plug.in/ns/ext/atom#";
const MIDI_EVENT: &str = "http://lv2plug.in/ns/ext/midi#MidiEvent";
const RDFS_SEE_ALSO: &str = "http://www.w3.org/2000/01/rdf-schema#seeAlso";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const DOAP_NAME: &str = "http://usefulinc.com/ns/doap#name";
const RSZ_MINIMUM_SIZE: &str = "http://lv2plug.in/ns/ext/resize-port#minimumSize";
pub(crate) const UI_NS: &str = "http://lv2plug.in/ns/extensions/ui#";

pub(crate) const URID_MAP: &str = "http://lv2plug.in/ns/ext/urid#map";
pub(crate) const URID_UNMAP: &str = "http://lv2plug.in/ns/ext/urid#unmap";
pub(crate) const OPTIONS_OPTIONS: &str = "http://lv2plug.in/ns/ext/options#options";
const BUF_BOUNDED: &str = "http://lv2plug.in/ns/ext/buf-size#boundedBlockLength";
const BUF_MIN: &str = "http://lv2plug.in/ns/ext/buf-size#minBlockLength";
const BUF_MAX: &str = "http://lv2plug.in/ns/ext/buf-size#maxBlockLength";
const BUF_NOMINAL: &str = "http://lv2plug.in/ns/ext/buf-size#nominalBlockLength";
const BUF_SEQUENCE_SIZE: &str = "http://lv2plug.in/ns/ext/buf-size#sequenceSize";
pub(crate) const PARAM_SAMPLE_RATE: &str = "http://lv2plug.in/ns/ext/parameters#sampleRate";
const WORKER_SCHEDULE: &str = "http://lv2plug.in/ns/ext/worker#schedule";
const WORKER_INTERFACE: &str = "http://lv2plug.in/ns/ext/worker#interface";
pub(crate) const LOG_LOG: &str = "http://lv2plug.in/ns/ext/log#log";
const STATE_INTERFACE: &str = "http://lv2plug.in/ns/ext/state#interface";
pub(crate) const ATOM_INT: &str = "http://lv2plug.in/ns/ext/atom#Int";
pub(crate) const ATOM_FLOAT: &str = "http://lv2plug.in/ns/ext/atom#Float";
const ATOM_SEQUENCE: &str = "http://lv2plug.in/ns/ext/atom#Sequence";
const ATOM_CHUNK: &str = "http://lv2plug.in/ns/ext/atom#Chunk";
const ATOM_FRAME_TIME: &str = "http://lv2plug.in/ns/ext/atom#frameTime";

/// Features a plugin may *require* that this host satisfies. Some are host
/// features handed over at instantiation; the rest are plugin properties that
/// ask nothing of a host which never shares a buffer between ports.
const SUPPORTED_FEATURES: &[&str] = &[
    URID_MAP,
    URID_UNMAP,
    OPTIONS_OPTIONS,
    BUF_BOUNDED,
    WORKER_SCHEDULE,
    LOG_LOG,
    "http://lv2plug.in/ns/lv2core#hardRTCapable",
    "http://lv2plug.in/ns/lv2core#inPlaceBroken",
    "http://lv2plug.in/ns/lv2core#isLive",
    "http://lv2plug.in/ns/ext/urid#map",
];

// ---------------------------------------------------------------------------
// The C ABI
// ---------------------------------------------------------------------------

#[repr(C)]
pub(crate) struct Lv2Feature {
    pub uri: *const c_char,
    pub data: *mut c_void,
}

pub(crate) type ExtensionDataFn = unsafe extern "C" fn(*const c_char) -> *const c_void;

#[repr(C)]
struct Lv2Descriptor {
    uri: *const c_char,
    instantiate: Option<
        unsafe extern "C" fn(*const Lv2Descriptor, f64, *const c_char, *const *const Lv2Feature) -> *mut c_void,
    >,
    connect_port: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void)>,
    activate: Option<unsafe extern "C" fn(*mut c_void)>,
    run: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    deactivate: Option<unsafe extern "C" fn(*mut c_void)>,
    cleanup: Option<unsafe extern "C" fn(*mut c_void)>,
    extension_data: Option<ExtensionDataFn>,
}

type DescriptorFn = unsafe extern "C" fn(u32) -> *const Lv2Descriptor;

#[repr(C)]
pub(crate) struct UridMap {
    pub handle: *mut c_void,
    pub map: unsafe extern "C" fn(*mut c_void, *const c_char) -> u32,
}

#[repr(C)]
pub(crate) struct UridUnmap {
    pub handle: *mut c_void,
    pub unmap: unsafe extern "C" fn(*mut c_void, u32) -> *const c_char,
}

#[repr(C)]
pub(crate) struct OptionsOption {
    pub context: u32,
    pub subject: u32,
    pub key: u32,
    pub size: u32,
    pub type_: u32,
    pub value: *const c_void,
}

#[repr(C)]
struct WorkerSchedule {
    handle: *mut c_void,
    schedule_work: unsafe extern "C" fn(*mut c_void, u32, *const c_void) -> u32,
}

type WorkerRespondFn = unsafe extern "C" fn(*mut c_void, u32, *const c_void) -> u32;

#[repr(C)]
struct WorkerInterface {
    work: Option<unsafe extern "C" fn(*mut c_void, WorkerRespondFn, *mut c_void, u32, *const c_void) -> u32>,
    work_response: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void) -> u32>,
    end_run: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
}

/// `LV2_Log_Log`. Its two functions are C-variadic, which stable Rust cannot
/// define; they are declared here with their fixed arguments only, which every
/// calling convention this host targets passes the same way, and the format
/// string is logged as written.
#[repr(C)]
pub(crate) struct LogLog {
    pub handle: *mut c_void,
    pub printf: unsafe extern "C" fn(*mut c_void, u32, *const c_char) -> i32,
    pub vprintf: unsafe extern "C" fn(*mut c_void, u32, *const c_char, *mut c_void) -> i32,
}

type StoreFn = unsafe extern "C" fn(*mut c_void, u32, *const c_void, usize, u32, u32) -> u32;
type RetrieveFn = unsafe extern "C" fn(*mut c_void, u32, *mut usize, *mut u32, *mut u32) -> *const c_void;

#[repr(C)]
struct StateInterface {
    save: Option<unsafe extern "C" fn(*mut c_void, StoreFn, *mut c_void, u32, *const *const Lv2Feature) -> u32>,
    restore: Option<unsafe extern "C" fn(*mut c_void, RetrieveFn, *mut c_void, u32, *const *const Lv2Feature) -> u32>,
}

/// `LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE`.
const STATE_FLAGS: u32 = 1 | 2;

// ---------------------------------------------------------------------------
// URIDs
// ---------------------------------------------------------------------------

/// The process-wide URI ↔ URID table. One table for every instance and every
/// UI: a URID a plugin hands its UI must mean the same thing on both sides.
struct UridTable {
    by_uri: HashMap<String, u32>,
    /// Index `n` is URID `n + 1`. A `CString`'s buffer never moves, so the
    /// pointers `unmap` hands out stay valid for the life of the process.
    uris: Vec<CString>,
}

static URIDS: Mutex<Option<UridTable>> = Mutex::new(None);

pub(crate) fn urid(uri: &str) -> u32 {
    let mut guard = URIDS.lock().unwrap();
    let table = guard.get_or_insert_with(|| UridTable { by_uri: HashMap::new(), uris: Vec::new() });
    if let Some(&id) = table.by_uri.get(uri) {
        return id;
    }
    table.uris.push(CString::new(uri).unwrap_or_default());
    let id = table.uris.len() as u32;
    table.by_uri.insert(uri.to_string(), id);
    id
}

pub(crate) fn uri_of(id: u32) -> Option<String> {
    let guard = URIDS.lock().unwrap();
    let table = guard.as_ref()?;
    table
        .uris
        .get(id.checked_sub(1)? as usize)
        .map(|c| c.to_string_lossy().into_owned())
}

pub(crate) unsafe extern "C" fn urid_map(_handle: *mut c_void, uri: *const c_char) -> u32 {
    if uri.is_null() {
        return 0;
    }
    urid(&CStr::from_ptr(uri).to_string_lossy())
}

pub(crate) unsafe extern "C" fn urid_unmap(_handle: *mut c_void, id: u32) -> *const c_char {
    let guard = URIDS.lock().unwrap();
    match guard.as_ref().and_then(|t| t.uris.get(id.wrapping_sub(1) as usize)) {
        Some(c) => c.as_ptr(),
        None => std::ptr::null(),
    }
}

pub(crate) unsafe extern "C" fn log_printf(_handle: *mut c_void, _type: u32, fmt: *const c_char) -> i32 {
    if !fmt.is_null() {
        log::info!("LV2 plugin: {}", CStr::from_ptr(fmt).to_string_lossy().trim_end());
    }
    0
}

pub(crate) unsafe extern "C" fn log_vprintf(
    handle: *mut c_void,
    type_: u32,
    fmt: *const c_char,
    _args: *mut c_void,
) -> i32 {
    log_printf(handle, type_, fmt)
}

// ---------------------------------------------------------------------------
// Bundles
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum PortKind {
    Audio,
    Control,
    Cv,
    Atom,
    Other,
}

#[derive(Clone, Debug)]
struct PortInfo {
    index: u32,
    symbol: String,
    kind: PortKind,
    input: bool,
    default: f32,
    /// An atom port that takes MIDI.
    midi: bool,
    /// Bytes the plugin asks its atom buffer to hold.
    min_size: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct UiInfo {
    pub uri: String,
    pub class: String,
    pub binary: PathBuf,
    /// Every feature the UI lists, required or optional. Direct access to the
    /// plugin instance is handed only to a UI that asks for it: one that can
    /// do without (DPF's) sets parameters on the plugin directly when offered
    /// it, behind the control ports a saved state is read from.
    pub features: Vec<String>,
}

/// One plugin, as its bundle describes it.
#[derive(Clone, Debug)]
pub(crate) struct PluginInfo {
    pub uri: String,
    pub name: String,
    pub bundle: PathBuf,
    binary: PathBuf,
    ports: Vec<PortInfo>,
    required_features: Vec<String>,
    classes: Vec<String>,
    pub uis: Vec<UiInfo>,
}

/// The bundle directory behind whatever was picked: the bundle itself, its
/// `manifest.ttl`, or a library inside it.
fn bundle_dir(path: &Path) -> Result<PathBuf> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .with_context(|| format!("{} is not inside an LV2 bundle", crate::file_label(path)))?
    };
    anyhow::ensure!(
        dir.join("manifest.ttl").is_file(),
        "{} is not an LV2 bundle (it has no manifest.ttl)",
        crate::file_label(&dir)
    );
    Ok(dir)
}

/// The `file:` IRI a path is known by inside the bundle's Turtle.
fn file_iri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/").replace('%', "%25");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// The path a `file:` IRI names.
fn iri_path(iri: &str) -> Option<PathBuf> {
    let rest = iri.strip_prefix("file://")?;
    let rest = percent_decode(rest);
    // `file:///C:/x` on Windows is the path `C:/x`.
    let rest = if rest.len() > 2 && rest.as_bytes()[2] == b':' && rest.starts_with('/') {
        rest[1..].to_string()
    } else {
        rest
    };
    Some(PathBuf::from(rest))
}

/// Read a bundle: its manifest and every data file the manifest points at.
fn read_bundle(dir: &Path) -> Result<Graph> {
    let mut graph = Graph::default();
    let manifest = dir.join("manifest.ttl");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("read {}", manifest.display()))?;
    graph
        .parse(&text, &file_iri(&manifest))
        .with_context(|| format!("in {}", manifest.display()))?;
    let mut files: Vec<PathBuf> = graph
        .triples
        .iter()
        .filter(|t| t.predicate == RDFS_SEE_ALSO)
        .filter_map(|t| t.object.iri().and_then(iri_path))
        .filter(|p| p.starts_with(dir) && p.is_file())
        .collect();
    files.sort();
    files.dedup();
    for file in files {
        let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        graph
            .parse(&text, &file_iri(&file))
            .with_context(|| format!("in {}", file.display()))?;
    }
    Ok(graph)
}

/// Every plugin the bundle at `dir` describes.
fn plugins_in(dir: &Path) -> Result<Vec<PluginInfo>> {
    let graph = read_bundle(dir)?;
    let plugin_class = Node::Iri(format!("{LV2}Plugin"));
    let rdf_type = turtle::RDF_TYPE;
    let mut uris: Vec<String> = graph
        .subjects(rdf_type, &plugin_class)
        .filter_map(|n| n.iri().map(str::to_string))
        .collect();
    uris.dedup();
    let mut out = Vec::new();
    for uri in uris {
        let subject = Node::Iri(uri.clone());
        let Some(binary) = graph
            .object(&subject, &format!("{LV2}binary"))
            .and_then(Node::iri)
            .and_then(iri_path)
        else {
            continue;
        };
        let name = graph
            .object(&subject, DOAP_NAME)
            .or_else(|| graph.object(&subject, RDFS_LABEL))
            .and_then(Node::text)
            .map(str::to_string)
            .unwrap_or_else(|| uri.rsplit(['/', '#', ':']).next().unwrap_or(&uri).to_string());

        let mut ports = Vec::new();
        for port in graph.objects(&subject, &format!("{LV2}port")) {
            let is = |class: &str| graph.is_a(port, class);
            let kind = if is(&format!("{LV2}AudioPort")) {
                PortKind::Audio
            } else if is(&format!("{LV2}ControlPort")) {
                PortKind::Control
            } else if is(&format!("{LV2}CVPort")) {
                PortKind::Cv
            } else if is(&format!("{ATOM}AtomPort")) {
                PortKind::Atom
            } else {
                PortKind::Other
            };
            let Some(index) = graph
                .object(port, &format!("{LV2}index"))
                .and_then(Node::number)
            else {
                continue;
            };
            let num = |p: &str| graph.object(port, &format!("{LV2}{p}")).and_then(Node::number);
            let (min, max) = (num("minimum"), num("maximum"));
            let default = num("default").or(min).unwrap_or(0.0).clamp(
                min.unwrap_or(f64::MIN),
                max.unwrap_or(f64::MAX).max(min.unwrap_or(f64::MIN)),
            );
            ports.push(PortInfo {
                index: index as u32,
                symbol: graph
                    .object(port, &format!("{LV2}symbol"))
                    .and_then(Node::text)
                    .unwrap_or_default()
                    .to_string(),
                kind,
                input: is(&format!("{LV2}InputPort")),
                default: default as f32,
                midi: graph
                    .objects(port, &format!("{ATOM}supports"))
                    .any(|o| o.iri() == Some(MIDI_EVENT)),
                min_size: graph
                    .object(port, RSZ_MINIMUM_SIZE)
                    .and_then(Node::number)
                    .unwrap_or(0.0) as usize,
            });
        }
        ports.sort_by_key(|p| p.index);

        let required_features = graph
            .objects(&subject, &format!("{LV2}requiredFeature"))
            .filter_map(|n| n.iri().map(str::to_string))
            .collect();
        let classes = graph
            .objects(&subject, rdf_type)
            .filter_map(|n| n.iri())
            .filter_map(|c| c.strip_prefix(LV2).map(str::to_string))
            .collect();
        let uis = graph
            .objects(&subject, &format!("{UI_NS}ui"))
            .filter_map(|ui| {
                let class = graph
                    .objects(ui, rdf_type)
                    .filter_map(Node::iri)
                    .find(|c| c.starts_with(UI_NS))?
                    .to_string();
                let binary = graph
                    .object(ui, &format!("{UI_NS}binary"))
                    .or_else(|| graph.object(ui, &format!("{LV2}binary")))
                    .and_then(Node::iri)
                    .and_then(iri_path)?;
                let features = graph
                    .objects(ui, &format!("{LV2}requiredFeature"))
                    .chain(graph.objects(ui, &format!("{LV2}optionalFeature")))
                    .filter_map(|n| n.iri().map(str::to_string))
                    .collect();
                Some(UiInfo { uri: ui.iri()?.to_string(), class, binary, features })
            })
            .collect();

        out.push(PluginInfo {
            uri,
            name,
            bundle: dir.to_path_buf(),
            binary,
            ports,
            required_features,
            classes,
            uis,
        });
    }
    Ok(out)
}

/// The plugin `plugin_id` names in the bundle at `path`, or with none the
/// first instrument, else the first plugin.
fn find_plugin(path: &Path, plugin_id: Option<&str>) -> Result<PluginInfo> {
    let dir = bundle_dir(path)?;
    let plugins = plugins_in(&dir)?;
    match plugin_id {
        Some(id) => plugins
            .into_iter()
            .find(|p| p.uri == id)
            .with_context(|| format!("{} holds no plugin <{id}>", crate::file_label(&dir))),
        None => {
            let pick = plugins
                .iter()
                .position(|p| p.classes.iter().any(|c| c == "InstrumentPlugin"))
                .unwrap_or(0);
            plugins
                .into_iter()
                .nth(pick)
                .with_context(|| format!("{} describes no plugins", crate::file_label(&dir)))
        }
    }
}

/// The required features this host does not provide.
fn missing_features(info: &PluginInfo) -> Vec<String> {
    info.required_features
        .iter()
        .filter(|f| !SUPPORTED_FEATURES.contains(&f.as_str()))
        .cloned()
        .collect()
}

pub fn validate(path: &Path, plugin_id: Option<&str>) -> Result<()> {
    let info = find_plugin(path, plugin_id)?;
    let missing = missing_features(&info);
    anyhow::ensure!(
        missing.is_empty(),
        "'{}' requires LV2 features this host does not provide: {}",
        info.name,
        missing.join(", ")
    );
    let library = unsafe { Library::new(&info.binary) }
        .with_context(|| format!("Failed to open {}", crate::file_label(&info.binary)))?;
    unsafe { descriptor(&library, &info) }?;
    Ok(())
}

/// Every plugin in the bundle at `path`, for the picker. A bundle that holds
/// only specifications (as the system's `lv2/` directory is full of) lists
/// nothing.
pub fn list_plugins(path: &Path) -> Vec<FoundPlugin> {
    match plugins_in(path) {
        Ok(plugins) => plugins
            .into_iter()
            .map(|p| FoundPlugin {
                name: p.name,
                format: PluginFormat::Lv2,
                path: path.to_path_buf(),
                plugin_id: Some(p.uri),
            })
            .collect(),
        Err(e) => {
            log::debug!("LV2 scan: {} — {e:#}", path.display());
            Vec::new()
        }
    }
}

/// A plugin's name and its LV2 classes, `|`-separated.
pub fn describe(path: &Path, plugin_id: Option<&str>) -> Option<(String, String)> {
    let info = find_plugin(path, plugin_id).ok()?;
    Some((info.name.clone(), info.classes.join("|")))
}

/// The plugin's descriptor, out of the library's `lv2_descriptor` list.
unsafe fn descriptor(library: &Library, info: &PluginInfo) -> Result<*const Lv2Descriptor> {
    let entry = library
        .get::<DescriptorFn>(b"lv2_descriptor\0")
        .with_context(|| format!("{} exports no lv2_descriptor", crate::file_label(&info.binary)))?;
    for i in 0.. {
        let d = entry(i);
        if d.is_null() {
            break;
        }
        if !(*d).uri.is_null() && CStr::from_ptr((*d).uri).to_string_lossy() == info.uri {
            return Ok(d);
        }
    }
    bail!("{} does not contain <{}>", crate::file_label(&info.binary), info.uri)
}

// ---------------------------------------------------------------------------
// Host features
// ---------------------------------------------------------------------------

/// Jobs a plugin handed to its worker, and the worker's answers on their way
/// back. Requests made during `run()` are worked straight after it; one made
/// anywhere else is worked on the spot, which the worker spec allows outside
/// the audio thread.
#[derive(Default)]
struct Worker {
    requests: Mutex<Vec<Vec<u8>>>,
    responses: Mutex<Vec<Vec<u8>>>,
    in_run: AtomicBool,
    /// The plugin's own worker interface and instance, once there is one.
    iface: Mutex<Option<(usize, usize)>>,
}

unsafe extern "C" fn worker_schedule(handle: *mut c_void, size: u32, data: *const c_void) -> u32 {
    let Some(worker) = (handle as *const Worker).as_ref() else { return 1 };
    let bytes = if data.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data as *const u8, size as usize).to_vec()
    };
    if worker.in_run.load(Ordering::Acquire) {
        worker.requests.lock().unwrap().push(bytes);
        return 0;
    }
    match *worker.iface.lock().unwrap() {
        Some((iface, handle)) => {
            let iface = &*(iface as *const WorkerInterface);
            if let Some(work) = iface.work {
                work(
                    handle as *mut c_void,
                    worker_respond,
                    worker as *const Worker as *mut c_void,
                    bytes.len() as u32,
                    bytes.as_ptr() as *const c_void,
                );
            }
            0
        }
        None => 1,
    }
}

unsafe extern "C" fn worker_respond(handle: *mut c_void, size: u32, data: *const c_void) -> u32 {
    let Some(worker) = (handle as *const Worker).as_ref() else { return 1 };
    let bytes = if data.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data as *const u8, size as usize).to_vec()
    };
    worker.responses.lock().unwrap().push(bytes);
    0
}

/// Everything handed to the plugin as features, kept at one address for as
/// long as the plugin instance lives.
struct HostFeatures {
    map: UridMap,
    unmap: UridUnmap,
    options: Vec<OptionsOption>,
    /// The values the options point at.
    option_values: Box<[u32; 4]>,
    sample_rate: Box<f32>,
    worker: Box<Worker>,
    schedule: WorkerSchedule,
    log: LogLog,
    uris: Vec<CString>,
    features: Vec<Lv2Feature>,
    /// The null-terminated pointer array `instantiate` takes.
    list: Vec<*const Lv2Feature>,
}

impl HostFeatures {
    fn new(sample_rate: f64, max_block: u32, sequence_size: u32) -> Box<Self> {
        let mut host = Box::new(HostFeatures {
            map: UridMap { handle: std::ptr::null_mut(), map: urid_map },
            unmap: UridUnmap { handle: std::ptr::null_mut(), unmap: urid_unmap },
            options: Vec::new(),
            option_values: Box::new([1, max_block, max_block, sequence_size]),
            sample_rate: Box::new(sample_rate as f32),
            worker: Box::default(),
            schedule: WorkerSchedule {
                handle: std::ptr::null_mut(),
                schedule_work: worker_schedule,
            },
            log: LogLog {
                handle: std::ptr::null_mut(),
                printf: log_printf,
                vprintf: log_vprintf,
            },
            uris: Vec::new(),
            features: Vec::new(),
            list: Vec::new(),
        });
        let int = urid(ATOM_INT);
        let values = host.option_values.as_ptr();
        let opt = |key: &str, type_: u32, size: usize, value: *const c_void| OptionsOption {
            context: 0,
            subject: 0,
            key: urid(key),
            size: size as u32,
            type_,
            value,
        };
        host.options = vec![
            opt(BUF_MIN, int, 4, values as *const c_void),
            opt(BUF_MAX, int, 4, unsafe { values.add(1) } as *const c_void),
            opt(BUF_NOMINAL, int, 4, unsafe { values.add(2) } as *const c_void),
            opt(BUF_SEQUENCE_SIZE, int, 4, unsafe { values.add(3) } as *const c_void),
            opt(
                PARAM_SAMPLE_RATE,
                urid(ATOM_FLOAT),
                4,
                &*host.sample_rate as *const f32 as *const c_void,
            ),
            OptionsOption { context: 0, subject: 0, key: 0, size: 0, type_: 0, value: std::ptr::null() },
        ];
        host.schedule.handle = &*host.worker as *const Worker as *mut c_void;

        let entries: Vec<(&str, *mut c_void)> = vec![
            (URID_MAP, &host.map as *const UridMap as *mut c_void),
            (URID_UNMAP, &host.unmap as *const UridUnmap as *mut c_void),
            (OPTIONS_OPTIONS, host.options.as_ptr() as *mut c_void),
            (BUF_BOUNDED, std::ptr::null_mut()),
            (WORKER_SCHEDULE, &host.schedule as *const WorkerSchedule as *mut c_void),
            (LOG_LOG, &host.log as *const LogLog as *mut c_void),
        ];
        host.uris = entries.iter().map(|(u, _)| CString::new(*u).unwrap()).collect();
        host.features = entries
            .iter()
            .zip(&host.uris)
            .map(|((_, data), uri)| Lv2Feature { uri: uri.as_ptr(), data: *data })
            .collect();
        host.list = host
            .features
            .iter()
            .map(|f| f as *const Lv2Feature)
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        host
    }
}

// ---------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------

/// What exists once the plugin has been instantiated.
struct Live {
    handle: *mut c_void,
    /// Must outlive `handle`: the plugin keeps pointers into it.
    host: Box<HostFeatures>,
    state: *const StateInterface,
    worker: *const WorkerInterface,
}

/// A loaded LV2 plugin.
pub struct Lv2Instance {
    info: Arc<PluginInfo>,
    descriptor: *const Lv2Descriptor,
    live: Mutex<Option<Live>>,
    /// One value per port index. Control ports are connected straight to
    /// these, so an editor's write is the plugin's next read. Atomic so the
    /// editor thread and the audio thread can share them.
    controls: Arc<[AtomicU32]>,
    /// A state handed over before instantiation, restored straight after.
    pending_state: Mutex<Option<Vec<u8>>>,
    /// Held by whoever must not run at the same time as `run()` — a state
    /// restore — and tried by the audio thread, which renders silence rather
    /// than wait.
    run_lock: Arc<Mutex<()>>,
    /// Atoms an editor sent to the plugin, delivered at the next block.
    ui_events: Arc<Mutex<Vec<(u32, u32, Vec<u8>)>>>,
    io: RwLock<PluginIo>,
    // Dropped last: the plugin's code lives in it.
    _library: Library,
}

// The instance is shared between the GUI, editor and audio threads; the
// plugin's own threading classes are kept by the locks above.
unsafe impl Send for Lv2Instance {}
unsafe impl Sync for Lv2Instance {}

impl Lv2Instance {
    pub fn load(path: &Path, plugin_id: Option<&str>) -> Result<Self> {
        let info = find_plugin(path, plugin_id)?;
        let missing = missing_features(&info);
        anyhow::ensure!(
            missing.is_empty(),
            "'{}' requires LV2 features this host does not provide: {}",
            info.name,
            missing.join(", ")
        );
        let library = unsafe { Library::new(&info.binary) }
            .with_context(|| format!("Failed to open {}", crate::file_label(&info.binary)))?;
        let descriptor = unsafe { descriptor(&library, &info) }?;
        let port_count = info.ports.iter().map(|p| p.index as usize + 1).max().unwrap_or(0);
        let controls: Arc<[AtomicU32]> = (0..port_count)
            .map(|i| {
                let default = info
                    .ports
                    .iter()
                    .find(|p| p.index as usize == i)
                    .map_or(0.0, |p| p.default);
                AtomicU32::new(default.to_bits())
            })
            .collect();
        log::info!(
            "Loaded LV2 '{}' <{}> from {} ({} ports)",
            info.name,
            info.uri,
            info.bundle.display(),
            info.ports.len()
        );
        Ok(Self {
            info: Arc::new(info),
            descriptor,
            live: Mutex::new(None),
            controls,
            pending_state: Mutex::new(None),
            run_lock: Arc::new(Mutex::new(())),
            ui_events: Arc::new(Mutex::new(Vec::new())),
            io: RwLock::new(PluginIo::default()),
            _library: library,
        })
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn io(&self) -> PluginIo {
        self.io.read().unwrap().clone()
    }

    fn desc(&self) -> &Lv2Descriptor {
        unsafe { &*self.descriptor }
    }

    fn audio_ports(&self, input: bool) -> Vec<&PortInfo> {
        self.info
            .ports
            .iter()
            .filter(|p| p.kind == PortKind::Audio && p.input == input)
            .collect()
    }

    pub fn initialize_audio(&self, sample_rate: f64, max_block_size: i32) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        if live.is_some() {
            return Ok(());
        }
        let max_block = max_block_size.max(1) as u32;
        let sequence_size = self
            .info
            .ports
            .iter()
            .filter(|p| p.kind == PortKind::Atom)
            .map(|p| atom_capacity(p.min_size))
            .max()
            .unwrap_or(8192) as u32;
        let host = HostFeatures::new(sample_rate, max_block, sequence_size);
        let mut bundle = self.info.bundle.to_string_lossy().into_owned();
        if !bundle.ends_with(std::path::MAIN_SEPARATOR) {
            bundle.push(std::path::MAIN_SEPARATOR);
        }
        let bundle = CString::new(bundle).context("bundle path contains a NUL byte")?;
        let instantiate = self.desc().instantiate.context("descriptor has no instantiate")?;
        let handle = unsafe { instantiate(self.descriptor, sample_rate, bundle.as_ptr(), host.list.as_ptr()) };
        anyhow::ensure!(!handle.is_null(), "'{}' refused to instantiate at {sample_rate} Hz", self.info.name);

        let ext = |uri: &str| -> *const c_void {
            let c = CString::new(uri).unwrap();
            match self.desc().extension_data {
                Some(f) => unsafe { f(c.as_ptr()) },
                None => std::ptr::null(),
            }
        };
        let state = ext(STATE_INTERFACE) as *const StateInterface;
        let worker = ext(WORKER_INTERFACE) as *const WorkerInterface;
        if !worker.is_null() {
            *host.worker.iface.lock().unwrap() = Some((worker as usize, handle as usize));
        }

        // Every control port, connected to its value for good. Audio, CV and
        // atom ports are connected by the processor that owns their buffers.
        if let Some(connect) = self.desc().connect_port {
            for port in self.info.ports.iter().filter(|p| p.kind == PortKind::Control) {
                let slot = &self.controls[port.index as usize];
                unsafe { connect(handle, port.index, slot.as_ptr() as *mut c_void) };
            }
        }
        *live = Some(Live { handle, host, state, worker });

        // A state handed over before there was an instance to give it to.
        if let Some(bytes) = self.pending_state.lock().unwrap().take() {
            if let Err(e) = self.restore_into(live.as_ref().unwrap(), &bytes) {
                log::warn!("'{}' state restore failed: {e:#}", self.info.name);
            }
        }
        if let Some(activate) = self.desc().activate {
            unsafe { activate(handle) };
        }

        let io = PluginIo {
            inputs: if self.audio_ports(true).is_empty() {
                Vec::new()
            } else {
                vec![self.audio_ports(true).len()]
            },
            outputs: if self.audio_ports(false).is_empty() {
                Vec::new()
            } else {
                vec![self.audio_ports(false).len()]
            },
            max_block: max_block as usize,
        };
        log::info!(
            "LV2 '{}' instantiated: sr={sample_rate}, block={max_block}, in={:?}, out={:?}",
            self.info.name,
            io.inputs,
            io.outputs
        );
        *self.io.write().unwrap() = io;
        Ok(())
    }

    /// The plugin's own state (where it has the state interface) and every
    /// input control port's value, by symbol.
    pub fn save_state(&self) -> Result<Vec<u8>> {
        let live = self.live.lock().unwrap();
        let mut out = StateWriter::default();
        for port in self.info.ports.iter().filter(|p| p.kind == PortKind::Control && p.input) {
            let value = f32::from_bits(self.controls[port.index as usize].load(Ordering::Relaxed));
            out.controls.push((port.symbol.clone(), value));
        }
        if let Some(live) = live.as_ref() {
            if let Some(save) = unsafe { live.state.as_ref() }.and_then(|s| s.save) {
                let _guard = self.run_lock.lock().unwrap();
                let features: [*const Lv2Feature; 1] = [std::ptr::null()];
                unsafe {
                    save(
                        live.handle,
                        state_store,
                        &mut out as *mut StateWriter as *mut c_void,
                        STATE_FLAGS,
                        features.as_ptr(),
                    );
                }
            }
        } else if let Some(pending) = self.pending_state.lock().unwrap().clone() {
            // Never instantiated: what it would have been restored to.
            return Ok(pending);
        }
        Ok(out.encode())
    }

    pub fn restore_state(&self, bytes: &[u8]) -> Result<()> {
        let live = self.live.lock().unwrap();
        match live.as_ref() {
            Some(live) => self.restore_into(live, bytes),
            None => {
                StateWriter::decode(bytes)?;
                *self.pending_state.lock().unwrap() = Some(bytes.to_vec());
                Ok(())
            }
        }
    }

    fn restore_into(&self, live: &Live, bytes: &[u8]) -> Result<()> {
        let state = StateWriter::decode(bytes)?;
        let _guard = self.run_lock.lock().unwrap();
        for (symbol, value) in &state.controls {
            if let Some(port) = self.info.ports.iter().find(|p| p.symbol == *symbol && p.kind == PortKind::Control) {
                self.controls[port.index as usize].store(value.to_bits(), Ordering::Relaxed);
            }
        }
        if !state.properties.is_empty() {
            if let Some(restore) = unsafe { live.state.as_ref() }.and_then(|s| s.restore) {
                let mut reader = StateReader {
                    properties: state
                        .properties
                        .iter()
                        .map(|(k, t, f, v)| (urid(k), urid(t), *f, v.clone()))
                        .collect(),
                };
                let features: [*const Lv2Feature; 1] = [std::ptr::null()];
                unsafe {
                    restore(
                        live.handle,
                        state_retrieve,
                        &mut reader as *mut StateReader as *mut c_void,
                        STATE_FLAGS,
                        features.as_ptr(),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn has_editor(&self) -> bool {
        ui::pick_ui(&self.info).is_some()
    }

    pub fn create_editor(&self) -> Result<Box<dyn PluginEditor>> {
        let live = self.live.lock().unwrap();
        let live = live
            .as_ref()
            .context("the plugin has to be running before its editor can open")?;
        ui::create(
            self.info.clone(),
            live.handle,
            self.desc().extension_data,
            self.controls.clone(),
            self.ui_events.clone(),
            *live.host.sample_rate,
        )
    }

    pub(crate) fn realtime(
        &self,
        _scratch: &mut AudioScratch,
        in_buses: &[usize],
        out_buses: &[usize],
        max_block: usize,
    ) -> Result<Box<dyn RealtimeProcess>> {
        let live = self.live.lock().unwrap();
        let live = live
            .as_ref()
            .context("the plugin has to be instantiated before it can be processed")?;
        let connect = self.desc().connect_port.context("descriptor has no connect_port")?;
        let run = self.desc().run.context("descriptor has no run")?;

        let in_channels: usize = in_buses.iter().sum();
        let out_channels: usize = out_buses.iter().sum();
        let audio_in: Vec<u32> = self.audio_ports(true).iter().map(|p| p.index).collect();
        let audio_out: Vec<u32> = self.audio_ports(false).iter().map(|p| p.index).collect();
        // Every audio port gets a scratch channel; one beyond the buses the
        // processor laid out gets a buffer of its own.
        let mut spare: Vec<Vec<f32>> = Vec::new();
        let mut audio: Vec<(u32, AudioSource)> = Vec::new();
        for (i, &index) in audio_in.iter().enumerate() {
            let source = if i < in_channels {
                AudioSource::Scratch(i)
            } else {
                spare.push(vec![0.0; max_block + super::processor::PROCESS_OVERRUN_PAD]);
                AudioSource::Spare(spare.len() - 1)
            };
            audio.push((index, source));
        }
        for (i, &index) in audio_out.iter().enumerate() {
            let source = if i < out_channels {
                AudioSource::Scratch(in_channels + i)
            } else {
                spare.push(vec![0.0; max_block + super::processor::PROCESS_OVERRUN_PAD]);
                AudioSource::Spare(spare.len() - 1)
            };
            audio.push((index, source));
        }

        let atom_ports: Vec<&PortInfo> =
            self.info.ports.iter().filter(|p| p.kind == PortKind::Atom).collect();
        // Notes go to the first atom input that says it takes MIDI, or failing
        // that to the first atom input there is.
        let midi_in = atom_ports
            .iter()
            .position(|p| p.input && p.midi)
            .or_else(|| atom_ports.iter().position(|p| p.input));
        let atoms: Vec<AtomPort> = atom_ports
            .iter()
            .map(|p| AtomPort {
                index: p.index,
                input: p.input,
                buffer: vec![0u64; atom_capacity(p.min_size) / 8],
            })
            .collect();
        let cv: Vec<(u32, Vec<f32>)> = self
            .info
            .ports
            .iter()
            .filter(|p| p.kind == PortKind::Cv || p.kind == PortKind::Other)
            .map(|p| (p.index, vec![0.0; max_block + super::processor::PROCESS_OVERRUN_PAD]))
            .collect();
        let mut rt = Box::new(Lv2Realtime {
            handle: live.handle,
            connect,
            run,
            audio,
            spare,
            atoms,
            midi_in,
            cv,
            worker: &*live.host.worker as *const Worker,
            worker_iface: live.worker,
            run_lock: self.run_lock.clone(),
            ui_events: self.ui_events.clone(),
            urid_sequence: urid(ATOM_SEQUENCE),
            urid_chunk: urid(ATOM_CHUNK),
            urid_frame_time: urid(ATOM_FRAME_TIME),
            urid_midi: urid(MIDI_EVENT),
        });
        // Atom and CV buffers never move from here on; connect them once.
        unsafe {
            for port in &mut rt.atoms {
                connect(live.handle, port.index, port.buffer.as_mut_ptr() as *mut c_void);
            }
            for (index, buffer) in &mut rt.cv {
                connect(live.handle, *index, buffer.as_mut_ptr() as *mut c_void);
            }
        }
        Ok(rt)
    }
}

impl Drop for Lv2Instance {
    fn drop(&mut self) {
        if let Some(live) = self.live.lock().unwrap().take() {
            unsafe {
                if let Some(deactivate) = self.desc().deactivate {
                    deactivate(live.handle);
                }
                if let Some(cleanup) = self.desc().cleanup {
                    cleanup(live.handle);
                }
            }
            drop(live);
        }
    }
}

/// Bytes an atom port's buffer holds: what the plugin asked for, and never
/// less than a block's worth of notes.
fn atom_capacity(min_size: usize) -> usize {
    min_size.max(8192).div_ceil(8) * 8
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const STATE_MAGIC: &[u8; 8] = b"GLV2ST1\0";

/// A saved LV2 state: the input control values, by port symbol, and whatever
/// properties the plugin's state interface stored — each with the URIs of its
/// key and type rather than their URIDs, which mean nothing in another process.
#[derive(Default)]
struct StateWriter {
    controls: Vec<(String, f32)>,
    properties: Vec<(String, String, u32, Vec<u8>)>,
}

impl StateWriter {
    fn encode(&self) -> Vec<u8> {
        let mut out = STATE_MAGIC.to_vec();
        let put_str = |out: &mut Vec<u8>, s: &str| {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        out.extend_from_slice(&(self.controls.len() as u32).to_le_bytes());
        for (symbol, value) in &self.controls {
            put_str(&mut out, symbol);
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&(self.properties.len() as u32).to_le_bytes());
        for (key, type_, flags, value) in &self.properties {
            put_str(&mut out, key);
            put_str(&mut out, type_);
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(value);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut rest = bytes
            .strip_prefix(&STATE_MAGIC[..])
            .context("not an LV2 plugin's saved state")?;
        fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
            anyhow::ensure!(rest.len() >= n, "truncated LV2 state");
            let (head, tail) = rest.split_at(n);
            *rest = tail;
            Ok(head)
        }
        fn u32_(rest: &mut &[u8]) -> Result<u32> {
            Ok(u32::from_le_bytes(take(rest, 4)?.try_into().unwrap()))
        }
        fn str_(rest: &mut &[u8]) -> Result<String> {
            let n = u32_(rest)? as usize;
            Ok(String::from_utf8_lossy(take(rest, n)?).into_owned())
        }
        let mut state = StateWriter::default();
        for _ in 0..u32_(&mut rest)? {
            let symbol = str_(&mut rest)?;
            let value = f32::from_le_bytes(take(&mut rest, 4)?.try_into().unwrap());
            state.controls.push((symbol, value));
        }
        for _ in 0..u32_(&mut rest)? {
            let key = str_(&mut rest)?;
            let type_ = str_(&mut rest)?;
            let flags = u32_(&mut rest)?;
            let n = u32_(&mut rest)? as usize;
            state.properties.push((key, type_, flags, take(&mut rest, n)?.to_vec()));
        }
        Ok(state)
    }
}

unsafe extern "C" fn state_store(
    handle: *mut c_void,
    key: u32,
    value: *const c_void,
    size: usize,
    type_: u32,
    flags: u32,
) -> u32 {
    let Some(writer) = (handle as *mut StateWriter).as_mut() else { return 1 };
    let (Some(key), Some(type_)) = (uri_of(key), uri_of(type_)) else { return 1 };
    let value = if value.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(value as *const u8, size).to_vec()
    };
    writer.properties.push((key, type_, flags, value));
    0
}

struct StateReader {
    properties: Vec<(u32, u32, u32, Vec<u8>)>,
}

unsafe extern "C" fn state_retrieve(
    handle: *mut c_void,
    key: u32,
    size: *mut usize,
    type_: *mut u32,
    flags: *mut u32,
) -> *const c_void {
    let Some(reader) = (handle as *const StateReader).as_ref() else { return std::ptr::null() };
    match reader.properties.iter().find(|p| p.0 == key) {
        Some((_, t, f, value)) => {
            if !size.is_null() {
                *size = value.len();
            }
            if !type_.is_null() {
                *type_ = *t;
            }
            if !flags.is_null() {
                *flags = *f;
            }
            value.as_ptr() as *const c_void
        }
        None => std::ptr::null(),
    }
}

// ---------------------------------------------------------------------------
// Processing
// ---------------------------------------------------------------------------

enum AudioSource {
    Scratch(usize),
    Spare(usize),
}

struct AtomPort {
    index: u32,
    input: bool,
    /// `u64`s, because an atom sequence must be 8-byte aligned.
    buffer: Vec<u64>,
}

struct Lv2Realtime {
    handle: *mut c_void,
    connect: unsafe extern "C" fn(*mut c_void, u32, *mut c_void),
    run: unsafe extern "C" fn(*mut c_void, u32),
    audio: Vec<(u32, AudioSource)>,
    spare: Vec<Vec<f32>>,
    atoms: Vec<AtomPort>,
    /// Which of `atoms` notes go to.
    midi_in: Option<usize>,
    cv: Vec<(u32, Vec<f32>)>,
    worker: *const Worker,
    worker_iface: *const WorkerInterface,
    run_lock: Arc<Mutex<()>>,
    ui_events: Arc<Mutex<Vec<(u32, u32, Vec<u8>)>>>,
    urid_sequence: u32,
    urid_chunk: u32,
    urid_frame_time: u32,
    urid_midi: u32,
}

unsafe impl Send for Lv2Realtime {}

/// An atom sequence being written into a port's buffer.
struct SequenceWriter<'a> {
    bytes: &'a mut [u8],
    /// Bytes used, counting from the start of the buffer.
    len: usize,
}

impl SequenceWriter<'_> {
    /// `{size, type}` header, then `{unit, pad}` body.
    fn begin(bytes: &mut [u8], sequence: u32, frame_time: u32) -> SequenceWriter<'_> {
        bytes[0..4].copy_from_slice(&8u32.to_ne_bytes());
        bytes[4..8].copy_from_slice(&sequence.to_ne_bytes());
        bytes[8..12].copy_from_slice(&frame_time.to_ne_bytes());
        bytes[12..16].copy_from_slice(&0u32.to_ne_bytes());
        SequenceWriter { bytes, len: 16 }
    }

    /// One event: frame time, atom header, body, padded to 8 bytes.
    fn push(&mut self, frames: i64, type_: u32, body: &[u8]) -> bool {
        let size = (16 + body.len()).div_ceil(8) * 8;
        if self.len + size > self.bytes.len() {
            return false;
        }
        let at = self.len;
        self.bytes[at..at + 8].copy_from_slice(&frames.to_ne_bytes());
        self.bytes[at + 8..at + 12].copy_from_slice(&(body.len() as u32).to_ne_bytes());
        self.bytes[at + 12..at + 16].copy_from_slice(&type_.to_ne_bytes());
        self.bytes[at + 16..at + 16 + body.len()].copy_from_slice(body);
        self.bytes[at + 16 + body.len()..at + size].fill(0);
        self.len += size;
        let atom_size = (self.len - 8) as u32;
        self.bytes[0..4].copy_from_slice(&atom_size.to_ne_bytes());
        true
    }
}

impl RealtimeProcess for Lv2Realtime {
    fn process(&mut self, scratch: &mut AudioScratch, frames: usize, events: &[MidiEvent]) {
        // A state is being restored: this block is silence rather than a race.
        let Ok(_guard) = self.run_lock.try_lock() else { return };
        let ptrs = scratch.ptrs_mut();
        unsafe {
            for (index, source) in &self.audio {
                let ptr = match source {
                    AudioSource::Scratch(ch) => ptrs[*ch],
                    AudioSource::Spare(i) => {
                        self.spare[*i].fill(0.0);
                        self.spare[*i].as_mut_ptr()
                    }
                };
                (self.connect)(self.handle, *index, ptr as *mut c_void);
            }
        }

        let ui_events = self.ui_events.try_lock().ok().map(|mut q| std::mem::take(&mut *q));
        for (i, port) in self.atoms.iter_mut().enumerate() {
            let cap = port.buffer.len() * 8;
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(port.buffer.as_mut_ptr() as *mut u8, cap)
            };
            if port.input {
                let mut seq = SequenceWriter::begin(bytes, self.urid_sequence, self.urid_frame_time);
                if self.midi_in == Some(i) {
                    for ev in events {
                        seq.push(ev.offset as i64, self.urid_midi, &ev.data);
                    }
                }
                if let Some(queued) = &ui_events {
                    for (port_index, type_, body) in queued {
                        if *port_index == port.index {
                            seq.push(0, *type_, body);
                        }
                    }
                }
            } else {
                // An output sequence is handed over as an empty chunk the size
                // of the buffer, for the plugin to fill.
                bytes[0..4].copy_from_slice(&((cap - 8) as u32).to_ne_bytes());
                bytes[4..8].copy_from_slice(&self.urid_chunk.to_ne_bytes());
            }
        }

        let worker = unsafe { &*self.worker };
        worker.in_run.store(true, Ordering::Release);
        unsafe { (self.run)(self.handle, frames as u32) };
        worker.in_run.store(false, Ordering::Release);

        // The worker: jobs asked for during the block, then their answers, then
        // the end of the cycle.
        if let Some(iface) = unsafe { self.worker_iface.as_ref() } {
            let requests = std::mem::take(&mut *worker.requests.lock().unwrap());
            if let Some(work) = iface.work {
                for request in &requests {
                    unsafe {
                        work(
                            self.handle,
                            worker_respond,
                            self.worker as *mut c_void,
                            request.len() as u32,
                            request.as_ptr() as *const c_void,
                        );
                    }
                }
            }
            let responses = std::mem::take(&mut *worker.responses.lock().unwrap());
            if let Some(work_response) = iface.work_response {
                for response in &responses {
                    unsafe {
                        work_response(self.handle, response.len() as u32, response.as_ptr() as *const c_void);
                    }
                }
            }
            if let Some(end_run) = iface.end_run {
                unsafe { end_run(self.handle) };
            }
        }
    }
}
