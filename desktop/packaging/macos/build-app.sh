#!/usr/bin/env bash
# Run on macOS. Does not install or register the resulting bundle.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
profile=debug
out="$root/target/macos"
bundle_id=io.github.stantonmatt.pneu
while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile) profile=${2:?--profile needs debug or release}; shift 2 ;;
        --out) out=${2:?--out needs a directory}; shift 2 ;;
        --bundle-id) bundle_id=${2:?--bundle-id needs an identifier}; shift 2 ;;
        -h|--help) echo 'build-app.sh [--profile debug|release] [--out DIR] [--bundle-id io.github.stantonmatt.pneu.test|io.github.stantonmatt.pneu.test.pkg|io.github.stantonmatt.pneu.test.pkg2]'; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ "$profile" == debug || "$profile" == release ]] || { echo 'Expected debug or release profile' >&2; exit 2; }
[[ "$bundle_id" == io.github.stantonmatt.pneu || "$bundle_id" == io.github.stantonmatt.pneu.test || "$bundle_id" == io.github.stantonmatt.pneu.test.pkg || "$bundle_id" == io.github.stantonmatt.pneu.test.pkg2 ]] || { echo 'Unsupported bundle identifier' >&2; exit 2; }
[[ $(uname -s) == Darwin ]] || { echo 'Build Pneu.app on macOS' >&2; exit 1; }
cd "$root"
build=(cargo build --locked -p pneud)
if [[ "$profile" == release ]]; then
    build+=(--release)
    if command -v heavy >/dev/null 2>&1; then heavy "${build[@]}"; else "$HOME/.local/bin/heavy" "${build[@]}"; fi
else
    "${build[@]}"
fi
# pneud inherits this workspace version. Avoid another Cargo invocation/build.
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' Cargo.toml)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'Expected a numeric workspace version' >&2; exit 1; }
mkdir -p "$out"
out=$(cd "$out" && pwd)
stage=$(mktemp -d "$out/.pneu-build.XXXXXX")
cleanup() {
    if [[ -d "$stage/previous.app" && ! -e "$out/Pneu.app" && ! -L "$out/Pneu.app" ]]; then
        mv "$stage/previous.app" "$out/Pneu.app"
    fi
    if [[ -d "$stage/previous.app" ]]; then
        "$out/Pneu.app/Contents/MacOS/pneud" uninstall-owned bundle "$stage/previous.app"
        if [[ -d "$stage/previous.app" ]]; then echo "Kept $stage: previous bundle contains other files" >&2; return; fi
    fi
    rm -rf -- "$stage"
}
trap cleanup EXIT
app="$stage/Pneu.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/menubar"
cp "${CARGO_TARGET_DIR:-target}/$profile/pneud" "$app/Contents/MacOS/pneud"
chmod 755 "$app/Contents/MacOS/pneud"
sed -e "s/@VERSION@/$version/g" -e "s/@BUNDLE_ID@/$bundle_id/g" packaging/macos/Info.plist.in > "$app/Contents/Info.plist"
agent="$app/Contents/Resources/daemon.plist.in"
sed "s/@BUNDLE_ID@/$bundle_id/g" packaging/macos/io.github.stantonmatt.pneu.daemon.plist.in > "$agent"
iconset="$stage/AppIcon.iconset"
mkdir -p "$iconset"
if [[ -d assets/macos/AppIcon.iconset ]]; then
    cp assets/macos/AppIcon.iconset/*.png "$iconset/"
else
    master=
    for candidate in assets/macos/AppIcon-1024.png assets/macos/app-icon-1024.png assets/macos/AppIcon.png assets/macos/app-icon.png; do
        if [[ -f "$candidate" ]]; then master=$candidate; break; fi
    done
    if [[ -z "$master" ]]; then
        echo 'Designer icon set unavailable; using a temporary icon rendered from assets/pneu.svg' >&2
        qlmanage -t -s 1024 -o "$stage" assets/pneu.svg >/dev/null
        master="$stage/pneu.svg.png"
    fi
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$master" --out "$iconset/icon_${size}x${size}.png" >/dev/null
        double=$((size * 2))
        sips -z "$double" "$double" "$master" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    done
fi
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
# Native tray projection uses the same pneu-tray-* names as Linux.
for image in assets/macos/menubar-*.png; do
    [[ -f "$image" ]] || continue
    name=${image##*/}
    cp "$image" "$app/Contents/Resources/menubar/pneu-tray-${name#menubar-}"
done
plutil -lint "$app/Contents/Info.plist" "$agent"
codesign --force --sign - --options runtime "$app"
codesign --verify --strict --verbose=2 "$app"
# Publish only after validation; never recursively remove a caller-selected path.
if [[ -e "$out/Pneu.app" || -L "$out/Pneu.app" ]]; then
    [[ -d "$out/Pneu.app" && ! -L "$out/Pneu.app" && -f "$out/Pneu.app/Contents/Info.plist" ]] || { echo 'Refusing to replace a non-bundle output' >&2; exit 1; }
    previous_id=$(plutil -extract CFBundleIdentifier raw -o - "$out/Pneu.app/Contents/Info.plist")
    [[ "$previous_id" == "$bundle_id" ]] || { echo 'Refusing to replace a bundle with a different identifier' >&2; exit 1; }
    "$app/Contents/MacOS/pneud" uninstall-owned check-bundle "$out/Pneu.app"
    mv "$out/Pneu.app" "$stage/previous.app"
fi
mv "$app" "$out/Pneu.app"
printf '%s\n' "$out/Pneu.app"
