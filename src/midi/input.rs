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
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use anyhow::Result;
use midir::{MidiInput, MidiInputConnection};

pub type MidiEventQueue = Arc<Mutex<VecDeque<[u8; 3]>>>;

/// One message as a listener sees it: when it arrived, and what it was. The
/// stamp is taken in the MIDI callback, which is the only place the keyboard's
/// timing survives — a GUI thread that polls at 60 Hz can only say "some time in
/// the last 16 ms", which is a third of a 1/16 note at 120 BPM.
pub type TimedMessage = (Instant, [u8; 3]);

/// A listener on the keyboard: every message copied here, stamped.
///
/// A tap **copies**; it does not consume. The queue an open editor plays from is
/// drained destructively by its audio callback, so a recorder that read from it
/// would take notes out of the instrument's mouth — the user would hear half of
/// what they played and record the other half.
pub type MidiTap = Arc<Mutex<Vec<TimedMessage>>>;

/// The taps currently listening, held weakly: a recorder that goes away
/// unregisters itself simply by being dropped, and nothing has to remember to
/// take it off the list.
pub type MidiTaps = Arc<Mutex<Vec<Weak<Mutex<Vec<TimedMessage>>>>>>;

/// Cap on messages held in one tap. A recording is a few hundred at most; this
/// only bounds a tap left listening and forgotten. Past it the tap stops
/// growing rather than dropping its oldest — a recording with a hole in the
/// middle is worse than one that ends early, and it can say where it stopped.
const MAX_TAPPED_EVENTS: usize = 65_536;

/// Cap on buffered MIDI messages. The queue is drained by an audio engine only
/// while a track/subtrack editor is open; without this bound, playing a
/// connected keyboard with no editor open would grow it without limit (and flood
/// on the next open). Dropping the oldest keeps at most a brief burst.
const MAX_QUEUED_EVENTS: usize = 1024;

/// Create a new empty MIDI event queue.
pub fn new_midi_queue() -> MidiEventQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// A fresh, empty tap list — nothing is listening yet.
pub fn new_midi_taps() -> MidiTaps {
    Arc::new(Mutex::new(Vec::new()))
}

/// Start listening. The returned tap fills with everything the keyboard sends
/// (alongside, not instead of, the main queue) until it is dropped.
pub fn add_midi_tap(taps: &MidiTaps) -> MidiTap {
    let tap: MidiTap = Arc::new(Mutex::new(Vec::new()));
    if let Ok(mut list) = taps.lock() {
        // Sweep out the taps whose owners have gone while we are here — this is
        // the only place the list is added to, so it is the natural place to
        // keep it from growing one dead entry per recording.
        list.retain(|t| t.strong_count() > 0);
        list.push(Arc::downgrade(&tap));
    }
    tap
}

/// List available MIDI input port names.
pub fn list_midi_ports() -> Result<Vec<String>> {
    let midi_in = MidiInput::new("gemstone-daw-query")?;
    let ports = midi_in.ports();
    let names: Vec<String> = ports
        .iter()
        .map(|p| midi_in.port_name(p).unwrap_or_else(|_| "Unknown".into()))
        .collect();
    Ok(names)
}

/// List MIDI input ports that look like USB MIDI keyboards.
/// Filters out virtual/software ports (e.g. "Midi Through") and keeps
/// ports whose names suggest a USB hardware device.
pub fn list_usb_midi_keyboards() -> Result<Vec<String>> {
    let all = list_midi_ports()?;
    let filtered = all
        .into_iter()
        .filter(|name| {
            let lower = name.to_lowercase();
            // Exclude virtual/through ports
            if lower.contains("through") || lower.contains("virtual") || lower.contains("rtpmidi") {
                return false;
            }
            // Keep everything else — on ALSA, remaining ports are typically
            // hardware devices (USB MIDI keyboards, controllers, etc.)
            true
        })
        .collect();
    Ok(filtered)
}

/// Spawn a MIDI input thread that pushes raw 3-byte messages into the queue.
/// Connects to the first port whose name contains `device_filter`, or port 0 if None.
pub fn spawn_midi_thread(
    midi_events: MidiEventQueue,
    taps: MidiTaps,
    device_filter: Option<&str>,
) -> Result<MidiInputConnection<()>> {
    let mut midi_in = MidiInput::new("gemstone-daw-midi-in")?;
    midi_in.ignore(midir::Ignore::None);

    let ports = midi_in.ports();
    if ports.is_empty() {
        anyhow::bail!("No MIDI input ports found");
    }

    log::info!("Available MIDI input ports:");
    for (i, port) in ports.iter().enumerate() {
        let name = midi_in
            .port_name(port)
            .unwrap_or_else(|_| "Unknown".to_string());
        log::info!("  [{}] {}", i, name);
    }

    let selected_port = if let Some(filter) = device_filter {
        ports
            .iter()
            .enumerate()
            .find(|(_, p)| {
                midi_in
                    .port_name(p)
                    .unwrap_or_default()
                    .contains(filter)
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    } else {
        0
    };

    let port = &ports[selected_port];
    log::info!(
        "Connecting to MIDI device: {}",
        midi_in.port_name(port)?
    );

    let conn = midi_in
        .connect(
            port,
            "gemstone-daw-midi-conn",
            move |_stamp, message, _| {
                if message.len() >= 3 {
                    let msg = [message[0], message[1], message[2]];
                    {
                        let mut queue = midi_events.lock().unwrap();
                        if queue.len() >= MAX_QUEUED_EVENTS {
                            queue.pop_front();
                        }
                        queue.push_back(msg);
                    }
                    // Listeners get their own stamped copy. `midir`'s own stamp
                    // is a device-relative microsecond count with no fixed
                    // origin; an `Instant` taken here can be compared with the
                    // one the transport started on, which is what a recording
                    // has to line up against.
                    let now = Instant::now();
                    if let Ok(list) = taps.lock() {
                        for tap in list.iter().filter_map(Weak::upgrade) {
                            if let Ok(mut events) = tap.lock() {
                                if events.len() < MAX_TAPPED_EVENTS {
                                    events.push((now, msg));
                                }
                            }
                        }
                    }
                }
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!("Failed to connect to MIDI input: {}", e))?;

    log::info!("MIDI input connected.");
    Ok(conn)
}