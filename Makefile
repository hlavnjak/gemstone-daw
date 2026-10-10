FOURIER_DIR := ../lesynth-fourier
INTERNAL_PLUGINS := internal_plugins

# ── Linux (native) ──────────────────────────────────────────────────────────
LINUX_TARGET := x86_64-unknown-linux-gnu
FOURIER_SO := $(FOURIER_DIR)/target/$(LINUX_TARGET)/release/liblesynth_fourier.so

# ── Windows (cross via mingw) ───────────────────────────────────────────────
WIN_TARGET := x86_64-pc-windows-gnu
FOURIER_DLL := $(FOURIER_DIR)/target/$(WIN_TARGET)/release/lesynth_fourier.dll

# ── macOS (cross via zig) ───────────────────────────────────────────────────
# Both architectures are built and fused with lipo into one universal binary,
# so the same app runs on Intel Macs and on Apple silicon.
MAC_X86 := x86_64-apple-darwin
MAC_ARM := aarch64-apple-darwin
# Oldest macOS each slice runs on. arm64 Macs shipped with 11.0.
MAC_X86_MIN := 10.13
MAC_ARM_MIN := 11.0
# The SDK supplies the framework headers and link stubs (CoreAudio, CoreMIDI,
# AppKit…) zig does not ship. Override with `make build-macos MACOS_SDK=...`.
MACOS_SDK ?= $(HOME)/.local/opt/MacOSX26.1.sdk
MAC_CC := $(CURDIR)/tools/zig-macos-cc.sh
LIPO ?= llvm-lipo
MAC_OUT := target/universal-apple-darwin/release
MAC_APP := $(MAC_OUT)/Gemstone DAW.app
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
FOURIER_DYLIB_X86 := $(FOURIER_DIR)/target/$(MAC_X86)/release/liblesynth_fourier.dylib
FOURIER_DYLIB_ARM := $(FOURIER_DIR)/target/$(MAC_ARM)/release/liblesynth_fourier.dylib

# `cargo zigbuild` for one Mac slice: $(1) = Rust target, $(2) = zig target
# (which carries the minimum version — see tools/zig-macos-cc.sh), $(3) = that
# minimum again, for rustc.
mac_cargo = SDKROOT=$(MACOS_SDK) MACOSX_DEPLOYMENT_TARGET=$(3) ZIG_MACOS_TARGET=$(2) \
	CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=$(MAC_CC) \
	CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=$(MAC_CC) \
	cargo zigbuild --release --target $(1)
mac_cargo_x86 = $(call mac_cargo,$(MAC_X86),x86_64-macos.$(MAC_X86_MIN)-none,$(MAC_X86_MIN))
mac_cargo_arm = $(call mac_cargo,$(MAC_ARM),aarch64-macos.$(MAC_ARM_MIN)-none,$(MAC_ARM_MIN))

# Source files of the plugin. Listing them as prerequisites of the build
# artifacts is essential: without them make sees the existing .so/.dll and
# considers the rule satisfied, so it never rebuilds after the plugin sources
# change — silently shipping a stale plugin into internal_plugins/.
FOURIER_SRCS := $(shell find $(FOURIER_DIR)/src -name '*.rs' 2>/dev/null) \
	$(FOURIER_DIR)/Cargo.toml

# Is the lesynth-fourier source tree available next to this repo? If so
# (developer setup), `make` rebuilds the embedded VST3 plugin from source and
# refreshes internal_plugins/. If not (a plain clone of the public repo), the
# precompiled binary committed under internal_plugins/ is used as-is.
HAVE_FOURIER_SRC := $(wildcard $(FOURIER_DIR)/Cargo.toml)

.PHONY: run build build-windows build-macos fourier fourier-windows fourier-macos \
	copy-internal copy-internal-windows copy-internal-macos macos-toolchain sign-macos clean clean-all dump scan

# ── Buzz debugging ──────────────────────────────────────────────────────────
# Source to render offline, and where the dumps land.
DUMP_SRC ?= D5.wav
DUMP_OUT ?= target/dump

# ── Linux build ─────────────────────────────────────────────────────────────

# Build (and embed the VST3 plugin), then run.
run: copy-internal
	cargo run --release

# Build, embedding the VST3 plugin.
build: copy-internal
	cargo build --release

ifeq ($(HAVE_FOURIER_SRC),)

