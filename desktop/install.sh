#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
export QMAKE="${QMAKE:-qmake6}"
/home/mjstanton/.local/bin/heavy cargo build --release --locked -p pneud
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
