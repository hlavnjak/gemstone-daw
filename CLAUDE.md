# CLAUDE.md — Gemstone DAW

Project guidance for working in this repository.

## Project

Gemstone DAW — a Rust DAW built with `egui`/`eframe`. It ports every feature of
`lesynth-daw` (except the piano keyboard) and hosts VST3 plugins, with an
embedded LeSynth Fourier VST3 ("Load Internal") as the key requirement.

- Language: **Rust**
- GUI: `egui` / `eframe` (ported from `iced`)
- Audio: `cpal` · MIDI: `midir` · VST3 hosting via the `vst3` crate
- Targets: Linux (X11/Wayland) and Windows (`x86_64-pc-windows-gnu` cross build)

See `README.md` for the full project layout and feature list.

## Build & Run

```sh
make build          # build fourier (release) + copy .so → internal_plugins/ + build gemstone-daw
make run            # build, then run the DAW
make fourier        # build only the LeSynth Fourier VST3 plugin
make clean          # clean build artifacts

cargo build         # plain debug build
cargo build --tests # build with tests
```

The Linux binary lands at `target/release/gemstone-daw`. The app is GUI-only and
needs a display.

### Windows cross build (from Linux via mingw)

Requires the mingw toolchain (`x86_64-w64-mingw32-gcc`/`g++`) and
`rustup target add x86_64-pc-windows-gnu`.

```sh
make build-windows    # build fourier .dll, copy to internal_plugins/, build gemstone-daw.exe
make fourier-windows  # build only the VST3 plugin for Windows
```

Output: `target/x86_64-pc-windows-gnu/release/gemstone-daw.exe`.

## Commits

- **Never** add a Claude signature, watermark, or "Generated with Claude Code" /
  "Co-Authored-By: Claude" trailer to commit messages or PR bodies.
- Keep commit messages concise and descriptive of the actual change.
- Only commit or push when explicitly asked.
