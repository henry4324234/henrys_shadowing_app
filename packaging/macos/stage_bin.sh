#!/usr/bin/env bash
#
# Stages the helper executables the .app bundles in Contents/Resources/bin,
# the macOS counterpart of packaging\stage_bin.ps1.
#
#     ./packaging/macos/stage_bin.sh <bin-dir>
#
# Like the Windows script it reads the pinned versions out of the MANIFEST in
# src/download.rs rather than keeping a second copy of them - here, the
# `#[cfg(target_os = "macos")]` list - and refuses to stage anything whose
# SHA-256 does not match. A mismatch means the release asset moved, and an
# unverified binary must not end up inside a bundle we sign.
#
# The transcription engine is deliberately NOT staged: it is 80 MB+ and the app
# downloads it on first run, same as on Windows.

set -euo pipefail

BIN_DIR="${1:?usage: stage_bin.sh <bin-dir>}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CACHE_DIR="$REPO_ROOT/packaging/.cache"
DOWNLOAD_RS="$REPO_ROOT/src/download.rs"

mkdir -p "$BIN_DIR" "$CACHE_DIR"

# --- Read the macOS half of the manifest -------------------------------------
# Emits one "id|version|url|sha256|kind|exe_name" line per ToolSpec.
manifest() {
	# POSIX awk only - macOS ships BSD awk, which has no gawk `match(s, re, arr)`.
	awk '
		function quoted(line,   v) {      # first "..." on the line
			v = line
			sub(/^[^"]*"/, "", v)
			sub(/".*$/, "", v)
			return v
		}
		function after_colons(line,   v) { # ToolId::Ffmpeg, -> Ffmpeg
			v = line
			sub(/^.*::/, "", v)
			sub(/[^A-Za-z0-9_].*$/, "", v)
			return v
		}
		/#\[cfg\(target_os = "macos"\)\]/ { in_macos = 1 }
		in_macos && /^pub const MANIFEST/ { in_list = 1; next }
		in_list && /^\];/ { exit }
		in_list && /ToolSpec \{/ { id=""; version=""; url=""; sha=""; kind=""; exe=""; next }
		in_list && /id: ToolId::/        { id = after_colons($0) }
		in_list && /version: "/          { version = quoted($0) }
		in_list && /url: "/              { url = quoted($0) }
		in_list && /sha256: Some\("/     { sha = quoted($0) }
		in_list && /kind: PayloadKind::/ { kind = after_colons($0) }
		in_list && /exe_name: "/         { exe = quoted($0) }
		in_list && /^    \},/ {
			if (id != "") print id "|" version "|" url "|" sha "|" kind "|" exe
		}
	' "$DOWNLOAD_RS"
}

sha_of() { shasum -a 256 "$1" | cut -d" " -f1; }

fetch() { # fetch <url> <sha256> <version> -> prints cached path
	local url="$1" sha="$2" version="$3"
	local name="${url##*/}"
	# Version-qualify the cache name: yt-dlp_macos has the same filename in
	# every release, and a stale hit would ship the wrong build.
	local cached="$CACHE_DIR/$version-$name"

	if [[ -f "$cached" && "$(sha_of "$cached")" == "$sha" ]]; then
		echo "  cached   $(basename "$cached")" >&2
		printf '%s' "$cached"
		return
	fi

	echo "  download $url" >&2
	curl -fsSL --retry 3 -o "$cached.part" "$url"

	local actual
	actual="$(sha_of "$cached.part")"
	if [[ "$actual" != "$sha" ]]; then
		rm -f "$cached.part"
		echo "SHA-256 mismatch for $url" >&2
		echo "  expected $sha" >&2
		echo "  got      $actual" >&2
		echo "The pinned asset changed. Do not ship this." >&2
		exit 1
	fi
	mv "$cached.part" "$cached"
	echo "  verified $sha" >&2
	printf '%s' "$cached"
}

# --- Stage ffmpeg, deno and yt-dlp -------------------------------------------

staged=0
while IFS="|" read -r id version url sha kind exe; do
	case "$id" in
		Ffmpeg|Deno|YtDlp) ;;
		*) continue ;;   # the engine is downloaded by the app, not bundled
	esac

	echo "$id $version"
	asset="$(fetch "$url" "$sha" "$version")"

	case "$kind" in
		RawExe)
			cp "$asset" "$BIN_DIR/$exe"
			;;
		Zip)
			work="$(mktemp -d)"
			unzip -q -o "$asset" -d "$work"
			found="$(find "$work" -type f -name "$exe" -print -quit)"
			if [[ -z "$found" ]]; then
				echo "$exe not found inside $(basename "$asset")" >&2
				exit 1
			fi
			cp "$found" "$BIN_DIR/$exe"
			rm -rf "$work"
			;;
		*)
			echo "unsupported payload kind $kind for $id" >&2
			exit 1
			;;
	esac

	chmod +x "$BIN_DIR/$exe"
	staged=$((staged + 1))
done < <(manifest)

if [[ "$staged" -ne 3 ]]; then
	echo "expected to stage 3 tools, staged $staged - is the macOS MANIFEST intact?" >&2
	exit 1
fi

echo
echo "staged in $BIN_DIR"
for f in "$BIN_DIR"/*; do
	printf '%-10s %6s MB  %s\n' "$(basename "$f")" \
		"$(( $(stat -f%z "$f") / 1048576 ))" "$(sha_of "$f")"
done
