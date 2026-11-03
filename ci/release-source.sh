#!/usr/bin/env bash
# Build the requested tag while retaining CI scripts from the pipeline ref.
set -euo pipefail
[[ "${RELEASE_TAG:?}" == v* ]]
git check-ref-format "refs/tags/$RELEASE_TAG"
git fetch --no-tags origin "refs/tags/$RELEASE_TAG:refs/tags/$RELEASE_TAG"
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct "refs/tags/$RELEASE_TAG")
export SOURCE_DATE_EPOCH
mkdir -p release-source
git archive "refs/tags/$RELEASE_TAG" | tar -x -C release-source
python3 - <<'PY'
import os, tomllib
with open('release-source/desktop/Cargo.toml', 'rb') as f:
    version = tomllib.load(f)['workspace']['package']['version']
if os.environ['RELEASE_TAG'] != f'v{version}':
    raise SystemExit(f'Tag must match workspace version: expected v{version}')
PY
