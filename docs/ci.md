# CI and releases

[GitHub](https://github.com/StantonMatt/pneu) is the main host.
GitLab is a read-only mirror updated from GitHub every five minutes; do not push
or open merge requests there.

## Checks

[CI](../.github/workflows/ci.yml) runs on pull requests and pushes to `main`,
using GitHub-hosted runners:

- `ubuntu-latest`: the complete desktop Rust test suite with Qt 6, then the Linux
  package reproducibility and lint checks on the CI-built binary.
- `macos-15` (Apple Silicon, arm64): the complete desktop Rust test suite.
- `windows-latest`: the Windows filesystem and shared daemon tests with
  `cargo test --locked -p pneud --features windows-package`.
- `ubuntu-latest` for Android: Java 21, the Android SDK, debug unit tests and
  debug lint, without release signing.

## Download site

[Pages](../.github/workflows/pages.yml) deploys `site/` to
https://stantonmatt.github.io/pneu/ when it changes on `main`, or through a manual
workflow dispatch. Deployment uses the `github-pages` environment and GitHub's
Pages artifact and OIDC actions.

## Desktop releases

[Desktop release](../.github/workflows/release.yml) builds `v*` tags and checks
that the tag matches the desktop workspace version. It uses GitHub-hosted
`macos-15`, `ubuntu-24.04` and `windows-latest` runners:

- macOS: an ad-hoc-signed `Pneu-macOS.zip`, without notarization.
- Linux: `pneu_<version>_amd64.deb` and `pneu-linux-x86_64.tar.gz`, built on the
  Ubuntu 24.04 glibc/Qt baseline.
- Windows: an unsigned Store MSIX retained as a workflow artifact. Store
  submission and signing are separate; the MSIX is not attached to the release.

Tag pushes publish the macOS and Linux downloads plus `SHA256SUMS.txt` to the
[GitHub Release](https://github.com/StantonMatt/pneu/releases/latest). Publication
waits for the macOS and Linux builds and creates the release or replaces its
assets. A manual workflow dispatch takes an existing `tag` and builds artifacts
only, without publishing a release.
