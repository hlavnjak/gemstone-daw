# Gemstone DAW

> **Pre-release public draft.** Gemstone DAW is under active development and the
> interface and formats may change. It is published early to share the core idea
> — additive **resynthesis** of arbitrary audio.

Gemstone DAW is a small DAW written in **Rust** with an `egui`/`eframe` GUI. It
hosts **VST3** plugins and ships with an embedded **LeSynth Fourier** additive
synthesizer, which powers its headline feature: turning any audio file into a
playable, editable additive-synthesis instrument.

## Headline feature — Resynthesis

The **Resynthesis** panel (`.wav` / `.mp3` / `.m4a` → LeSynth Fourier) takes an
arbitrary recording and rebuilds it as additive synthesis you can play and edit:

1. **Pick a file** — any `.wav`, `.mp3`, or `.m4a`.
2. **Segment** — the audio is split host-side into pitch-stable *subtracks*.
3. **Analyse** — each usable subtrack is handed to LeSynth Fourier, which
   subdivides it into per-period *buckets* and extracts an amplitude/phase value
   for every harmonic in every bucket (the FFT step). Pitch contours (vibrato)
   are tracked, so the analysis follows the note rather than smearing it.
4. **Play & edit** — the analysed grid is previewed inline ("Preview FFT") and
   can be opened in a full LeSynth Fourier editor instance ("Open in LeSynth")
   running in Analysis mode, where individual harmonics can be toggled and the
   result played back on the keyboard.

A file can also skip all of it. **"Add whole file as Track"** publishes the
recording *itself* — not an analysis of it — as a **wav track**: a Track a
Composer row plays for the length of one note. There is no pitch to pick, because
a recording has the pitch it was made at; what a note frame carries in the pitch
box's place is a **start** — a slider saying where in the file the note begins,
which is how a take gets lined up with the beat. It is what a take no
additive analysis does justice to is for — a drum loop, a spoken line, a whole
performance — and a project saves it as the path to the file it was added from,
so the file stays where it is rather than being copied into the project folder.

## Track Composer

Arrange the tracks in rows of frames and play them together. Each row plays
exactly one Track — a LeSynth Fourier track, a custom VST3 track, a subtrack
published from the Resynthesis panel with "Add as Track", or a whole audio file
published there as a wav track — and any number of rows may share the same one.

- **Rows** are added by hand ("➕ Add Track Row"); a fresh instance starts empty.
  The select box at the head of a row picks its Track, and follows the track
  list: delete the track a row is playing and the row moves to another one, or
  to a placeholder while no tracks exist.
- **A row is a sequence of frames**, laid left to right and simply played one
  after another: nothing is positioned by hand, nothing is dragged, and nothing
  can overlap. Deleting a frame pulls everything behind it forward.
- **Drum tracks name their notes.** On a row playing a plugin recognised as a
  drum kit, the pitch box reads `C2 · Bass Drum (Kick)` rather than `C2`, and a
  new note starts on the kick instead of middle C. The names are General MIDI's,
  which is the map nearly every kit follows; they are added to the pitch, never
  swapped for it, so a kit laid out some other way still shows what is sent. The
  map and the rule that spots a drum plugin are in `src/midi/drums.rs`.
- **Notes and spaces.** "➕ Add Note" appends two frames: a **note** frame
  (blue), and behind it a **space** frame (amber) which is the silence that
  follows. A note frame carries four select boxes — pitch (`C0`–`B8`), the
  whole-note part of its length, the nominator (`× 1` … `× 16`), and the
  fraction it counts (1/2 … 1/256). A space frame carries the same three length
  boxes; it has no pitch — and neither does a note on a **wav row**, which plays
  its file for as long as its length says. That frame is wider than the rest, and
  carries a **start** slider where a pitch would be: a take does not begin on its
  first sample, so the start is what puts the sound on the beat instead of the
  room tone in front of it. Drag the slider to find it, drag the number beside it
  for milliseconds, or double-click to type one. The **⏱** button beside the
  frame's ✖ sets the note's length to what the file has left to play from that
  start, as closely as the boxes can name it *at the current tempo* — its hover
  text says how close, since a recording's length and a whole number of note
  values agree only by luck. Change the BPM and press it again. A length is the
  parts added together, so `1` + `× 1` +
  `1/8` is a whole note tied to an eighth, and `× 3` over `1/8` is `3/8` — the
  dotted quarter the halving fraction box cannot name on its own. The nominator
  needs a fraction to count, so it is greyed out while the fraction reads `—`.