# ── Public mode: no plugin source; use the committed binaries as-is ──────────
copy-internal:
	@test -f $(INTERNAL_PLUGINS)/liblesynth_fourier.so || { \
		echo "error: $(INTERNAL_PLUGINS)/liblesynth_fourier.so is missing."; \
		echo "       It is committed to the repo — re-clone, or place the"; \
		echo "       lesynth-fourier source at $(FOURIER_DIR) and run 'make fourier'."; \
		exit 1; }

copy-internal-windows:
	@test -f $(INTERNAL_PLUGINS)/lesynth_fourier.dll || { \
		echo "error: $(INTERNAL_PLUGINS)/lesynth_fourier.dll is missing."; exit 1; }

copy-internal-macos:
	@test -f $(INTERNAL_PLUGINS)/liblesynth_fourier.dylib || { \
		echo "error: $(INTERNAL_PLUGINS)/liblesynth_fourier.dylib is missing."; exit 1; }

fourier fourier-windows fourier-macos:
	@echo "lesynth-fourier source not found at $(FOURIER_DIR); using committed binary."

else

# ── Developer mode: rebuild the plugin from ../lesynth-fourier ───────────────

# Copy the freshly built lesynth-fourier .so into internal_plugins/.
copy-internal: $(INTERNAL_PLUGINS)/liblesynth_fourier.so

$(INTERNAL_PLUGINS)/liblesynth_fourier.so: $(FOURIER_SO)
	@mkdir -p $(INTERNAL_PLUGINS)
	cp $< $@

# Build the lesynth-fourier VST3 plugin (release, Linux).
fourier $(FOURIER_SO): $(FOURIER_SRCS)
	cd $(FOURIER_DIR) && cargo build --release --target $(LINUX_TARGET)

# Copy the freshly built lesynth-fourier .dll into internal_plugins/.
copy-internal-windows: $(INTERNAL_PLUGINS)/lesynth_fourier.dll

$(INTERNAL_PLUGINS)/lesynth_fourier.dll: $(FOURIER_DLL)
	@mkdir -p $(INTERNAL_PLUGINS)
	cp $< $@

# Build the lesynth-fourier VST3 plugin (release, Windows).
fourier-windows $(FOURIER_DLL): $(FOURIER_SRCS)
	cd $(FOURIER_DIR) && cargo build --release --target $(WIN_TARGET)

# Fuse the two freshly built Mac slices of the plugin into internal_plugins/.
copy-internal-macos fourier-macos: $(INTERNAL_PLUGINS)/liblesynth_fourier.dylib

$(INTERNAL_PLUGINS)/liblesynth_fourier.dylib: $(FOURIER_DYLIB_X86) $(FOURIER_DYLIB_ARM)
	@mkdir -p $(INTERNAL_PLUGINS)
	$(LIPO) -create -output $@ $^

# Build the lesynth-fourier VST3 plugin (release, macOS, one slice each).
$(FOURIER_DYLIB_X86): $(FOURIER_SRCS) | macos-toolchain
	cd $(FOURIER_DIR) && $(mac_cargo_x86)

$(FOURIER_DYLIB_ARM): $(FOURIER_SRCS) | macos-toolchain
	cd $(FOURIER_DIR) && $(mac_cargo_arm)

endif

# ── Windows cross build ─────────────────────────────────────────────────────

# Build for Windows, embedding the VST3 plugin.
build-windows: copy-internal-windows
	cargo build --release --target $(WIN_TARGET)

# ── macOS cross build ───────────────────────────────────────────────────────

# Fail up front, with the fix, rather than deep inside a linker error.
macos-toolchain:
	@command -v zig >/dev/null || { echo "error: zig not found (https://ziglang.org/download/)"; exit 1; }
	@command -v cargo-zigbuild >/dev/null || { echo "error: run 'cargo install cargo-zigbuild'"; exit 1; }
	@command -v $(LIPO) >/dev/null || { echo "error: $(LIPO) not found (part of LLVM); set LIPO=..."; exit 1; }
	@test -d "$(MACOS_SDK)/System/Library/Frameworks" || { \
		echo "error: no macOS SDK at $(MACOS_SDK); unpack one there or pass MACOS_SDK=..."; exit 1; }
	@rustup target list --installed | grep -qx $(MAC_X86) && \
		rustup target list --installed | grep -qx $(MAC_ARM) || { \
		echo "error: run 'rustup target add $(MAC_X86) $(MAC_ARM)'"; exit 1; }

