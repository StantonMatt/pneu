#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$root/install-user.sh"
case "${1:-}" in
    --uninstall) pneu_uninstall_user "$root" ;;
    '') pneu_install_user "$root/pneud" "$root" ;;
    *) echo "Usage: $0 [--uninstall]" >&2; exit 2 ;;
esac
