#!/usr/bin/env bash
set -euo pipefail
[[ $# == 3 ]] || { echo "Usage: $0 BINARY VERSION OUTPUT_DIRECTORY" >&2; exit 2; }
script=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
"$script/build-packages.sh" "$1" "$2" "$3/first"
"$script/build-packages.sh" "$1" "$2" "$3/second"
deb=$3/first/pneu_${2}_amd64.deb
cmp "$deb" "$3/second/pneu_${2}_amd64.deb"
cmp "$3/first/pneu-linux-x86_64.tar.gz" "$3/second/pneu-linux-x86_64.tar.gz"
dpkg-deb --info "$deb"
dpkg-deb --contents "$deb"
tar -tzf "$3/first/pneu-linux-x86_64.tar.gz"
if command -v lintian >/dev/null 2>&1; then
    lintian "$deb"
fi
