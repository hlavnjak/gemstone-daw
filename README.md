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
- **Play / Stop** at the foot of the section renders the whole arrangement in
  real time, lighting up the frame each row is sounding as it goes: one output
  stream, one plugin instance per row, mixed with each row's own gain. The
  Composer loads its own instances, so a composition plays whether or not the
  tracks' editors are open — and when one is open, its live grid is what plays.

## Other features

- **Load Internal plugin** — loads the embedded LeSynth Fourier VST3
  (`internal_plugins/liblesynth_fourier.so`, committed precompiled) by its class
  ID. No separate plugin install required.
- **External VST3 plugins** — load from a path, unload, and show the plugin's
  own GUI embedded in a native window (raw X11 on Linux, raw Win32 on Windows).
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
  vst/{host,handler,event_list,mod}.rs    # VST3 hosting
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
