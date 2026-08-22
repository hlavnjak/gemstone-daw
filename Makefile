FOURIER_DIR := ../lesynth-fourier
INTERNAL_PLUGINS := internal_plugins

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

.PHONY: run build build-windows fourier fourier-windows copy-internal copy-internal-windows clean clean-all dump scan

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
build-windows: copy-internal-windows
	cargo build --release --target $(WIN_TARGET)

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
