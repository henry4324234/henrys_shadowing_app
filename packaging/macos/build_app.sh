#!/usr/bin/env bash
#
# Builds "Henry's Shadowing App.app" and wraps it in a .dmg. Must run on macOS
# (it needs sips, iconutil, codesign and hdiutil); CI does this on a runner,
# since the project is otherwise developed on Windows.
#
#     ./packaging/macos/build_app.sh [version]
#
# Apple Silicon only, deliberately: the ffmpeg and deno pinned for macOS in
# src/download.rs are arm64 builds, so a universal app would carry helpers half
# of it could not run. Intel Macs are not a target yet.
#
# The result is unsigned beyond an ad-hoc signature. That is enough for the
# binary to launch on arm64 - which refuses unsigned code outright - but not
# enough for Gatekeeper, so a downloaded copy needs right-click - Open, or
# `xattr -dr com.apple.quarantine`. Real signing needs a paid Apple Developer
# account; when there is one, replace the ad-hoc codesign below with the
# Developer ID identity and add a notarytool submission.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
	VERSION="$(awk '/^\[package\]/ { p = 1; next } p && /^version = "/ { sub(/^version = "/, ""); sub(/".*$/, ""); print; exit }' Cargo.toml)"
fi
if [[ -z "$VERSION" ]]; then
	echo "could not read version from Cargo.toml - pass it as an argument" >&2
	exit 1
fi

TARGET="aarch64-apple-darwin"
DIST="$REPO_ROOT/packaging/dist/mac"
APP="$DIST/Henry's Shadowing App.app"
DMG="$REPO_ROOT/packaging/dist/HenrysShadowingApp-$VERSION-AppleSilicon.dmg"

echo "Henry's Shadowing App $VERSION ($TARGET)"

# --- 1. Compile ---------------------------------------------------------------
echo
echo "[1/5] cargo build --release --target $TARGET"
rustup target add "$TARGET" >/dev/null
cargo build --release --target "$TARGET"

# --- 2. Assemble the bundle ---------------------------------------------------
echo
echo "[2/5] assembling the .app"
rm -rf "$DIST"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/bin"

cp "target/$TARGET/release/henrys_shadowing_app" "$APP/Contents/MacOS/"
chmod +x "$APP/Contents/MacOS/henrys_shadowing_app"

sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

# .icns from the same source png the Windows .ico comes from, so both platforms
# show the same icon. macOS picks the size it wants out of the set.
ICONSET="$DIST/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
	sips -z "$size" "$size" assets/icon.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
	double=$((size * 2))
	sips -z "$double" "$double" assets/icon.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

# --- 3. Bundle ffmpeg / deno / yt-dlp ------------------------------------------
echo
echo "[3/5] staging Contents/Resources/bin"
./packaging/macos/stage_bin.sh "$APP/Contents/Resources/bin"

# --- 4. Sign ------------------------------------------------------------------
echo
echo "[4/5] ad-hoc signing"
# Sign the nested helpers first: --deep is deprecated and misses things, and an
# unsigned arm64 helper is killed the moment it is spawned.
for helper in "$APP/Contents/Resources/bin/"*; do
	codesign --force --timestamp=none --sign - "$helper"
done
codesign --force --timestamp=none --sign - "$APP"
codesign --verify --verbose=2 "$APP"

# --- 5. Package ---------------------------------------------------------------
echo
echo "[5/5] building the disk image"
STAGE="$DIST/dmg"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"   # the drag-to-install gesture
rm -f "$DMG"
hdiutil create \
	-volname "Henry's Shadowing App" \
	-srcfolder "$STAGE" \
	-ov -format UDZO \
	"$DMG" >/dev/null
rm -rf "$STAGE"

SHA="$(shasum -a 256 "$DMG" | cut -d" " -f1)"
printf '%s  %s\n' "$SHA" "$(basename "$DMG")" > "$DMG.sha256"

echo
echo "$DMG"
echo "$(( $(stat -f%z "$DMG") / 1048576 )) MB"
echo "sha256 $SHA"
