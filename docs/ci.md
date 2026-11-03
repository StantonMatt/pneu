# CI and releases

[GitLab](https://gitlab.com/stantonmatthewj/pneu) is the current host.
`.gitlab-ci.yml` tests merge requests and pushes to `main`: the complete desktop
workspace on Linux and Apple Silicon macOS, the Windows-compilable `pneud`
targets with `windows-package`, and Android debug unit tests and lint. Linux also
checks reproducible `.deb` and tarball packaging. Superseded test jobs cancel;
release and Pages jobs finish without cancellation. Fork MRs run in their fork.

Linux and Android use disposable Docker jobs tagged `ci-public`. Linux release
builds stay on Ubuntu 24.04 for the glibc/Qt baseline. Android uses Java 21 and
SDK platform `android-37.2` / build tools `37.0.0`, without release signing.
Windows uses GitLab's `saas-windows-medium-amd64` runner with MSVC and Windows
SDK tools. It installs the workspace Rust version if needed. These Windows jobs
spend hosted-runner minutes.

The `macos` shell runner is Matthew's Apple Silicon Mac and runs only while it
is awake. It needs Xcode command-line tools, Python 3.11+, rustup and
`~/.local/bin/heavy`. Jobs select the workspace Rust version without changing
the user's default toolchain, and use a job-local temporary directory. No Qt,
Java or Android SDK is required on the Mac. Only trusted project contributors
may run pipelines against this shell runner.

## Pages

The `pages` job publishes `site/` on pushes to `main` changing `site/**`, or a
manual pipeline on `main` with `RELEASE_TAG` empty. The public site is
https://stantonmatthewj.gitlab.io/pneu/. Project Pages access must be **Everyone**
and unique domains disabled to retain that URL. The site links directly to
GitLab Release assets; there is no GitHub OIDC or Pages deployment credential.

## Releases

Push a `v*` tag matching `desktop/Cargo.toml` to build the macOS zip, Linux `.deb`
and tarball, and unsigned Store MSIX. The release jobs build the tag's source,
while retaining CI scripts from the pipeline ref. The macOS and Linux artifacts
are checksummed and uploaded with `CI_JOB_TOKEN` to the generic package registry;
GitLab Release links provide stable latest-download URLs. Each publishing job
uses a fresh package version so a retry can update links without overwriting an
existing package. Existing release notes are replaced with the installation
link. The MSIX stays a seven-day pipeline artifact for Partner Center; CI does
not submit or sign it for the Store. Publication depends on macOS and Linux,
as in the previous workflow, independently of Windows packaging.

For a dry run, choose **Build → Pipelines → New pipeline**, select the CI branch,
and set `RELEASE_TAG` to an existing version tag (for example `v2.2.2`). Or run:

```sh
glab ci run --branch gitlab-ci --variables-env RELEASE_TAG:v2.2.2
```

Manual/web/API builds keep artifacts for seven days and publish nothing automatically. This
also lets the migration branch validate existing tags before CI is on `main`.
After verifying the dry-run artifacts and pipeline, play the optional manual
`republish` job to publish that existing release on GitLab. This is an explicit
publication gate, separate from the dry run. Do not play it until its dry run
has been checked.

## If moving back to GitHub

`.github/workflows/` is the inactive GitHub version and is preserved unchanged.
Keep it in sync by hand only if a switch back happens. Re-enable those workflows
and deactivate GitLab pipelines; GitHub release publication still uses its
original `gh release` path. Point `origin` back to the saved `github` SSH remote,
restore GitHub Pages and the repository's GitHub Actions permissions, and change
site/download/privacy links back to `https://stantonmatt.github.io/pneu/` and
`https://github.com/StantonMatt/pneu` (including Android's privacy URL and test).
For Linux package metadata, set
`PNEU_SITE_URL=https://stantonmatt.github.io/pneu/` when building. Also restore
the manpage URL and external Play/Store listing URLs. The GitLab equivalents
are listed in the migration report outside the repository.
