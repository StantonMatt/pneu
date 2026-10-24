#!/usr/bin/env bash
# Shared installer helpers. Only the production bundle/service is managed here.
pneu_bundle_id=io.github.stantonmatt.pneu
pneu_label=$pneu_bundle_id.daemon
pneu_target="gui/$(id -u)/$pneu_label"

pneu_check_bundle() {
    local app=$1 identifier
    [[ -d "$app" && ! -L "$app" && -f "$app/Contents/Info.plist" && -x "$app/Contents/MacOS/pneud" ]] || {
        echo "Not a Pneu bundle: $app" >&2; return 1;
    }
    identifier=$(plutil -extract CFBundleIdentifier raw -o - "$app/Contents/Info.plist")
    [[ "$identifier" == "$pneu_bundle_id" ]] || { echo "Unexpected bundle identifier: $identifier" >&2; return 1; }
}

pneu_stop_bundle() {
    local app=$1 state pid count program
    state=$(launchctl print "$pneu_target" 2>/dev/null || true)
    pid=$(printf '%s\n' "$state" | awk '$1 == "pid" && $2 == "=" { print $3; exit }')
    if [[ -n "$state" ]]; then
        program=$(printf '%s\n' "$state" | sed -n 's/^[[:space:]]*program = //p')
        [[ "$program" == "$app/Contents/MacOS/pneud" ]] || {
            echo 'The launchd label belongs to a different executable; refusing to stop it' >&2; return 1;
        }
        if [[ -n "$pid" ]]; then
            [[ "$pid" =~ ^[0-9]+$ && "$pid" -gt 1 ]] || { echo 'Invalid launchd PID' >&2; return 1; }
        fi
        launchctl bootout "$pneu_target"
    fi
    # bootout asks launchd to terminate the daemon. Its SIGTERM handler
    # shuts down and reaps the persistent UI child before the daemon exits.
    if [[ -n "$pid" ]]; then
        count=0
        while kill -0 "$pid" 2>/dev/null; do
            if [[ $count -ge 150 ]]; then
                echo 'Pneu has not stopped after 15 seconds; leaving the existing bundle intact' >&2
                return 1
            fi
            sleep 0.1
            count=$((count + 1))
        done
    fi
    pneu_refuse_running
    "$app/Contents/MacOS/pneud" login-item unregister
}

# Include foreground daemons, other bundle locations and private PNEU_* paths.
# Refuse rather than signal an unmanaged PID: a process name alone is never
# authority to kill it. Also refuse while a Pneu UI/helper is still shutting down.
pneu_refuse_running() {
    local pids status=0
    pids=$(pgrep -U "$(id -u)" -x pneud) || status=$?
    if [[ $status -gt 1 ]]; then
        echo 'Cannot check for running Pneu processes; leaving bundle and data intact' >&2
        return 1
    fi
    if [[ -n "$pids" ]]; then
        echo "Pneu processes are still running for this user (PIDs: $pids). Quit Pneu, or stop foreground pneud run with Ctrl-C, then retry. Bundle and data have been retained." >&2
        return 1
    fi
}

pneu_check_cli() {
    local cli=$1 expected=$2
    if [[ -e "$cli" || -L "$cli" ]]; then
        [[ -L "$cli" && $(readlink "$cli") == "$expected" ]] || {
            echo "Refusing to overwrite an unrelated CLI: $cli" >&2; return 1;
        }
    fi
}
