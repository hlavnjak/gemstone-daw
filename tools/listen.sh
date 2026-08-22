#!/bin/sh
# Regenerate the A/B listening set for one source, at several keys.
#
#   tools/listen.sh target/listen11 [my_voice.m4a]
#
# A_preserve_seconds_<hz>.wav — the key holds the source's own duration.
# B_synth_timeline_<hz>.wav   — one grain per bucket; the note's length follows
#                               the key.
# 0_original_pitch_reference.wav — the exact inverse, the ceiling to beat.
set -e
OUT=${1:-target/listen}
SRC=${2:-my_voice.m4a}
DUMP=./target/release/dump_render
cargo build --release --bin dump_render 2>&1 | tail -2
mkdir -p "$OUT"
tmp="$OUT/.tmp"
rm -rf "$tmp"

for hz in 65 110 262 523; do
    $DUMP "$SRC" --out "$tmp" --note "$hz" >"$OUT/log_A_$hz.txt"
    cp "$tmp/key_${hz}hz.wav" "$OUT/A_preserve_seconds_${hz}hz.wav"
    [ -f "$OUT/0_original_pitch_reference.wav" ] || \
        cp "$tmp/exact.wav" "$OUT/0_original_pitch_reference.wav"
    cp "$tmp/source.wav" "$OUT/0_source.wav"

    $DUMP "$SRC" --out "$tmp" --note "$hz" --synth-timeline >"$OUT/log_B_$hz.txt"
    cp "$tmp/key_${hz}hz.wav" "$OUT/B_synth_timeline_${hz}hz.wav"
done
rm -rf "$tmp"
ls -la "$OUT"
