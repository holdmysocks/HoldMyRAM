# Measures the tray process before and after the scheduled update check.
#
# Usage (PowerShell 7 or 64-bit Windows PowerShell, from the repo root):
#   powershell -ExecutionPolicy Bypass -File scripts\measure-idle.ps1                 # release build
#   powershell -ExecutionPolicy Bypass -File scripts\measure-idle.ps1 -Debug          # debug build, check after 5 s
#
# It runs a COPY of the exe from a temp folder with APPDATA, LOCALAPPDATA and TEMP
# redirected there (fresh config, journal, update cache and log), so it never uses your
# settings and never touches the repo's binaries. No window is opened and nothing is
# toggled; the copy is stopped at the end and the temp folder removed. The release build
# checks GitHub once ~60 s after start (private repo today: "not found"). Quit any running
# HoldMyRAM first (shared hotkey).

param(
    [switch]$Debug,
    [int[]]$After = @(5, 30, 90)
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$src = Join-Path $root ($(if ($Debug) { 'target\debug' } else { 'target\release' }))
foreach ($e in 'holdmyram.exe', 'holdmyram-helper.exe') {
    if (-not (Test-Path (Join-Path $src $e))) { Write-Host "ERROR: $src\$e not found; build first." -ForegroundColor Red; exit 1 }
}
if (Get-Process -Name holdmyram -ErrorAction SilentlyContinue) { Write-Host 'ERROR: HoldMyRAM is running; quit it first.' -ForegroundColor Red; exit 1 }

$work = Join-Path ([IO.Path]::GetTempPath()) ("holdmyram-measure-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
$app = Join-Path $work 'app'
foreach ($d in $app, "$work\Roaming", "$work\Local", "$work\Temp") { New-Item -ItemType Directory -Force $d | Out-Null }
Copy-Item (Join-Path $src 'holdmyram.exe'), (Join-Path $src 'holdmyram-helper.exe') $app
$log = Join-Path $work 'Temp\holdmyram.log'

$saved = @{ APPDATA = $env:APPDATA; LOCALAPPDATA = $env:LOCALAPPDATA; TEMP = $env:TEMP; TMP = $env:TMP; D = $env:HOLDMYRAM_FIRST_CHECK_DELAY_SECS }
$env:APPDATA = "$work\Roaming"; $env:LOCALAPPDATA = "$work\Local"; $env:TEMP = "$work\Temp"; $env:TMP = "$work\Temp"
# Honoured by debug builds only (release always waits ~60 s).
if ($Debug) { $env:HOLDMYRAM_FIRST_CHECK_DELAY_SECS = '5' }
$proc = Start-Process -FilePath (Join-Path $app 'holdmyram.exe') -WorkingDirectory $app -PassThru
$env:APPDATA = $saved.APPDATA; $env:LOCALAPPDATA = $saved.LOCALAPPDATA; $env:TEMP = $saved.TEMP; $env:TMP = $saved.TMP; $env:HOLDMYRAM_FIRST_CHECK_DELAY_SECS = $saved.D

function Sample([string]$label) {
    $p = Get-Process -Id $proc.Id
    $mods = @($p.Modules | ForEach-Object { $_.ModuleName.ToLower() })
    [pscustomobject]@{
        When       = $label
        WorkingMB  = [math]::Round($p.WorkingSet64 / 1MB, 1)
        PrivateMB  = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
        Threads    = $p.Threads.Count
        Modules    = $mods.Count
        WinHttp    = $mods -contains 'winhttp.dll'
        ModuleList = $mods
    }
}

$rows = @()
try {
    Start-Sleep -Seconds 8
    $before = Sample 't+8s (before check)'
    $rows += $before
    $deadline = (Get-Date).AddSeconds(150)
    while (-not ((Test-Path $log) -and (Select-String -Path $log -Pattern 'update: check' -Quiet))) {
        if ((Get-Date) -gt $deadline) { throw 'no update check within 150 s' }
        Start-Sleep -Milliseconds 500
    }
    $finished = Get-Date
    $elapsed = 0
    foreach ($s in $After) {
        Start-Sleep -Seconds ($s - $elapsed); $elapsed = $s
        $rows += Sample "check done + ${s}s"
    }
    $rows | Format-Table When, WorkingMB, PrivateMB, Threads, Modules, WinHttp -AutoSize | Out-String | Write-Host
    $last = $rows[-1]
    $added = $last.ModuleList | Where-Object { $before.ModuleList -notcontains $_ }
    Write-Host ("Modules still loaded that were not there before the check: " + ($(if ($added) { $added -join ', ' } else { '(none)' })))
    Write-Host 'Log:'; Get-Content $log | Where-Object { $_ -match 'update' } | ForEach-Object { "  $_" }
} finally {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
