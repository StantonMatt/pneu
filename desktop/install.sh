#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
/home/mjstanton/.local/bin/heavy cargo build --release --locked -p pneud
install -Dm755 target/release/pneud "$HOME/.local/bin/pneud"
install -Dm644 packaging/pneud.service "$HOME/.config/systemd/user/pneud.service"
systemctl --user daemon-reload
# Reenable removes an older default.target link as well as installing the new one.
systemctl --user reenable pneud.service
if systemctl --user is-active --quiet graphical-session.target; then
    systemctl --user restart pneud.service
fi
