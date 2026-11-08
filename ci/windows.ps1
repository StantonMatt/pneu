# Dot-source: selects Rust for this process, without changing the runner default.
param([switch]$Release)
$ErrorActionPreference = 'Stop'
function Check-Exit([string]$Step) { if ($LASTEXITCODE -ne 0) { throw "$Step failed ($LASTEXITCODE)" } }
$env:PATH = "$env:CARGO_HOME/bin;$env:USERPROFILE/.cargo/bin;$env:PATH"
$source = '.'
if ($Release) {
    if ($env:RELEASE_TAG -notlike 'v*') { throw 'Expected a version tag' }
    git check-ref-format "refs/tags/$env:RELEASE_TAG"
    Check-Exit 'tag validation'
    git fetch --no-tags origin "refs/tags/${env:RELEASE_TAG}:refs/tags/${env:RELEASE_TAG}"
    Check-Exit 'fetch tag'
    New-Item -ItemType Directory release-source -Force | Out-Null
    # Avoid PowerShell's text pipeline corrupting the binary tar stream.
    git archive "refs/tags/$env:RELEASE_TAG" -o release-source.tar
    Check-Exit 'git archive'
    tar -xf release-source.tar -C release-source
    Check-Exit 'extract release'
    Remove-Item -LiteralPath release-source.tar
    $source = 'release-source'
}
$manifest = Get-Content "$source/desktop/Cargo.toml" -Raw
$package = [regex]::Match($manifest, '(?s)\[workspace\.package\](.*?)(?:\n\[|$)').Groups[1].Value
$rust = [regex]::Match($package, '(?m)^rust-version\s*=\s*"([^"]+)"').Groups[1].Value
$version = [regex]::Match($package, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if (!$rust -or !$version) { throw 'Missing workspace version or rust-version' }
if ($Release -and $env:RELEASE_TAG -ne "v$version") { throw 'Tag must match workspace version' }
if (!(Get-Command rustup -ErrorAction SilentlyContinue)) {
    New-Item -ItemType Directory "$env:CI_PROJECT_DIR/.ci-cache" -Force | Out-Null
    $installer = "$env:CI_PROJECT_DIR/.ci-cache/rustup-init.exe"
    Invoke-WebRequest https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe -OutFile $installer
    & $installer -y --no-modify-path --profile minimal --default-toolchain none
    Check-Exit 'install rustup'
}
rustup toolchain install $rust --profile minimal
Check-Exit 'install Rust toolchain'
$env:RUSTUP_TOOLCHAIN = $rust
cargo --version
Check-Exit 'cargo version'
