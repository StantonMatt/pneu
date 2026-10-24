#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
applications="$HOME/Applications"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --system) applications=/Applications; shift ;;
        -h|--help) echo 'install-macos.sh [--system]'; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ $(uname -s) == Darwin ]] || { echo 'Run this installer on macOS' >&2; exit 1; }
# shellcheck source=desktop/packaging/macos/common.sh
source "$root/packaging/macos/common.sh"
app="$applications/Pneu.app"
cli="$HOME/.local/bin/pneud"
binary="$app/Contents/MacOS/pneud"
pneu_check_cli "$cli" "$binary"
if [[ -e "$app" || -L "$app" ]]; then pneu_check_bundle "$app"; fi
mkdir -p "$applications" "$HOME/.local/bin"
[[ -w "$applications" ]] || { echo "No write access to $applications; grant access before installing (do not run this script with sudo)" >&2; exit 1; }
"$root/packaging/macos/build-app.sh" --profile release --out "$root/target/macos"
stage=$(mktemp -d "$applications/.pneu-install.XXXXXX")
cleanup() {
    if [[ -d "$stage/previous.app" && ! -e "$app" && ! -L "$app" ]]; then
        mv "$stage/previous.app" "$app"
    fi
    if [[ -d "$stage/previous.app" ]]; then
        "$app/Contents/MacOS/pneud" uninstall-owned bundle "$stage/previous.app"
        # Preserve even an unexpected concurrent addition to the old bundle.
        if [[ -d "$stage/previous.app" ]]; then echo "Kept $stage: previous bundle contains other files" >&2; return; fi
    fi
    rm -rf -- "$stage"
}
trap cleanup EXIT
cp -R "$root/target/macos/Pneu.app" "$stage/Pneu.app"
codesign --verify --strict "$stage/Pneu.app"
"$stage/Pneu.app/Contents/MacOS/pneud" uninstall-owned check-bundle "$app"
if [[ -d "$app" ]]; then
    login_status=$("$binary" login-item status)
    pneu_stop_bundle "$app"
    mv "$app" "$stage/previous.app"
else
    pneu_refuse_running
fi
if ! mv "$stage/Pneu.app" "$app"; then
    if [[ -d "$stage/previous.app" ]]; then mv "$stage/previous.app" "$app"; fi
    exit 1
fi
ln -sfn "$binary" "$cli"
case "${login_status:-new}" in
    enabled|requires\ approval*) status=$("$binary" login-item register) ;;
    new) status=$("$binary" login-item initialize) ;;
    'not registered'|'not found') status=$("$binary" login-item unregister) ;;
    *) echo "Unexpected previous login status: $login_status" >&2; exit 1 ;;
esac
printf 'Login item: %s\n' "$status"
[[ "$status" == enabled || "$status" == 'not registered' || "$status" == 'not found' ]] || { echo 'Pneu is installed; approve it in System Settings → General → Login Items, then open Pneu.app' >&2; exit 1; }
"$binary"
printf 'Installed %s\nCLI: %s\n' "$app" "$cli"
