#!/usr/bin/env bash
# Build "Ghost May Cry.app", the menu-bar app, from crates/ghostmc-desktop.
#
# Usage: scripts/build_macos_app.sh [--debug] [--out DIR]
#
#   --debug     build the dev profile instead of release
#   --out DIR   where to put the app (default: target/macos)
#
# Environment:
#   SIGN_IDENTITY  codesign identity (default "-": ad-hoc, for this Mac only;
#                  macOS asks for Automation again after each ad-hoc rebuild)
#   BUNDLE_ID      bundle identifier (default io.github.tjcccc.ghostmaycry)
#
# The `ghostmc` CLI (hooks, host) is not bundled; install it separately with
# `cargo install --path crates/ghostmc-cli`.
set -euo pipefail

APP_NAME="Ghost May Cry"
EXECUTABLE="ghostmc-desktop"
BUNDLE_ID="${BUNDLE_ID:-io.github.tjcccc.ghostmaycry}"
SIGN_IDENTITY="${SIGN_IDENTITY:--}"

profile="release"
out=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug) profile="dev"; shift ;;
        --out)
            [[ $# -ge 2 ]] || { echo "--out needs a directory" >&2; exit 2; }
            out="$2"; shift 2 ;;
        -h|--help) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "this builds a macOS app; run it on macOS" >&2
    exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
out="${out:-$root/target/macos}"

echo "==> cargo build ($profile) -p $EXECUTABLE"
cargo build --locked --profile "$profile" -p "$EXECUTABLE"
target_dir="$(cargo metadata --no-deps --format-version 1 \
    | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
if [[ "$profile" == "dev" ]]; then binary="$target_dir/debug/$EXECUTABLE"; else binary="$target_dir/release/$EXECUTABLE"; fi
version="$(cargo metadata --no-deps --format-version 1 \
    | python3 -I -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == sys.argv[1]))' "$EXECUTABLE")"

app="$out/$APP_NAME.app"
echo "==> $app ($version)"
mkdir -p "$out"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/$EXECUTABLE"

# LSUIElement: menu-bar app, no Dock icon even before it starts.
# LSEnvironment PATH: apps opened from Finder get only the system PATH, which
#   misses Homebrew's tmux and git; `[tmux] binary` in config.toml can also
#   name an absolute path.
# NSAppleEventsUsageDescription: required to script Ghostty (Automation).
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleExecutable</key>
    <string>$EXECUTABLE</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.15</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSAppleEventsUsageDescription</key>
    <string>Ghost May Cry lists, focuses and opens your Ghostty terminals.</string>
    <key>LSEnvironment</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
</dict>
</plist>
PLIST
plutil -lint -s "$app/Contents/Info.plist"

echo "==> codesign (${SIGN_IDENTITY/#-/ad-hoc})"
codesign --force --sign "$SIGN_IDENTITY" "$app"
codesign --verify --strict "$app"

echo "Built $app"
echo "Run it with: open \"$app\""
