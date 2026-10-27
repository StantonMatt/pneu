# Requires Rust/MSVC and Windows SDK MakePri/MakeAppx/SignTool. Assets are checked in;
# regenerate with: python desktop/packaging/windows/generate-assets.py (Pillow).
[CmdletBinding()]
param(
    [string]$IdentityName = 'StantonMatt.Pneu.Dev',
    [string]$Publisher = 'CN=Pneu Dev',
    [string]$PublisherDisplayName = 'Matthew Stanton',
    [string]$Version,
    [string]$OutDir = (Join-Path $PSScriptRoot '../../../dist/windows'),
    [string]$SdkBinPath,
    [string]$CertificateThumbprint
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
function Check-Exit([string]$Step) { if ($LASTEXITCODE -ne 0) { throw "$Step failed ($LASTEXITCODE)" } }
function Xml([string]$Value) { [System.Security.SecurityElement]::Escape($Value) }
if ($env:OS -ne 'Windows_NT') { throw 'MSIX packaging requires Windows' }
if ($IdentityName -notmatch '^[A-Za-z0-9.-]{3,50}$') { throw 'Invalid package identity name' }
if ([string]::IsNullOrWhiteSpace($Publisher) -or [string]::IsNullOrWhiteSpace($PublisherDisplayName)) { throw 'Publisher fields are required' }
$workspace = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
Push-Location $workspace
$stage = $null
$previousRustflags = [Environment]::GetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS')
try {
    # Ship a self-contained desktop app. Packaging owns these flags; restore
    # the caller's environment afterward. The explicit target confines this to
    # Windows target code (including bundled SQLite), not host build tools.
    $env:CARGO_ENCODED_RUSTFLAGS = '-C' + [char]31 + 'target-feature=+crt-static'

    $metadataText = & cargo metadata --locked --no-deps --format-version 1
    Check-Exit 'cargo metadata'
    $metadata = $metadataText | ConvertFrom-Json
    $crate = $metadata.packages | Where-Object name -eq 'pneud'
    if (!$Version) {
        if ($crate.version -notmatch '^\d+\.\d+\.\d+$') { throw 'Supply a four-part MSIX Version for a prerelease crate' }
        $Version = "$($crate.version).0"
    }
    if ($Version -notmatch '^\d+\.\d+\.\d+\.0$' -or @($Version.Split('.') | Where-Object { [long]$_ -gt 65535 }).Count -gt 0 -or [int]$Version.Split('.')[0] -eq 0) {
        throw 'Version must be four 16-bit integers, nonzero major, revision 0 (Store requirement)'
    }
    if (!$SdkBinPath) {
        $kits = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots').KitsRoot10
        $SdkBinPath = Get-ChildItem (Join-Path $kits 'bin') -Directory |
            Where-Object { Test-Path (Join-Path $_.FullName 'x64/makeappx.exe') } |
            Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1 -ExpandProperty FullName
        if ($SdkBinPath) { $SdkBinPath = Join-Path $SdkBinPath 'x64' }
    }
    if (!$SdkBinPath -or !(Test-Path "$SdkBinPath/makeappx.exe")) { throw 'Windows SDK MakeAppx not found; supply -SdkBinPath' }
    if (!(Test-Path "$SdkBinPath/makepri.exe")) { throw 'Windows SDK MakePri not found' }
    & cargo build --release --locked --target x86_64-pc-windows-msvc -p pneud --features windows-package --bin Pneu --bin pneud
    Check-Exit 'cargo build'
    $OutDir = [IO.Path]::GetFullPath($OutDir)
    New-Item -ItemType Directory $OutDir -Force | Out-Null
    $stage = Join-Path $OutDir ('.stage-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $stage | Out-Null
    Copy-Item (Join-Path $metadata.target_directory 'x86_64-pc-windows-msvc/release/Pneu.exe'),
              (Join-Path $metadata.target_directory 'x86_64-pc-windows-msvc/release/pneud.exe') $stage
    Copy-Item "$PSScriptRoot/Assets" $stage -Recurse
    $manifest = Get-Content "$PSScriptRoot/AppxManifest.xml.in" -Raw
    $manifest = $manifest.Replace('@IDENTITY_NAME@', (Xml $IdentityName)).Replace('@PUBLISHER@', (Xml $Publisher)).Replace('@PUBLISHER_DISPLAY_NAME@', (Xml $PublisherDisplayName)).Replace('@VERSION@', $Version)
    [IO.File]::WriteAllText("$stage/AppxManifest.xml", $manifest, [Text.UTF8Encoding]::new($false))
    # Index scale/targetsize candidates into one PRI (no split resource packs).
    & "$SdkBinPath/makepri.exe" new /pr $stage /cf "$PSScriptRoot/priconfig.xml" /of "$stage/resources.pri" /o
    Check-Exit 'MakePri'
    $package = Join-Path $OutDir "Pneu-$Version-x64.msix"
    & "$SdkBinPath/makeappx.exe" pack /d $stage /p $package /o
    Check-Exit 'MakeAppx'
    if ($CertificateThumbprint) {
        & "$SdkBinPath/signtool.exe" sign /fd SHA256 /s My /sha1 $CertificateThumbprint $package
        Check-Exit 'SignTool'
    }
    Write-Output $package
} finally {
    [Environment]::SetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS', $previousRustflags)
    if ($stage -and (Test-Path $stage)) { Remove-Item $stage -Recurse -Force }
    Pop-Location
}
