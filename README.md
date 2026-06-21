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
.cargo/config.toml  # Windows cross linker; VST3_SDK_DIR comes from the environment
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
    resynth.rs        # the Resynthesis panel
    editor_window/    # embedded plugin editor window (x11 / windows / fallback)
```

## Requirements

- A recent **Rust** toolchain.
- The **Steinberg VST3 SDK** — the `vst3` crate generates its bindings from the
  SDK headers at build time. Download it from Steinberg and point `VST3_SDK_DIR`
  at the checkout:

  ```sh
  export VST3_SDK_DIR=/path/to/vst3sdk
  ```

  (You can also pass it per-invocation: `make run VST3_SDK_DIR=/path/to/vst3sdk`.)
- Linux: an X11 or Wayland display, plus GTK3 (used by the file-open dialog).

## Build & run (Linux)

```sh
make run      # build and run the DAW
make build    # build only
make clean    # cargo clean (keeps the committed plugin binaries)
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
