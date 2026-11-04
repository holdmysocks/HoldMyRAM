# Builds a release and stages a drop-in Windows package.
# Usage: powershell -ExecutionPolicy Bypass -File scripts\package.ps1
# Output: distHoldMyRAM-<version>-win-x64, the .zip, and distSHA256SUMS.txt
# Does not touch services, startup entries or user config, and never starts the helper.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Fail([string]$m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }

# (a) version
$m = Select-String -Path (Join-Path $root 'crates\app\Cargo.toml') -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m) { Fail 'Could not read version from crates\app\Cargo.toml' }
$version = $m.Matches[0].Groups[1].Value
$name = "HoldMyRAM-$version-win-x64"
Write-Host "HoldMyRAM $version"

# (b) cargo
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoBin) { $env:PATH = "$cargoBin;$env:PATH" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail 'cargo not found. Install Rust from https://rustup.rs (default MSVC toolchain; Visual Studio Build Tools are required), then reopen the terminal.'
}
$targetDir = Join-Path $root 'target\release'
foreach ($p in 'holdmyram', 'holdmyram-helper') {
    $running = Get-Process -Name $p -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($targetDir, [StringComparison]::OrdinalIgnoreCase) }
    if ($running) { Fail "$p.exe is running from target\release. Quit it first (it locks the file), then rerun." }
}

function Invoke-Native([string]$exe, [string[]]$arguments) {
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    & $exe @arguments
    $code = $LASTEXITCODE
    $ErrorActionPreference = $old
    if ($code -ne 0) { Fail "$exe $($arguments -join ' ') failed with exit code $code" }
}

Write-Host 'Building release...'
Invoke-Native 'cargo' @('build', '--release', '-p', 'holdmyram', '-p', 'holdmyram-helper')

foreach ($e in 'holdmyram.exe', 'holdmyram-helper.exe') {
    if (-not (Test-Path (Join-Path $targetDir $e))) { Fail "Build did not produce $e" }
}

# (c) stage
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist $name
$zip = Join-Path $dist "$name.zip"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
if (Test-Path $zip) { Remove-Item -Force $zip }
$sums = Join-Path $dist "SHA256SUMS.txt"
if (Test-Path $sums) { Remove-Item -Force $sums }
if (Test-Path "$zip.sha256") { Remove-Item -Force "$zip.sha256" }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item (Join-Path $targetDir 'holdmyram.exe') $stage
Copy-Item (Join-Path $targetDir 'holdmyram-helper.exe') $stage
$utf8 = New-Object System.Text.UTF8Encoding $false
foreach ($t in 'README.txt', 'Start here.txt') {
    $txt = (Get-Content -Raw (Join-Path $PSScriptRoot "templates\$t")).Replace('{{VERSION}}', $version)
    $txt = $txt -replace "`r?`n", "`r`n"
    [System.IO.File]::WriteAllText((Join-Path $stage $t), $txt, $utf8)
}

# LICENSE notice for the project itself
$own = "HoldMyRAM $version`r`nAll rights reserved by the author; not yet licensed for redistribution.`r`nThird-party components and their licences: see THIRD-PARTY-NOTICES.txt.`r`n"
[System.IO.File]::WriteAllText((Join-Path $stage 'LICENSE.txt'), $own, $utf8)

# Third-party notices from cargo metadata (normal deps only, Windows target)
Write-Host 'Generating THIRD-PARTY-NOTICES.txt...'
$oldEap = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
$metaJson = (& cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc 2>$null) -join "`n"
$code = $LASTEXITCODE
$ErrorActionPreference = $oldEap
if ($code -ne 0) { Fail 'cargo metadata failed' }
$meta = ConvertFrom-Json $metaJson
$pkgById = @{}
foreach ($p in $meta.packages) { $pkgById[$p.id] = $p }
$nodeById = @{}
foreach ($n in $meta.resolve.nodes) { $nodeById[$n.id] = $n }
$starts = $meta.packages | Where-Object { $_.name -in 'holdmyram', 'holdmyram-helper' } | ForEach-Object { $_.id }
$seen = New-Object 'System.Collections.Generic.HashSet[string]'
$queue = New-Object 'System.Collections.Generic.Queue[string]'
foreach ($s in $starts) { [void]$seen.Add($s); $queue.Enqueue($s) }
while ($queue.Count -gt 0) {
    $id = $queue.Dequeue()
    foreach ($d in $nodeById[$id].deps) {
        $normal = $d.dep_kinds | Where-Object { -not $_.kind }
        if ($normal -and $seen.Add($d.pkg)) { $queue.Enqueue($d.pkg) }
    }
}
$ours = @('holdmyram', 'holdmyram-helper', 'holdmyram-core')
$deps = $seen | ForEach-Object { $pkgById[$_] } | Where-Object { $_.name -notin $ours } | Sort-Object name, version

