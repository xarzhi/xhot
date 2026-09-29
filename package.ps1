
# powershell -ExecutionPolicy Bypass -File .\package.ps1 0.3.0
<#
.SYNOPSIS
    Builds the xshot installer (NSIS setup .exe) for a given version.

.DESCRIPTION
    The version lives in exactly one place - the `version` field of
    `[package]` in Cargo.toml - and everything downstream follows from it:

      * `build.rs` writes a VERSIONINFO resource into the executable using
        CARGO_PKG_VERSION, so Explorer's right-click -> Properties -> Details
        shows it for the installed xshot.exe;
      * `cargo packager` names the artifacts and stamps the NSIS installer
        with the same version.

    Because build.rs only re-runs when it has to, this script touches it after
    changing the version, and then does not take the outcome on trust: it reads
    the version resource back out of the built executable and fails if it does
    not match. That check is the point of the script - a setup whose exe reports
    the wrong version is worse than no setup.

.PARAMETER Version
    Version to package, e.g. 1.2.0 or 1.2.0-beta.1. Omit it to package whatever
    Cargo.toml already says.

.PARAMETER SkipTests
    Skip `cargo test --release`.

.EXAMPLE
    .\package.ps1 1.1.0
    .\package.ps1 1.1.0 -SkipTests
    .\package.ps1 1.1.0 -WhatIf      # only show what would change
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Position = 0)]
    [string]$Version,

    [switch]$SkipTests
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot -ErrorAction Stop

$manifestPath = Join-Path $PSScriptRoot 'Cargo.toml'
$exePath = Join-Path $PSScriptRoot 'target\release\xshot.exe'
$distPath = Join-Path $PSScriptRoot 'dist'

# The package version is the first `version = "..."` inside the `[package]`
# section. It cannot be found by line pattern alone: `[dependencies.windows]` has
# its own `version = "0.62"` at the start of a line too.
$versionPattern = '(?m)^version = "([^"]*)"'

function Get-PackageSection([string]$text) {
    $header = [regex]::Match($text, '(?m)^\[package\]\s*$')
    if (-not $header.Success) {
        throw 'Cargo.toml has no [package] section'
    }
    $start = $header.Index + $header.Length
    $next = [regex]::Match($text.Substring($start), '(?m)^\[')
    $end = if ($next.Success) { $start + $next.Index } else { $text.Length }
    return @{ Start = $start; End = $end }
}

function Get-ManifestVersion {
    $text = [System.IO.File]::ReadAllText($manifestPath)
    $section = Get-PackageSection $text
    $inside = $text.Substring($section.Start, $section.End - $section.Start)
    $matches = [regex]::Matches($inside, $versionPattern)
    if ($matches.Count -ne 1) {
        throw "expected exactly one version line in the [package] section, found $($matches.Count)"
    }
    return $matches[0].Groups[1].Value
}

