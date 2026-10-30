#!/usr/bin/env bash
# Shared by source and binary installers. Sourcing this file has no side effects.
pneu_install_user() {
    local binary=$1 root=$2
    local data=${XDG_DATA_HOME:-$HOME/.local/share}
    local config=${XDG_CONFIG_HOME:-$HOME/.config}
    install -Dm755 "$binary" "$HOME/.local/bin/pneud"
    install -Dm644 "$root/assets/pneu.svg" "$data/icons/hicolor/scalable/apps/pneu.svg"
    install -Dm644 "$root/packaging/pneu.desktop" "$data/applications/pneu.desktop"
    install -Dm755 "$root/packaging/pneu-send.desktop" "$data/kio/servicemenus/pneu-send.desktop"
    # Tray SVGs are embedded and materialized by pneud in its data directory.
    install -Dm644 "$root/packaging/pneud.service" "$config/systemd/user/pneud.service"
    systemctl --user daemon-reload
    # Also removes the old default.target link on source-install upgrades.
    systemctl --user reenable pneud.service
    if systemctl --user is-active --quiet graphical-session.target; then
        systemctl --user restart pneud.service
    fi
}

pneu_uninstall_user() {
    local root=$1
    local data=${XDG_DATA_HOME:-$HOME/.local/share}
    local config=${XDG_CONFIG_HOME:-$HOME/.config}
    local unit=$config/systemd/user/pneud.service
    if [[ -e "$unit" ]] && ! cmp -s "$unit" "$root/packaging/pneud.service"; then
        echo "Refusing to remove a customized user unit: $unit" >&2
        return 1
    fi
    if [[ -e "$unit" ]]; then
        systemctl --user stop pneud.service
        systemctl --user disable pneud.service
        rm -- "$unit"
    fi
    rm -f -- "$HOME/.local/bin/pneud" "$data/icons/hicolor/scalable/apps/pneu.svg" \
        "$data/applications/pneu.desktop" "$data/kio/servicemenus/pneu-send.desktop"
    systemctl --user daemon-reload
    echo 'Removed the per-user installation. Pairings, settings and received files are preserved.'
}
