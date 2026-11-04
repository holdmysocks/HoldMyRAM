# Manual end-to-end test of the in-app updater WITHOUT GitHub (debug builds only).
#
# Usage (from the repo root, after `cargo build -p holdmyram -p holdmyram-helper`):
#   powershell -ExecutionPolicy Bypass -File scripts\test-update-local.ps1            # serve + launch a copy
#   powershell -ExecutionPolicy Bypass -File scripts\test-update-local.ps1 -ServeOnly # serve only (for the core test)
#
# What it does:
#   1. Makes a fake "newer" release from target\debug: HoldMyRAM-<version+1>-win-x64.zip
#      (same binaries, bumped name) and SHA256SUMS.txt, in a new temp folder.
#   2. Serves the release JSON and both files with System.Net.HttpListener on 127.0.0.1.
#   3. Copies the debug build into the temp folder and starts THAT copy with
#      HOLDMYRAM_UPDATE_API_URL pointing at the local server and APPDATA / LOCALAPPDATA /
#      TEMP redirected into the temp folder (config, journal, update cache, log, WebView2
#      data all land there).
#
# It never touches the repo's binaries (it copies them) or your real HoldMyRAM settings.
# Not redirected: the registry. The copy only writes it if you use Startup or
# "Start with Windows" in its window, so don't. The global hotkey is shared with any
# HoldMyRAM you already run: quit that one first. Do NOT toggle in the test copy: the
# toggle acts on real processes (default profile: Recommended).
# Only debug builds honour HOLDMYRAM_UPDATE_API_URL; a release build ignores it.

