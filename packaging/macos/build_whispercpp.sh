#!/usr/bin/env bash
#
# Builds whisper.cpp for Apple Silicon with Metal, and stages it together with
# the one model that ships inside the app.
#
#     ./packaging/macos/build_whispercpp.sh <bin-dir> <models-dir>
#
# Unlike every other helper, this one we compile. whisper.cpp publishes no
# macOS command-line binary - its releases carry Windows builds, Linux builds
# and an Apple xcframework, none of which is a program the app can spawn - so
# there is no permanent release asset to pin the way ffmpeg and yt-dlp are.
# Building it here is the price of using it, and it makes us the publisher: if
# this binary is wrong, it is ours to fix.
#
# The model is pinned and hash-checked the same way the downloads in
# src/download.rs are. Only the smallest ships; the larger ones are fetched on
# demand at runtime.
#
# Needs cmake (`brew install cmake`). The GitHub macOS runners have it already.

set -euo pipefail

BIN_DIR="${1:?usage: build_whispercpp.sh <bin-dir> <models-dir>}"
MODELS_DIR="${2:?usage: build_whispercpp.sh <bin-dir> <models-dir>}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CACHE="$REPO_ROOT/packaging/.cache/whispercpp"

# Pin both, for the same reason every other component is pinned: a build that
# cannot be reproduced later is a build nobody can debug.
WHISPER_TAG="v1.9.3"
MODEL_FILE="ggml-tiny.bin"
MODEL_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$MODEL_FILE"
MODEL_SHA="be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21"

mkdir -p "$CACHE" "$BIN_DIR" "$MODELS_DIR"

if ! command -v cmake >/dev/null 2>&1; then
	echo "cmake not found - install it with: brew install cmake" >&2
	exit 1
fi

# --- the engine ---------------------------------------------------------------

SRC="$CACHE/whisper.cpp-$WHISPER_TAG"
BUILT="$CACHE/whisper-cli-$WHISPER_TAG"

if [[ ! -x "$BUILT" ]]; then
	echo "whisper.cpp $WHISPER_TAG"
	if [[ ! -d "$SRC" ]]; then
		echo "  clone $WHISPER_TAG"
		git clone --quiet --depth 1 --branch "$WHISPER_TAG" \
			https://github.com/ggml-org/whisper.cpp.git "$SRC"
	fi

	echo "  build (Metal)"
	cmake -S "$SRC" -B "$SRC/build" \
		-DCMAKE_BUILD_TYPE=Release \
		-DGGML_METAL=ON \
		-DWHISPER_BUILD_TESTS=OFF \
		-DWHISPER_BUILD_SERVER=OFF \
		>/dev/null
	cmake --build "$SRC/build" --config Release -j "$(sysctl -n hw.ncpu)" >/dev/null

	cp "$SRC/build/bin/whisper-cli" "$BUILT"
else
	echo "whisper.cpp $WHISPER_TAG (cached)"
fi

cp "$BUILT" "$BIN_DIR/whisper-cli"
chmod +x "$BIN_DIR/whisper-cli"

# Confirm it is the architecture we meant to build, not whatever the host
# happened to default to. A universal or x86-64 binary here would run - under
# Rosetta, on the CPU - and silently give up the entire reason for the switch.
ARCH="$(lipo -archs "$BIN_DIR/whisper-cli" 2>/dev/null || echo unknown)"
if [[ "$ARCH" != "arm64" ]]; then
	echo "  expected an arm64 build, got '$ARCH'" >&2
	exit 1
fi
echo "  staged whisper-cli ($ARCH)"

# --- the bundled model --------------------------------------------------------

CACHED_MODEL="$CACHE/$MODEL_FILE"
if [[ ! -f "$CACHED_MODEL" ]]; then
	echo "  download $MODEL_FILE"
	curl -fsSL -o "$CACHED_MODEL.part" "$MODEL_URL"
	mv "$CACHED_MODEL.part" "$CACHED_MODEL"
fi

GOT="$(shasum -a 256 "$CACHED_MODEL" | cut -d' ' -f1)"
if [[ "$GOT" != "$MODEL_SHA" ]]; then
	echo "  $MODEL_FILE hash mismatch" >&2
	echo "    expected $MODEL_SHA" >&2
	echo "    got      $GOT" >&2
	rm -f "$CACHED_MODEL"
	exit 1
fi

cp "$CACHED_MODEL" "$MODELS_DIR/$MODEL_FILE"
echo "  staged $MODEL_FILE ($(( $(stat -f%z "$MODELS_DIR/$MODEL_FILE") / 1048576 )) MB, verified)"
