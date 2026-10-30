#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
source packaging/linux/install-user.sh
if [[ "${1:-}" == --uninstall ]]; then
    pneu_uninstall_user "$PWD"
    exit
fi
if [[ $# != 0 ]]; then
    echo "Usage: $0 [--uninstall]" >&2
    exit 2
fi
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
pneu_install_user target/release/pneud "$PWD"
