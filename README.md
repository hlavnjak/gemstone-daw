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

## Track Composer

Arrange the tracks in rows of frames and play them together. Each row plays
exactly one Track — a LeSynth Fourier track, a custom VST3 track, or a subtrack
published from the Resynthesis panel with "Add as Track" — and any number of
rows may share the same one.

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
  follows. A note frame carries three select boxes — pitch (`C0`–`B8`), the
  whole-note part of its length, and the fractional part (1/2 … 1/256). A space
  frame carries only the two length boxes; it has no pitch. A length is the two
  parts added together, so `1` + `1/8` is a whole note tied to an eighth.
- **A space belongs to its note.** It has no delete button of its own: the ✖ on
  a note frame removes the note *and* its space, and there is no way to remove
  one without the other. Setting a space to `0 whole` + `—` is the way to run
  two notes together — the frame stays as a placeholder, but adds no time.
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
