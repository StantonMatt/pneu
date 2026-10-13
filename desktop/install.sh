#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
export QMAKE="${QMAKE:-qmake6}"
# Use the machine's job-limiting wrapper when present; otherwise build directly.
build=(cargo build --release --locked -p pneud)
if command -v heavy >/dev/null 2>&1; then
    heavy "${build[@]}"
elif [[ -x "$HOME/.local/bin/heavy" ]]; then
    "$HOME/.local/bin/heavy" "${build[@]}"
else
    "${build[@]}"
fi
install -Dm755 target/release/pneud "$HOME/.local/bin/pneud"
install -Dm644 assets/pneu.svg "$HOME/.local/share/icons/hicolor/scalable/apps/pneu.svg"
install -Dm644 packaging/pneu.desktop "$HOME/.local/share/applications/pneu.desktop"
for icon in assets/pneu-tray-*.svg; do
    install -Dm644 "$icon" "$HOME/.local/share/pneu/icons/hicolor/scalable/status/${icon##*/}"
done
install -Dm644 packaging/pneud.service "$HOME/.config/systemd/user/pneud.service"
systemctl --user daemon-reload
# Reenable removes an older default.target link as well as installing the new one.
systemctl --user reenable pneud.service
if systemctl --user is-active --quiet graphical-session.target; then
    systemctl --user restart pneud.service
fi