- **A space belongs to its note.** It has no delete button of its own: the ✖ on
  a note frame removes the note *and* its space, and there is no way to remove
  one without the other. Setting a space to `0 whole` and `—` is the way to run
  two notes together — the frame stays as a placeholder, but adds no time.
- **Blocks — copying a section across every track at once.** Music repeats, and
  the second time round should not cost what the first did. Each note frame
  carries a select box (`☐`): click it to start a **block**, click another frame
  on the row to take everything between them, and click one that is already in
  the block to drop it. Shift-click moves the end last clicked, which is how a
  block shrinks.
  Rows are selected independently, so a block spans as many of them as you like —
  or mark the phrase on the row you can hear it in and press **↔ Span Rows**,
  which takes *the same stretch of time* out of every other row. That is what
  makes a natural block one press: a section is a stretch of time, not a count of
  frames, and no two rows spend it on the same number of notes.
  - **🔁 Clone ×N** repeats the block in place — each copy directly behind the
    last, on every row at once, with everything that followed moved along by the
    same amount on every row. A copy occupies the block's **window** (from its
    first note to the end of its last space), not the frames' own extent, so the
    silence a row leaves at either end of the window is put back between one copy
    and the next and the tracks stay in step however many times it is pressed.
    The selection moves onto the copy, so pressing it again repeats the repeat.
  - **🗐 Copy** and **📋 Paste at End** carry a block, with the silence in front
    of each row's part of it, to the end of the composition — every row padded up
    to the end of the longest one, so a block that sounded together is pasted
    sounding together. It goes back on the rows it came from: the frames name a
    pitch, a length and a place in a file that mean what they mean on the track
    they were written for.
  - A gap between copies is arithmetic, not a length anyone picked, so it is not
    always one the three length boxes can name — `255/256` needs 255 of the
    smallest fraction there is. What will not fit in one frame is laid out over
    frames of no length: silence, which is what a gap is, and which the transport
    skips. A rounded gap would be a copy that drifts further out of step with
    every repeat.
- **Time**: every row starts at zero, so two rows sound together exactly when the
  lengths in front of their frames add up the same — that is what makes a chord.
  The tempo control (BPM) sets what a beat is worth — a beat is a quarter note.
- **Repeat** loops the arrangement for as long as it is ticked, on the written
  length — the longest row, trailing silence included — rather than on the last
  note, so the bar structure survives the wrap and releases ring on across it.
  It can be switched on and off while the transport runs; unticking it makes the
  pass in flight the last one.
- **A loop follows your edits.** While Repeat is on, changing notes — pitch,
  length, adding or deleting them — or a row's gain or the tempo is picked up at
  the next time round, without stopping. Adding a row or pointing one at another
  track is the exception: that needs a plugin loaded, which cannot happen while
  the audio callback is running, so the panel says to press Play again.
- **Play / Stop** at the foot of the section renders the whole arrangement in
  real time, lighting up the frame each row is sounding as it goes: one output
  stream, one plugin instance per row, mixed with each row's own gain. The
  Composer loads its own instances, so a composition plays whether or not the
  tracks' editors are open — and when one is open, its live grid is what plays.
- **Export WAV** writes the whole composition to a 16-bit `.wav` at the output
  device's rate and channel count. It renders offline, through the same voices,
  schedule and mix the transport uses, so the file is what Play sounds like; the
  render runs in the background and the panel stays usable while it works.

## Other features

- **Load Internal plugin** — loads the embedded LeSynth Fourier VST3
  (`internal_plugins/liblesynth_fourier.so`, committed precompiled) by its class
  ID. No separate plugin install required.
- **A wav track is a path.** A row playing one needs no plugin at all: the
  Composer decodes the file once, however many rows name it, and plays it under
  every note from that note's own start, cut to the note's length and faded a few
  milliseconds at each end so a cut cannot click. A start past the end of the
  file is simply silent. It is mixed through the same
  voices and the same per-row gain as everything else, so "Export WAV" writes
  exactly what the transport plays.