$nl = "`r`n"
$sb = New-Object System.Text.StringBuilder
[void]$sb.Append("THIRD-PARTY NOTICES - HoldMyRAM $version$nl")
[void]$sb.Append("========================================$nl$nl")
[void]$sb.Append("HoldMyRAM itself: All rights reserved by the author; not yet licensed for redistribution.$nl$nl")
[void]$sb.Append("HoldMyRAM includes the open-source components below (runtime dependency graph of$nl")
[void]$sb.Append("holdmyram.exe and holdmyram-helper.exe on x86_64 Windows; dev dependencies excluded).$nl$nl")
[void]$sb.Append("Vendored component: muda 0.20.0 (Apache-2.0 OR MIT) is bundled in vendor/muda with one$nl")
[void]$sb.Append("patched line (TPM_BOTTOMALIGN added to the Windows tray menu popup so the menu opens$nl")
[void]$sb.Append("above a bottom taskbar). Its licence files are Apache-2.0 and MIT as listed below.$nl$nl")
[void]$sb.Append("Components ($($deps.Count))$nl-----------------$nl")
foreach ($d in $deps) {
    $lic = if ($d.license) { $d.license } elseif ($d.license_file) { "see $($d.license_file)" } else { 'UNSPECIFIED' }
    $repo = if ($d.repository) { $d.repository } else { '-' }
    [void]$sb.Append("$($d.name) $($d.version) | $lic | $repo$nl")
}
[void]$sb.Append("$nl$nl")
[void]$sb.Append("Licence texts$nl=============$nl")
[void]$sb.Append("Identical texts are printed once, followed by the crates that ship them.$nl")
[void]$sb.Append("Crates with no licence file in their source are covered by the SPDX identifier above.$nl$nl")
$byHash = [ordered]@{}
$noText = @()
$sha = [System.Security.Cryptography.SHA256]::Create()
foreach ($d in $deps) {
    $dir = Split-Path -Parent $d.manifest_path
    $files = @()
    if (Test-Path $dir) {
        $files = Get-ChildItem -LiteralPath $dir -File -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE)' -and $_.Length -lt 200KB }
    }
    if (-not $files) { $noText += "$($d.name) $($d.version) [$($d.license)]"; continue }
    foreach ($f in $files) {
        $text = (Get-Content -Raw -LiteralPath $f.FullName).Trim() -replace "`r?`n", $nl
        $h = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($text)))
        if (-not $byHash.Contains($h)) { $byHash[$h] = @{ Text = $text; Crates = New-Object System.Collections.ArrayList; File = $f.Name } }
        [void]$byHash[$h].Crates.Add("$($d.name) $($d.version)")
    }
}
foreach ($h in $byHash.Keys) {
    $e = $byHash[$h]
    [void]$sb.Append("----------------------------------------------------------------$nl")
    [void]$sb.Append("Applies to: $((($e.Crates | Select-Object -Unique) -join ', '))$nl")
    [void]$sb.Append("File: $($e.File)$nl$nl$($e.Text)$nl$nl")
}
if ($noText) {
    [void]$sb.Append("----------------------------------------------------------------$nl")
    [void]$sb.Append("No licence file found in source; SPDX identifier only:$nl")
    foreach ($n in $noText) { [void]$sb.Append("  $n$nl") }
}
[System.IO.File]::WriteAllText((Join-Path $stage 'THIRD-PARTY-NOTICES.txt'), $sb.ToString(), $utf8)

# (d) zip + hash
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
# SHA256SUMS.txt in the form "<64 hex>  <filename>" (two spaces), ASCII, LF, as the updater expects.
[System.IO.File]::WriteAllText($sums, "$hash  $name.zip`n", [System.Text.Encoding]::ASCII)

# (e) verify
Add-Type -AssemblyName System.IO.Compression.FileSystem
$errors = @()
foreach ($e in 'holdmyram.exe', 'holdmyram-helper.exe', 'README.txt', 'Start here.txt', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt') {
    $f = Join-Path $stage $e
    if (-not (Test-Path $f) -or (Get-Item $f).Length -le 0) { $errors += "missing or empty: $e" }
}
$za = [System.IO.Compression.ZipFile]::OpenRead($zip)
try {
    $entries = $za.Entries | ForEach-Object { $_.FullName }
    foreach ($e in 'holdmyram.exe', 'holdmyram-helper.exe') {
        if (-not ($entries | Where-Object { $_ -match "^$([regex]::Escape($name))[\\/]$([regex]::Escape($e))$" })) { $errors += "zip lacks $e" }
    }
    $count = $za.Entries.Count
} finally { $za.Dispose() }
if ($errors) { Fail ($errors -join '; ') }

Write-Host ''
Write-Host 'Package OK' -ForegroundColor Green
Get-ChildItem $stage | ForEach-Object { '  {0,-28} {1,10:N0} bytes' -f $_.Name, $_.Length }
Write-Host "Notices:  $($deps.Count) components, $($byHash.Count) distinct licence texts, $($noText.Count) SPDX-only"
Write-Host "Folder:   $stage"
Write-Host "Zip:      $zip ($('{0:N0}' -f (Get-Item $zip).Length) bytes, $count entries)"
Write-Host "SHA256:   $hash"
Write-Host "Sums:     $sums"
Write-Host ("  " + (Get-Content -Raw $sums).TrimEnd())
