#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
applications="$HOME/Applications"
purge=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --system) applications=/Applications; shift ;;
        --purge) purge=true; shift ;;
        -h|--help) echo 'uninstall-macos.sh [--system] [--purge]'; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ $(uname -s) == Darwin ]] || { echo 'Run this uninstaller on macOS' >&2; exit 1; }
# shellcheck source=desktop/packaging/macos/common.sh
source "$root/packaging/macos/common.sh"
app="$applications/Pneu.app"
cli="$HOME/.local/bin/pneud"
# The bundled remover unlinks only named owned entries, using no-follow fds.
if $purge; then
    [[ -n "$HOME" && "$HOME" != / ]] || { echo 'Unsafe HOME' >&2; exit 1; }
    [[ -e "$app" ]] || { echo 'Purge needs the bundled entry remover; reinstall Pneu then retry; data retained.' >&2; exit 1; }
fi
# Preserve an unrelated CLI, including one pointing to a different installation.
if [[ -e "$app" || -L "$app" ]]; then
    pneu_check_bundle "$app"
    pneu_stop_bundle "$app"
    if $purge; then "$app/Contents/MacOS/pneud" uninstall-owned purge "$HOME"; fi
    "$app/Contents/MacOS/pneud" uninstall-owned bundle "$app"
else
    pneu_refuse_running
fi
if [[ -L "$cli" && $(readlink "$cli") == "$app/Contents/MacOS/pneud" ]]; then rm -- "$cli"; fi
printf 'Uninstalled %s\n' "$app"
if ! $purge; then echo 'User data and received files retained.'; fi
