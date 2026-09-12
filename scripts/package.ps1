<#
.SYNOPSIS
    Builds RTLScope and packages it as a Windows installer.

.DESCRIPTION
    Three steps, in order, because each takes what the one before made:

      cargo build --release --workspace
      wix build installer/rtlscope.wxs     -> the .msi
      wix build installer/bundle.wxs     -> the .exe around it

    Both land in target/installer. The .exe is the one to double-click; the .msi
    is the one to hand a deployment tool. Either installs the three binaries
    under %LOCALAPPDATA%\Programs\RTLScope, puts that folder on the user's PATH,
    makes a Start-menu shortcut, and adds an entry to Apps & features that takes
    all of it away again. No administrator is involved: everything it wants has
    a per-user home.

    WiX is a dotnet global tool and does not come with the Rust toolchain. Pin
    the version — v7 refuses to build without accepting the Open Source
    Maintenance Fee EULA, and stops with WIX7015:

      dotnet tool install --global wix --version 5.0.2
      wix extension add --global WixToolset.BootstrapperApplications.wixext/5.0.2

.PARAMETER Version
    Overrides the version stamped into the package. Read from the GUI's
    Cargo.toml when not given, since the window is the part a person installs.

.PARAMETER SkipBuild
    Packages whatever is already in target/release. For iterating on the
    installer itself without paying for a rebuild.

.EXAMPLE
    pwsh scripts/package.ps1
#>

[CmdletBinding()]
param(
    [string] $Version,
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$build = Join-Path $root 'target\release'
$out = Join-Path $root 'target\installer'
$assets = Join-Path $root 'assets'

# The tool installs into the dotnet tools folder and the shell that installed it
# does not always have that on PATH yet, so look in both places rather than
# telling the reader to open a new terminal.
$wix = Get-Command wix -ErrorAction SilentlyContinue
if (-not $wix) {
    $candidate = Join-Path $env:USERPROFILE '.dotnet\tools\wix.exe'
    if (Test-Path $candidate) {
        $wix = Get-Item $candidate
    } else {
        throw "wix not found. Install it with: dotnet tool install --global wix --version 5.0.2"
    }
}

if (-not $Version) {
    $manifest = Get-Content (Join-Path $root 'crates\rtlscope-gui\Cargo.toml')
    $line = $manifest | Where-Object { $_ -match '^version\s*=' } | Select-Object -First 1
    if (-not $line) { throw "no version in crates/rtlscope-gui/Cargo.toml" }
    $Version = ($line -split '"')[1]
}

if (-not $SkipBuild) {
    Write-Host "building $Version ..." -ForegroundColor Cyan
    Push-Location $root
    try {
        cargo build --release --workspace
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } finally {
        Pop-Location
    }
}

foreach ($exe in 'rtlscope-gui.exe', 'rtlscope.exe', 'rtlscope-mcp.exe') {
    $path = Join-Path $build $exe
    if (-not (Test-Path $path)) { throw "missing $path - build first, or drop -SkipBuild" }
}

New-Item -ItemType Directory -Force -Path $out | Out-Null

# MSI versions are three numbers; a pre-release suffix has nowhere to go.
$stamped = ($Version -split '-')[0]
$msi = Join-Path $out "RTLScope-$Version-x64.msi"
$setup = Join-Path $out "RTLScope-$Version-x64-setup.exe"

Write-Host "packaging $msi ..." -ForegroundColor Cyan
& $wix.Source build `
    (Join-Path $root 'installer\rtlscope.wxs') `
    -arch x64 `
    -d "Version=$stamped" `
    -d "BuildDir=$build" `
    -d "Assets=$assets" `
    -d "Root=$root" `
    -o $msi
if ($LASTEXITCODE -ne 0) { throw "wix build failed (msi)" }

Write-Host "packaging $setup ..." -ForegroundColor Cyan
& $wix.Source build `
    (Join-Path $root 'installer\bundle.wxs') `
    -arch x64 `
    -ext WixToolset.BootstrapperApplications.wixext `
    -d "Version=$stamped" `
    -d "Assets=$assets" `
    -d "Msi=$msi" `
    -o $setup
if ($LASTEXITCODE -ne 0) { throw "wix build failed (bundle)" }

foreach ($made in $setup, $msi) {
    $size = [math]::Round((Get-Item $made).Length / 1MB, 1)
    Write-Host ("{0}  ({1} MB)" -f $made, $size) -ForegroundColor Green
}

Write-Host ""
Write-Host "install:   & `"$setup`"" -ForegroundColor DarkGray
Write-Host "quietly:   & `"$setup`" /quiet" -ForegroundColor DarkGray
Write-Host "uninstall: & `"$setup`" /uninstall   (or Apps and features)" -ForegroundColor DarkGray
Write-Host ""
Write-Host 'The & is not decoration. A PowerShell line that starts with a quoted' -ForegroundColor DarkGray
Write-Host 'path is a string expression, not a command, and /quiet after one reads' -ForegroundColor DarkGray
Write-Host 'as a division: "You must provide a value expression following the ''/''".' -ForegroundColor DarkGray
