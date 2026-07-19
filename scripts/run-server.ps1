<#
.SYNOPSIS
  Run the Project Dawn server with a per-run archived log and an auto-refreshed
  world_report.html on exit. PowerShell counterpart to scripts/dev-run.sh.

.DESCRIPTION
  Each run streams live to BOTH a unique logs\server_<timestamp>.log (never
  overwritten) AND server.log (the familiar name), so restarting the server no
  longer loses an earlier run's log. On Ctrl-C or the server exiting, it
  regenerates world_report.html via the admin_report bin.

  Logs are UTF-16 with ANSI colour codes (PowerShell's Tee-Object default). Ask
  Claude to clean one for review, or read the console directly.

.PARAMETER Dev
  Set PD_DEV_CMDS=1, turning dev/GM commands ON for EVERY connection (local solo
  iteration). Omit it for a hosted / GM-playtest run: dev commands OFF, so only
  is_gm accounts get tools (see scripts\..\, grant_gm bin).

.EXAMPLE
  .\scripts\run-server.ps1
  Dev commands OFF (hosted / GM-playtest mode).

.EXAMPLE
  .\scripts\run-server.ps1 -Dev
  Dev commands ON for everyone (PD_DEV_CMDS=1).
#>
param([switch]$Dev)

# cd to the repo root (parent of scripts\), wherever the script was invoked from.
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

if ($Dev) {
    $env:PD_DEV_CMDS = '1'
    Write-Host "PD_DEV_CMDS=1  ::  dev commands ON for EVERY connection (local solo)" -ForegroundColor Yellow
} else {
    Remove-Item Env:\PD_DEV_CMDS -ErrorAction Ignore
    Write-Host "PD_DEV_CMDS unset  ::  dev commands OFF; only is_gm accounts get tools" -ForegroundColor Green
}

# Unique per-run log so a restart cannot clobber an earlier run.
$logDir = Join-Path $repo 'logs'
if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Path $logDir | Out-Null }
$stampedLog = Join-Path $logDir ("server_{0}.log" -f (Get-Date -Format 'yyyy-MM-dd_HHmmss'))
$serverLog  = Join-Path $repo 'server.log'
Write-Host "Log: $stampedLog  (also mirrored live to server.log)" -ForegroundColor DarkGray
Write-Host ""

try {
    # *>&1 folds the server's stderr (where it logs) into the pipeline; the two
    # Tee-Objects write both files live and still pass output through to the
    # console.
    cargo run -p projectdawn-server *>&1 | Tee-Object -FilePath $stampedLog | Tee-Object -FilePath $serverLog
} finally {
    Write-Host ""
    Write-Host "Server stopped. Regenerating world_report.html ..." -ForegroundColor Cyan
    cargo run -p projectdawn-server --bin admin_report
}
