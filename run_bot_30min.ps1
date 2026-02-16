$ErrorActionPreference = "Stop"

$ProjectDir = (Resolve-Path -LiteralPath $PSScriptRoot).Path
Set-Location -LiteralPath $ProjectDir
$Exe        = Join-Path $ProjectDir "target\release\binance_survival_bot.exe"
$Log        = Join-Path $ProjectDir "task_bot.log"

$env:BOT_LOCK_PATH = (Join-Path $ProjectDir "bot.lock")

try {
  # ---- deterministic bot base dir
  $env:BOT_BASE_DIR = $ProjectDir

  # ---- LIVE latch (safe default)
  # LIVE requires BOTH:
  #   BOT_LIVE_TRADING=1
  #   BOT_LIVE_CONFIRM=YES
  if ([string]::IsNullOrWhiteSpace($env:BOT_LIVE_TRADING)) {
    $env:BOT_LIVE_TRADING = "0"
  }

  # ---- keep log file clean (no ANSI color codes)
  $env:NO_COLOR = "1"
  $env:RUST_LOG_STYLE = "never"

  # ---- Task Scheduler often does NOT inherit your User env vars.
  $k = [Environment]::GetEnvironmentVariable("BINANCE_API_KEY", "User")
  $s = [Environment]::GetEnvironmentVariable("BINANCE_API_SECRET", "User")
  if ([string]::IsNullOrWhiteSpace($k) -or [string]::IsNullOrWhiteSpace($s)) {
    $k = [Environment]::GetEnvironmentVariable("BINANCE_API_KEY", "Machine")
    $s = [Environment]::GetEnvironmentVariable("BINANCE_API_SECRET", "Machine")
  }
  $env:BINANCE_API_KEY    = $k
  $env:BINANCE_API_SECRET = $s

  "`n--- $(Get-Date -Format s) Task start ---" | Out-File -Append -FilePath $Log
  "Exe: $Exe" | Out-File -Append -FilePath $Log
  "BOT_BASE_DIR=$($env:BOT_BASE_DIR)" | Out-File -Append -FilePath $Log
  "BOT_LOCK_PATH=$($env:BOT_LOCK_PATH)" | Out-File -Append -FilePath $Log
  "BOT_LIVE_TRADING=$($env:BOT_LIVE_TRADING)" | Out-File -Append -FilePath $Log
  "BOT_LIVE_CONFIRM=$($env:BOT_LIVE_CONFIRM)" | Out-File -Append -FilePath $Log
  "Key present: $([bool]$env:BINANCE_API_KEY) Secret present: $([bool]$env:BINANCE_API_SECRET)" | Out-File -Append -FilePath $Log

  if (!(Test-Path $Exe)) {
    "ERROR: exe not found: $Exe" | Out-File -Append -FilePath $Log
    exit 2
  }

  $end = (Get-Date).AddMinutes(60)

  while ((Get-Date) -lt $end) {
    try {
      & $Exe *>> $Log
      if ($LASTEXITCODE -ne 0) {
        "Bot exit code: $LASTEXITCODE" | Out-File -Append -FilePath $Log
      }
    } catch {
      "Binance/network problem. We stop and try again later." | Out-File -Append -FilePath $Log
      "ERROR: $($_.Exception.Message)" | Out-File -Append -FilePath $Log
    }
    Start-Sleep -Seconds 300
  }

  "--- $(Get-Date -Format s) Task end ---" | Out-File -Append -FilePath $Log