function Set-ManifestVersion([string]$value) {
    $text = [System.IO.File]::ReadAllText($manifestPath)
    $section = Get-PackageSection $text
    $inside = $text.Substring($section.Start, $section.End - $section.Start)
    $updated = [regex]::Replace($inside, $versionPattern, "version = `"$value`"")
    $result = $text.Substring(0, $section.Start) + $updated + $text.Substring($section.End)
    # UTF-8 without a BOM, exactly like the file it replaces.
    [System.IO.File]::WriteAllText($manifestPath, $result, (New-Object System.Text.UTF8Encoding($false)))
}

function Assert-Success([string]$what) {
    if ($LASTEXITCODE -ne 0) {
        throw "$what failed with exit code $LASTEXITCODE"
    }
}

# Runs a native command whose stderr is not an error.
#
# Cargo writes its whole progress report to stderr, and with
# `$ErrorActionPreference = 'Stop'` PowerShell turns a native command's stderr
# into a *terminating* error: the script would die on the very run that has
# something to compile, while looking like the build failed. The exit code is the
# only thing worth trusting, so stderr is simply let through.
function Invoke-Tool {
    param([string]$What, [string[]]$Arguments)

    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & cargo @Arguments
    }
    finally {
        $ErrorActionPreference = $previous
    }
    Assert-Success $What
}

# -- 1. Which version are we packaging? --------------------------------------
$current = Get-ManifestVersion
if (-not $Version) {
    $Version = $current
    Write-Host "Cargo.toml already says $Version; packaging that." -ForegroundColor Cyan
}
elseif ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$') {
    throw "'$Version' is not a version this script understands (expected e.g. 1.2.0 or 1.2.0-beta.1)."
}

# -- 2. Put it in the one place everything else reads ------------------------
if ($Version -ne $current) {
    if ($PSCmdlet.ShouldProcess('Cargo.toml', "set the package version to $Version (was $current)")) {
        Set-ManifestVersion $Version
        Write-Host "Cargo.toml: $current -> $Version" -ForegroundColor Green
    }
}
else {
    Write-Host "Cargo.toml: already $Version"
}

if ($WhatIfPreference) {
    Write-Host ""
    Write-Host "WhatIf: stopping here. Without -WhatIf this would now run:" -ForegroundColor Yellow
    Write-Host "  cargo test --release"
    Write-Host "  cargo build --release"
    Write-Host "  check target\release\xshot.exe reports $Version"
    Write-Host "  cargo packager --release"
    return
}

# The build script renders the resource from CARGO_PKG_VERSION, and cargo
# re-runs it when that changes - but a touch costs nothing and removes the doubt.
(Get-Item (Join-Path $PSScriptRoot 'build.rs')).LastWriteTime = Get-Date

# -- 3. A running instance holds the executable open ------------------------
# `@(...)` because a single process has no `.Count`, and StrictMode says so.
$running = @(Get-Process xshot -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
    Write-Host "Stopping $($running.Count) running xshot process(es) so the build can replace the exe." -ForegroundColor Yellow
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 500
}

# -- 4. Tests, then the build ------------------------------------------------
if (-not $SkipTests) {
    Write-Host "`n=== cargo test --release ===" -ForegroundColor Cyan
    Invoke-Tool -What 'cargo test' -Arguments @('test', '--release')
    Write-Host "tests passed"
}

Write-Host "`n=== cargo build --release ===" -ForegroundColor Cyan
Invoke-Tool -What 'cargo build' -Arguments @('build', '--release')

# -- 5. The version has to be *in* the exe ----------------------------------
if (-not (Test-Path -LiteralPath $exePath)) {
    throw "no executable at $exePath after the build"
}
$info = (Get-Item -LiteralPath $exePath).VersionInfo
Write-Host "`n=== version resource of the built exe ===" -ForegroundColor Cyan
Write-Host ("  FileVersion    : {0}" -f $info.FileVersion)
Write-Host ("  ProductVersion : {0}" -f $info.ProductVersion)
Write-Host ("  ProductName    : {0}" -f $info.ProductName)
Write-Host ("  FileDescription: {0}" -f $info.FileDescription)
Write-Host ("  Copyright      : {0}" -f $info.LegalCopyright)

if ($info.FileVersion -ne $Version -or $info.ProductVersion -ne $Version) {
    throw ("the executable reports FileVersion='{0}' / ProductVersion='{1}' but {2} was asked for. " +
        "Windows' rc.exe is probably missing from PATH, so build.rs could not embed the resource - " +
        "install the Windows SDK (or run the build from a Developer Command Prompt) and try again.") -f `
        $info.FileVersion, $info.ProductVersion, $Version
}

# -- 6. Package --------------------------------------------------------------
Write-Host "`n=== cargo packager --release ===" -ForegroundColor Cyan
Invoke-Tool -What 'cargo packager' -Arguments @('packager', '--release')

# -- 7. Report ---------------------------------------------------------------
$setup = Get-ChildItem -LiteralPath $distPath -Filter "*$Version*-setup.exe" -ErrorAction SilentlyContinue
if (-not $setup) {
    $setup = Get-ChildItem -LiteralPath $distPath -Filter "*-setup.exe" |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
}
if (-not $setup) {
    throw "packaging finished but no setup executable was found in $distPath"
}

Write-Host "`n=== artifacts ===" -ForegroundColor Cyan
Get-ChildItem -LiteralPath $distPath -File |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 5 |
    ForEach-Object {
        Write-Host ("  {0,-32} {1,10:N0} bytes   {2}" -f $_.Name, $_.Length, $_.LastWriteTime)
    }

$setupInfo = (Get-Item -LiteralPath $setup.FullName).VersionInfo
Write-Host ""
Write-Host ("installer : {0}" -f $setup.FullName) -ForegroundColor Green
Write-Host ("version   : {0} (installer reports '{1}')" -f $Version, $setupInfo.ProductVersion)
Write-Host ("exe inside: reports FileVersion '{0}'" -f $info.FileVersion)
Write-Host ""
Write-Host "Run the setup to install; the installed xshot.exe reports this version in"
Write-Host "right-click -> Properties -> Details."
