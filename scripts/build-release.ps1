<#
.SYNOPSIS
    Builds a QuickByte release: the three executables, the installer, and the
    update manifest.

.DESCRIPTION
    1. cargo build --release for the whole workspace (static CRT, LTO).
    2. Stages the binaries under their shipped names in rust\dist\stage:
         QuickByte.exe, QuickByte.Agent.exe (built as quickbyte-agent.exe), qb.exe
    3. Compiles installer\QuickByte.iss with Inno Setup 6 -> installer\Output\QuickByte-<version>-Setup.exe
    4. Writes latest.json beside it (and, with -PublishToSite, copies both into
       site\releases\ so deploying the website publishes the update).

    The version is read from [workspace.package] in rust\Cargo.toml: the single
    source of truth. QB_VERSION_SUFFIX / QB_BUILD_REVISION are honoured by the
    build scripts for pre-release labels and build numbers.

.EXAMPLE
    ./scripts/build-release.ps1 -ReleaseNotes "Rewritten in Rust: faster, lighter, new interface."

.EXAMPLE
    ./scripts/build-release.ps1 -PublishToSite
#>
[CmdletBinding()]
param(
    [string] $ReleaseNotes = "",
    [switch] $PublishToSite,
    [switch] $SkipBuild,
    [string] $BaseDownloadUrl = "https://quickbyte-app.pages.dev/releases"
)

$ErrorActionPreference = 'Stop'
$rust = Split-Path -Parent $PSScriptRoot
$repo = Split-Path -Parent $rust

# --- version ---------------------------------------------------------------
$manifest = Get-Content (Join-Path $rust 'Cargo.toml') -Raw
if ($manifest -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    throw "Could not read [workspace.package] version from rust\Cargo.toml"
}
$version = $Matches[1]
if ($env:QB_VERSION_SUFFIX) { $version = "$version-$($env:QB_VERSION_SUFFIX.Trim())" }
Write-Host "QuickByte $version" -ForegroundColor Cyan

# --- build -----------------------------------------------------------------
Push-Location $rust
try {
    if (-not $SkipBuild) {
        # cargo reports progress on stderr; under Windows PowerShell 5.1 with
        # ErrorActionPreference=Stop that alone would abort the script, so judge
        # the build by its exit code instead.
        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        cargo build --release --workspace 2>&1 | ForEach-Object { "$_" }
        $ErrorActionPreference = $previous
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    }
} finally { Pop-Location }

$release = Join-Path $rust 'target\release'
$stage = Join-Path $rust 'dist\stage'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null

$shipped = @{
    'QuickByte.exe'       = 'QuickByte.exe'
    'quickbyte-agent.exe' = 'QuickByte.Agent.exe'
    'qb.exe'              = 'qb.exe'
}
foreach ($built in $shipped.Keys) {
    $source = Join-Path $release $built
    if (-not (Test-Path $source)) { throw "Missing build output: $source" }
    Copy-Item $source (Join-Path $stage $shipped[$built])
}
Get-ChildItem $stage | ForEach-Object { Write-Host ("  {0,-22} {1,8:N0} KB" -f $_.Name, ($_.Length / 1KB)) }

# --- installer -------------------------------------------------------------
$iscc = @(
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 (ISCC.exe) was not found." }

$installerDir = Join-Path $rust 'installer'
$output = Join-Path $installerDir 'Output'
& $iscc "/DAppVersion=$version" "/DSourceDir=$stage" "/DOutputDir=$output" (Join-Path $installerDir 'QuickByte.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }

$setupName = "QuickByte-$version-Setup.exe"
$setup = Join-Path $output $setupName
$hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLowerInvariant()
$size = (Get-Item $setup).Length

# --- update manifest (same shape the C# UpdateService reads) ----------------
$latest = [ordered]@{
    version       = $version
    downloadUrl   = "$BaseDownloadUrl/$setupName"
    releaseNotes  = $ReleaseNotes
    releaseDate   = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    fileSizeBytes = $size
    sha256        = $hash
}
$latestPath = Join-Path $output 'latest.json'
# UTF-8 without a BOM (Windows PowerShell 5.1 has no utf8NoBOM encoding).
[System.IO.File]::WriteAllText($latestPath, ($latest | ConvertTo-Json), (New-Object System.Text.UTF8Encoding $false))

Write-Host ""
Write-Host "Installer: $setup ($([math]::Round($size / 1MB, 2)) MB)" -ForegroundColor Green
Write-Host "SHA-256:   $hash"
Write-Host "Manifest:  $latestPath"

if ($PublishToSite) {
    $siteReleases = Join-Path $repo 'site\releases'
    New-Item -ItemType Directory -Force $siteReleases | Out-Null
    Copy-Item $setup $siteReleases -Force
    Copy-Item $latestPath $siteReleases -Force
    Write-Host "Copied to $siteReleases (deploy the site to publish the update)." -ForegroundColor Green
}
