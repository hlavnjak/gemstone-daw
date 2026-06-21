FOURIER_DIR := ../lesynth-fourier
INTERNAL_PLUGINS := internal_plugins

# The `vst3` crate's build script reads the Steinberg VST3 SDK location from the
# VST3_SDK_DIR environment variable. Re-export it so it reaches cargo whether it
# comes from the shell environment or from `make VST3_SDK_DIR=... run`.
export VST3_SDK_DIR

# ── Linux (native) ──────────────────────────────────────────────────────────
LINUX_TARGET := x86_64-unknown-linux-gnu
FOURIER_SO := $(FOURIER_DIR)/target/$(LINUX_TARGET)/release/liblesynth_fourier.so

# ── Windows (cross via mingw) ───────────────────────────────────────────────
WIN_TARGET := x86_64-pc-windows-gnu
FOURIER_DLL := $(FOURIER_DIR)/target/$(WIN_TARGET)/release/lesynth_fourier.dll

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

# Mingw ships libstdc++ headers (cstdint, ...) that clang/com-scrape need when
# generating the VST3 bindings for the Windows target. Pull the exact include
# search paths out of the cross g++ itself (version-independent), colon-joined
# for CPLUS_INCLUDE_PATH.
WIN_CPLUS_INCLUDE_PATH := $(shell echo | x86_64-w64-mingw32-g++ -E -x c++ -v - 2>&1 | \
	awk '/#include <...> search starts/{f=1;next} /End of search/{f=0} f{gsub(/^ +/,"");print}' | \
	paste -sd:)

.PHONY: run build build-windows fourier fourier-windows copy-internal copy-internal-windows check-sdk clean

# Fail early with a helpful message if the VST3 SDK location isn't set.
check-sdk:
	@if [ -z "$$VST3_SDK_DIR" ]; then \
		echo "error: VST3_SDK_DIR is not set."; \
		echo "       Point it at your Steinberg VST3 SDK checkout, e.g."; \
		echo "         export VST3_SDK_DIR=/path/to/vst3sdk"; \
		echo "       See the README for details."; \
		exit 1; \
	fi

# ── Linux build ─────────────────────────────────────────────────────────────

# Build (and embed the VST3 plugin), then run.
run: check-sdk copy-internal
	cargo run --release

# Build, embedding the VST3 plugin.
build: check-sdk copy-internal
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

fourier fourier-windows:
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

endif

# ── Windows cross build ─────────────────────────────────────────────────────

# Build for Windows, embedding the VST3 plugin.
build-windows: check-sdk copy-internal-windows
	CPLUS_INCLUDE_PATH="$(WIN_CPLUS_INCLUDE_PATH)" cargo build --release --target $(WIN_TARGET)

clean:
	cargo clean
	@echo "Note: committed plugin binaries in $(INTERNAL_PLUGINS)/ are kept."
