# Prepares (and optionally creates) a GitHub release for the package built by package.ps1.
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1             # prints the command only
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Publish    # checks, asks, then publishes
param([switch]$Publish)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Fail([string]$m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }

$m = Select-String -Path (Join-Path $root 'crates\app\Cargo.toml') -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m) { Fail 'Could not read version from crates\app\Cargo.toml' }
$version = $m.Matches[0].Groups[1].Value
$tag = "v$version"
$zip = Join-Path $root "dist\HoldMyRAM-$version-win-x64.zip"
$sums = Join-Path $root 'dist\SHA256SUMS.txt'
foreach ($f in $zip, $sums) { if (-not (Test-Path $f)) { Fail "Missing $f. Run scripts\package.ps1 first." } }

# The sums file must match the zip.
$expect = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
if (-not ((Get-Content -Raw $sums) -match "(?m)^$expect  HoldMyRAM-$([regex]::Escape($version))-win-x64\.zip\s*$")) {
    Fail 'SHA256SUMS.txt does not match the zip. Rerun scripts\package.ps1.'
}

$title = "HoldMyRAM $version"
$notes = "HoldMyRAM $version for Windows 10/11 x64. Unzip, keep both exes together, run holdmyram.exe. Unsigned: SmartScreen shows More info, Run anyway. Verify the zip against SHA256SUMS.txt."
$cmd = "gh release create $tag `"$zip`" `"$sums`" --title `"$title`" --notes `"$notes`""

if (-not $Publish) {
    Write-Host "Nothing was published. To create the release, review and run:`n"
    Write-Host $cmd
    Write-Host "`nOr rerun this script with -Publish for checks and a confirmation prompt."
    exit 0
}

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { Fail 'gh (GitHub CLI) not found. Install from https://cli.github.com' }
$old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
& gh auth status *> $null
$authOk = ($LASTEXITCODE -eq 0)
$dirty = (& git status --porcelain) -join "`n"
$localTag = (& git tag --list $tag) -join ''
$remoteTag = (& gh release view $tag 2>$null | Out-String)
$remoteCode = $LASTEXITCODE
$ErrorActionPreference = $old
if (-not $authOk) { Fail 'gh is not authenticated. Run: gh auth login' }
if ($dirty) { Fail "Working tree has uncommitted changes; commit or stash first:`n$dirty" }
if ($localTag) { Fail "Tag $tag already exists locally." }
if ($remoteCode -eq 0) { Fail "A release for $tag already exists on GitHub." }

Write-Host "About to create GitHub release $tag with:`n  $zip`n  $sums"
$answer = Read-Host "Type the tag ($tag) to publish, anything else cancels"
if ($answer -ne $tag) { Write-Host 'Cancelled; nothing published.'; exit 0 }
Invoke-Expression $cmd
if ($LASTEXITCODE -ne 0) { Fail 'gh release create failed.' }
