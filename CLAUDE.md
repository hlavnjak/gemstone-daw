# CLAUDE.md — Gemstone DAW

Project guidance for working in this repository.

## Project

Gemstone DAW — a Rust DAW built with `egui`/`eframe`. It ports every feature of
`lesynth-daw` (except the piano keyboard) and hosts VST3, CLAP, VST2 and LV2
plugins (plus Audio Units on macOS), with an embedded LeSynth Fourier VST3
("Load Internal") as the key requirement.

- Language: **Rust**
- GUI: `egui` / `eframe` (ported from `iced`)
- Audio: `cpal` · MIDI: `midir` · plugin hosting in `src/plugin/` (VST3 via the
  `vst3` crate in `src/vst/`, CLAP via `clap-sys`, VST2/LV2/AU with no crate)
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

### macOS cross build (from Linux via zig)

Requires `zig` + `cargo install cargo-zigbuild`, `rustup target add
x86_64-apple-darwin aarch64-apple-darwin`, `llvm-lipo`, and a macOS SDK at
`~/.local/opt/MacOSX26.1.sdk` (or `MACOS_SDK=...`).

```sh
make build-macos    # universal (x86_64 10.13+ / arm64 11+) binary + .app bundle
make fourier-macos  # build only the universal VST3 plugin .dylib
```

Output: `target/universal-apple-darwin/release/Gemstone DAW.app`. Linking goes
through `tools/zig-macos-cc.sh`, because plain `cargo zigbuild` stamps every
binary "minimum macOS 13" whatever `MACOSX_DEPLOYMENT_TARGET` says.

## Commits

- **Never** add a Claude signature, watermark, or "Generated with Claude Code" /
  "Co-Authored-By: Claude" trailer to commit messages or PR bodies.
- Keep commit messages concise and descriptive of the actual change.
- Only commit or push when explicitly asked.