# Build a universal (Intel + Apple silicon) binary and an app bundle around it,
# with the universal plugin in Contents/PlugIns/ where the app looks for it.
build-macos: copy-internal-macos | macos-toolchain
	$(mac_cargo_x86)
	$(mac_cargo_arm)
	@mkdir -p $(MAC_OUT)
	$(LIPO) -create -output $(MAC_OUT)/gemstone-daw \
		target/$(MAC_X86)/release/gemstone-daw target/$(MAC_ARM)/release/gemstone-daw
	rm -rf "$(MAC_APP)"
	mkdir -p "$(MAC_APP)/Contents/MacOS" "$(MAC_APP)/Contents/PlugIns"
	cp $(MAC_OUT)/gemstone-daw "$(MAC_APP)/Contents/MacOS/"
	cp $(INTERNAL_PLUGINS)/liblesynth_fourier.dylib "$(MAC_APP)/Contents/PlugIns/"
	sed -e 's/@VERSION@/$(VERSION)/' -e 's/@MIN_MACOS@/$(MAC_X86_MIN)/' \
		packaging/macos/Info.plist > "$(MAC_APP)/Contents/Info.plist"
	printf 'APPL????' > "$(MAC_APP)/Contents/PkgInfo"
	@$(LIPO) -info $(MAC_OUT)/gemstone-daw
	@echo "App bundle: $(MAC_APP)"

# Sign with the Developer ID certificate, notarise and staple — on a real Mac
# over ssh, since codesign and notarytool exist only on macOS. Which Mac, which
# identity and which notary key come from $(MAC_SIGN_ENV), which is gitignored;
# see tools/macos-sign.sh.
MAC_SIGN_ENV ?= packaging/macos/sign.env
sign-macos: build-macos
	MAC_SIGN_ENV=$(MAC_SIGN_ENV) VERSION=$(VERSION) tools/macos-sign.sh "$(MAC_APP)" $(MAC_OUT)

# ── Buzz debugging ──────────────────────────────────────────────────────────

# Render a source offline through a real plugin instance — no cpal, no engine
# ring buffer, no system mixer — so a defect found here is the plugin's.
#   make dump DUMP_SRC=my_voice.m4a DUMP_OUT=target/dump_voice
dump: copy-internal
	cargo build --release --bin dump_render
	./target/release/dump_render $(DUMP_SRC) --out $(DUMP_OUT)

# Scan what `dump` wrote.
#
#  1. exact.wav is subtracted from source.wav — they are meant to be sample-
#     identical, so anything left is the transform's own error.
#  2. the key render is compared against exact.wav with --vs, which fits out the
#     constant lag and level a render always has. At the source's own pitch the
#     two are supposed to be the same signal, so this residual is the keyboard's
#     added error with the voice's glottal pulse cancelled — the one number that
#     answers "does a key buzz".
#  3. the key render is compared against the host bridge's contour render, which
#     is what the keyboard used to play, so a change here is a change a listener
#     would hear.
#  4. each render on its own, for the event/recurrence view.
scan: dump
	@echo "── exact.wav residual vs source.wav ────────────────────────────────"
	python3 tools/buzzscan.py $(DUMP_OUT)/exact.wav --ref $(DUMP_OUT)/source.wav
	@echo
	@echo "── key render vs the exact inverse (the keyboard's added error) ────"
	@for f in $(DUMP_OUT)/key_*.wav; do \
		python3 tools/buzzscan.py $$f --vs $(DUMP_OUT)/exact.wav; done
	@echo
	@echo "── key render vs the host-bridge contour path ──────────────────────"
	@for f in $(DUMP_OUT)/key_*.wav; do \
		c=$$(echo $$f | sed 's|/key_|/contour_|'); \
		test -f $$c && python3 tools/buzzscan.py $$f --vs $$c | head -12; done
	@echo
	@echo "── each render on its own ──────────────────────────────────────────"
	@for f in $(DUMP_OUT)/key_*.wav $(DUMP_OUT)/contour_*.wav; do python3 tools/buzzscan.py $$f; done

clean:
	cargo clean
	@echo "Note: committed plugin binaries in $(INTERNAL_PLUGINS)/ are kept."

# Like `clean`, but also cleans the lesynth-fourier source tree if it is checked
# out next to this repo. No-op for the sibling on a plain public clone.
clean-all: clean
	@if [ -d $(FOURIER_DIR) ]; then \
		echo "Cleaning $(FOURIER_DIR)"; \
		cargo clean --manifest-path $(FOURIER_DIR)/Cargo.toml; \
	else \
		echo "$(FOURIER_DIR) not found; nothing else to clean."; \
	fi