param(
    [switch]$ServeOnly,
    [int]$Port = 0
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
function Fail([string]$m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }

$debugDir = Join-Path $root 'target\debug'
foreach ($e in 'holdmyram.exe', 'holdmyram-helper.exe') {
    if (-not (Test-Path (Join-Path $debugDir $e))) { Fail "target\debug\$e not found. Run: cargo build -p holdmyram -p holdmyram-helper" }
}

# Version: crates\app\Cargo.toml, patch + 1.
$m = Select-String -Path (Join-Path $root 'crates\app\Cargo.toml') -Pattern '^\s*version\s*=\s*"(\d+)\.(\d+)\.(\d+)"' | Select-Object -First 1
if (-not $m) { Fail 'Could not read the version from crates\app\Cargo.toml' }
$g = $m.Matches[0].Groups
$current = "$($g[1].Value).$($g[2].Value).$($g[3].Value)"
$newer = "$($g[1].Value).$($g[2].Value).$([int]$g[3].Value + 1)"
$name = "HoldMyRAM-$newer-win-x64"

$work = Join-Path ([IO.Path]::GetTempPath()) ("holdmyram-update-test-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
$serve = Join-Path $work 'server'
$pkg = Join-Path $work "pkg\$name"
$app = Join-Path $work 'app'
$data = Join-Path $work 'data'
foreach ($d in $serve, $pkg, $app, "$data\Roaming", "$data\Local", "$data\Temp") { New-Item -ItemType Directory -Force $d | Out-Null }

# Fake newer release (same layout as scripts\package.ps1: one top folder).
Copy-Item (Join-Path $debugDir 'holdmyram.exe') $pkg
Copy-Item (Join-Path $debugDir 'holdmyram-helper.exe') $pkg
[IO.File]::WriteAllText((Join-Path $pkg 'README.txt'), "Fake HoldMyRAM $newer for the local update test.`r`n", (New-Object Text.UTF8Encoding $false))
$zip = Join-Path $serve "$name.zip"
Compress-Archive -Path $pkg -DestinationPath $zip
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
[IO.File]::WriteAllText((Join-Path $serve 'SHA256SUMS.txt'), "$hash  $name.zip`n", [Text.Encoding]::ASCII)

if ($Port -eq 0) {
    $l = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
    $l.Start(); $Port = $l.LocalEndpoint.Port; $l.Stop()
}
$base = "http://127.0.0.1:$Port"
$apiPath = '/repos/holdmysocks/HoldMyRAM/releases/latest'
$release = @{
    tag_name   = "v$newer"
    html_url   = "https://github.com/holdmysocks/HoldMyRAM/releases/tag/v$newer"
    body       = "Local test release $newer.`n`nThis is the current debug build with a bumped name."
    draft      = $false
    prerelease = $false
    assets     = @(
        @{ name = "$name.zip"; browser_download_url = "$base/download/v$newer/$name.zip"; size = (Get-Item $zip).Length },
        @{ name = 'SHA256SUMS.txt'; browser_download_url = "$base/download/v$newer/SHA256SUMS.txt"; size = (Get-Item (Join-Path $serve 'SHA256SUMS.txt')).Length }
    )
}
$json = $release | ConvertTo-Json -Depth 5
[IO.File]::WriteAllText((Join-Path $serve 'latest.json'), $json, (New-Object Text.UTF8Encoding $false))
$apiUrl = "$base$apiPath"

Write-Host ''
Write-Host "Running version: $current   fake release: $newer" -ForegroundColor Cyan
Write-Host "Work folder:     $work"
Write-Host "API URL:         $apiUrl"
Write-Host "Zip SHA-256:     $hash"

if (-not $ServeOnly) {
    $others = Get-Process -Name holdmyram -ErrorAction SilentlyContinue
    if ($others) { Write-Host 'WARNING: HoldMyRAM is already running; quit it first (shared hotkey and tray confusion).' -ForegroundColor Yellow }
    Copy-Item (Join-Path $debugDir 'holdmyram.exe') $app
    Copy-Item (Join-Path $debugDir 'holdmyram-helper.exe') $app
    # Children inherit these; this PowerShell session is the only other thing affected.
    $env:APPDATA = "$data\Roaming"
    $env:LOCALAPPDATA = "$data\Local"
    $env:TEMP = "$data\Temp"
    $env:TMP = "$data\Temp"
    $env:HOLDMYRAM_UPDATE_API_URL = $apiUrl
    $proc = Start-Process -FilePath (Join-Path $app 'holdmyram.exe') -WorkingDirectory $app -PassThru
    Write-Host "Started test copy: $app\holdmyram.exe (pid $($proc.Id))" -ForegroundColor Green
    Write-Host ''
    Write-Host 'What to check:' -ForegroundColor Cyan
    Write-Host "  1. Open the window from the (second) HoldMyRAM tray icon. Do not toggle."
    Write-Host "  2. Settings > Updates: within a few seconds of opening (or ~60 s after start)"
    Write-Host "     latest = $newer, available = true; phase goes downloading -> ready"
    Write-Host "     (auto-download is on by default). The requests show up below."
    Write-Host "  3. Click Install. The copy exits and a new process starts from $app."
    Write-Host "     Expected: update_status.updated_from = $current; holdmyram.exe.old and"
    Write-Host "     holdmyram-helper.exe.old are gone from $app after the restart;"
    Write-Host "     $data\Temp\holdmyram.log has 'update: updated from $current to $current ...'."
    Write-Host "     (The 'new' exe is the same build, so it offers $newer again. Expected.)"
    Write-Host "  4. Failure paths to try: edit $serve\SHA256SUMS.txt (bad_checksum), delete it"
    Write-Host "     (not_found), or stop this script (offline) and press Check now."
    Write-Host "  Update cache: $data\Local\HoldMyRAM\update.json   staged: $data\Local\HoldMyRAM\updates\$newer"
} else {
    Write-Host ''
    Write-Host 'Serve-only. In another terminal:' -ForegroundColor Cyan
    Write-Host "  `$env:HOLDMYRAM_UPDATE_API_URL = '$apiUrl'"
    Write-Host '  cargo test -p holdmyram-core update_local_server -- --ignored --nocapture'
}

$listener = New-Object Net.HttpListener
$listener.Prefixes.Add("$base/")
$listener.Start()
Write-Host ''
Write-Host "Serving on $base (Ctrl+C to stop). The work folder is left for inspection; delete it when done." -ForegroundColor Cyan
$files = @{
    $apiPath                                   = @{ Path = (Join-Path $serve 'latest.json'); Type = 'application/json' }
    "/download/v$newer/$name.zip"              = @{ Path = $zip; Type = 'application/zip' }
    "/download/v$newer/SHA256SUMS.txt"         = @{ Path = (Join-Path $serve 'SHA256SUMS.txt'); Type = 'text/plain' }
}
try {
    while ($listener.IsListening) {
        $ctx = $listener.GetContext()
        $path = $ctx.Request.Url.AbsolutePath
        $entry = $files[$path]
        if ($entry -and (Test-Path $entry.Path)) {
            $bytes = [IO.File]::ReadAllBytes($entry.Path)
            $ctx.Response.StatusCode = 200
            $ctx.Response.ContentType = $entry.Type
            $ctx.Response.ContentLength64 = $bytes.Length
            $ctx.Response.OutputStream.Write($bytes, 0, $bytes.Length)
        } else {
            $ctx.Response.StatusCode = 404
        }
        Write-Host ("{0:HH:mm:ss} {1} {2} -> {3}" -f (Get-Date), $ctx.Request.HttpMethod, $path, $ctx.Response.StatusCode)
        $ctx.Response.Close()
    }
} finally {
    $listener.Stop()
}
