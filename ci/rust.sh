#!/usr/bin/env bash
# Source this file so the selected toolchain applies only to this job.
set -euo pipefail
mkdir -p "${CARGO_HOME:?}"
export PATH="${CARGO_HOME:?}/bin:$HOME/.cargo/bin:$PATH"
rust_version=$(python3 -c 'import sys,tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["workspace"]["package"]["rust-version"])' "$1/Cargo.toml")
if ! command -v rustup >/dev/null 2>&1; then
    curl --fail --silent --show-error --location https://sh.rustup.rs -o "$CI_PROJECT_DIR/.ci-cache/rustup-init.sh"
    sh "$CI_PROJECT_DIR/.ci-cache/rustup-init.sh" -y --no-modify-path --default-toolchain none --profile minimal
fi
rustup toolchain install "$rust_version" --profile minimal
export RUSTUP_TOOLCHAIN="$rust_version"
cargo --version