- **Custom VST3 sounds are kept.** What a plugin is playing — the knobs set in
  its own editor — is taken from the instance when its editor closes, handed to
  the Composer's instances, and written into the project folder as a
  `.vststate` beside the manifest. It travels over `IComponent::getState`, the
  mechanism every VST3 has, rather than LeSynth's grid ABI, which only ours does.
- **External VST3 plugins** — "Create Custom VST Track" lists the plugins
  installed in the standard locations (`VST3_PATH`, `~/.vst3`, `/usr/lib/vst3`,
  `/usr/local/lib/vst3`) and can browse for a `.vst3` bundle or a plugin library
  anywhere else. A bundle is resolved to the library inside it
  (`Foo.vst3/Contents/x86_64-linux/Foo.so`), the module's `ModuleEntry` is run,
  its component and edit controller are initialised against a host context and
  connected, and the plugin's own GUI is shown in a native window (raw X11 on
  Linux, raw Win32 on Windows). The X11 window provides the `IPlugFrame` and
  `Linux::IRunLoop` a JUCE or Steinberg-SDK editor needs in order to draw at all.
  Anything that is not a loadable VST3 — a VST2 `.so`, a library with a missing
  dependency — is reported when the plugin is picked, not when its editor opens.
- **MIDI input** — pick a USB keyboard / port, connect, refresh.
- **Logging** to `gemstone-daw.log`.

## Project layout

```
Cargo.toml          # egui/eframe app crate
Makefile            # builds the app; (re)builds + embeds the VST3 when its source is present
.cargo/config.toml  # Windows cross linker
internal_plugins/   # the embedded LeSynth Fourier VST3 (committed precompiled)
src/
  main.rs                                 # eframe entry point
  lib.rs
  vst/{host,module,host_context,handler,event_list,mod}.rs  # VST3 hosting
  bin/vst3_probe.rs                       # `cargo run --bin vst3_probe -- <plugin>`:
                                          #   what the host sees in a plugin, and why
                                          #   one will not load
  audio/{engine,decode,mod}.rs            # cpal audio engine + audio file decoding
  midi/{input,mod}.rs                     # midir MIDI input
  analysis/{segmentation,mod}.rs          # host-side subtrack segmentation
  gui/
    app.rs            # top-level egui app
    registry.rs       # the shared list of Tracks every panel agrees on
    track.rs          # the Tracks panel
    composer/         # the Track Composer panel + its playback engine
    resynth.rs        # the Resynthesis panel
    editor_window/    # embedded plugin editor window (x11 / windows / fallback)
```

## Requirements

- A recent **Rust** toolchain.
- Linux: an X11 or Wayland display, plus GTK3 (used by the file-open dialog).

No copy of the Steinberg VST3 SDK is needed: the `vst3` crate ships pre-generated
bindings, so nothing is scraped from the SDK headers at build time.

## Build & run (Linux)

```sh
make run        # build and run the DAW
make build      # build only
make clean      # cargo clean (keeps the committed plugin binaries)
make clean-all  # also clean ../lesynth-fourier if it is checked out alongside
```

The binary lands at `target/release/gemstone-daw`. The app is GUI-only and needs
a display. The embedded plugin is loaded from the committed
`internal_plugins/liblesynth_fourier.so`, so you do **not** need the
lesynth-fourier source tree just to run Gemstone DAW.

### Rebuilding the embedded plugin (developers)

If you have the `lesynth-fourier` source checked out next to this repo
(`../lesynth-fourier`), `make` automatically rebuilds it and refreshes
`internal_plugins/` when its sources change. Otherwise the committed binary is
used as-is.

```sh
make fourier  # build only the LeSynth Fourier VST3 (requires ../lesynth-fourier)
```

## Windows cross build (from Linux via mingw)

Requires the mingw toolchain (`x86_64-w64-mingw32-gcc`/`g++`) and
`rustup target add x86_64-pc-windows-gnu`.

```sh
make build-windows    # build the app + embed the VST3 .dll
make fourier-windows  # build only the VST3 plugin for Windows
```

Output: `target/x86_64-pc-windows-gnu/release/gemstone-daw.exe`.

## License

Licensed under the **Apache License, Version 2.0**. See [`LICENSE`](LICENSE).

Copyright 2025 Jakub Hlavnicka.
