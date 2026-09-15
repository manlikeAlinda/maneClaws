$ErrorActionPreference = "Stop"

$ProjectDir = (Resolve-Path -LiteralPath $PSScriptRoot).Path
Set-Location -LiteralPath $ProjectDir
$Exe = Join-Path $ProjectDir "target\release\binance_survival_bot.exe"
$Log = Join-Path $ProjectDir "task_bot_live.log"

$env:BOT_LOCK_PATH = (Join-Path $ProjectDir "bot.lock")

# ---- RESEARCH HOLD (2026-09-09) ----
# Every entry strategy in this bot has been backtested against 90-365 days of real
# BTCUSDT history and none has survived full re-simulation with a positive edge.
# Mean-reversion is structurally falsified (falling-knife entries, three flat filter
# candidates, still net negative after fixing the one real exit bug). Trend-breakout's
# best-looking filter candidate looked good in a static table and then failed under
# actual re-simulation. This project is a research effort until a strategy clears that
# bar (full re-simulation, not a static table) with a real, positive, reproducible edge.
# The Scheduled Task calling this script is still enabled (this session did not have
# permission to disable it — `Disable-ScheduledTask` and `schtasks /Change /DISABLE`
# both returned Access Denied) — disable/delete "Binance Survival Bot - 30min Bursts"
# in Task Scheduler (elevated) for a fully clean stop. This hard-exits before any
# trading activity either way, live or practice, so no capital or unattended activity
# is at risk in the meantime. Remove this block only once a strategy has cleared the
# re-simulation bar above.
$haltLog = Join-Path $ProjectDir "task_bot_live.log"
"`n--- $(Get-Date -Format s) RESEARCH HOLD: bot run skipped. See run_bot_30min.ps1 top-of-file comment. ---" | Out-File -Append -FilePath $haltLog
exit 0

try {
  # ---- deterministic bot base dir
  $env:BOT_BASE_DIR = $ProjectDir

  # ---- LIVE latch (REAL MONEY)
  # LIVE requires BOTH:
  #   BOT_LIVE_TRADING=1
  #   BOT_LIVE_CONFIRM=YES
  # Defense-in-depth only: the RESEARCH HOLD above already exits before this point is
  # ever reached. If that block is ever removed, this still defaults to PRACTICE rather
  # than arming LIVE, unlike the previous version of this script — arm LIVE explicitly
  # and deliberately, not as an unattended default, once a strategy clears re-simulation.
  if ([string]::IsNullOrWhiteSpace($env:BOT_LIVE_TRADING)) {
    $env:BOT_LIVE_TRADING = "0"
  }
  if ([string]::IsNullOrWhiteSpace($env:BOT_LIVE_CONFIRM)) {
    $env:BOT_LIVE_CONFIRM = ""
  }

  # ---- allow managing/selling existing BTC (riskier)
  # Enables LIVE takeover of ExternalInventory into a managed position.
  $env:BOT_ALLOW_EXTERNAL_INVENTORY = "1"

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
  $env:BINANCE_API_KEY = $k
  $env:BINANCE_API_SECRET = $s

  # ---- High-Intensity Loop Variables
  $env:BOT_LOOP = "1"
  $env:BOT_LOOP_SLEEP_SECS = "2"
  $env:BOT_LOOP_DURATION_SECS = "43200" # 12 hours

  "`n--- $(Get-Date -Format s) Task start (High-Intensity 12h) ---" | Out-File -Append -FilePath $Log
  "Exe: $Exe" | Out-File -Append -FilePath $Log
  "BOT_BASE_DIR=$($env:BOT_BASE_DIR)" | Out-File -Append -FilePath $Log
  "BOT_LOCK_PATH=$($env:BOT_LOCK_PATH)" | Out-File -Append -FilePath $Log
  "BOT_LIVE_TRADING=$($env:BOT_LIVE_TRADING)" | Out-File -Append -FilePath $Log
  "BOT_LIVE_CONFIRM=$($env:BOT_LIVE_CONFIRM)" | Out-File -Append -FilePath $Log
  "BOT_ALLOW_EXTERNAL_INVENTORY=$($env:BOT_ALLOW_EXTERNAL_INVENTORY)" | Out-File -Append -FilePath $Log
  "BOT_MAX_TRADE_NOTIONAL_USDT=$($env:BOT_MAX_TRADE_NOTIONAL_USDT)" | Out-File -Append -FilePath $Log
  "BOT_MAX_TRADE_NOTIONAL_FRACTION=$($env:BOT_MAX_TRADE_NOTIONAL_FRACTION)" | Out-File -Append -FilePath $Log
  "BOT_LOOP_DURATION_SECS=$($env:BOT_LOOP_DURATION_SECS)" | Out-File -Append -FilePath $Log
  "Key present: $([bool]$env:BINANCE_API_KEY) Secret present: $([bool]$env:BINANCE_API_SECRET)" | Out-File -Append -FilePath $Log

  if (!(Test-Path $Exe)) {
    "ERROR: exe not found: $Exe" | Out-File -Append -FilePath $Log
    exit 2
  }

  $end = (Get-Date).AddHours(12)

  while ((Get-Date) -lt $end) {
    try {
      & $Exe --loop *>> $Log
      if ($LASTEXITCODE -ne 0) {
        "Bot exit code: $LASTEXITCODE" | Out-File -Append -FilePath $Log
      }
    }
    catch {
      "Binance/network problem. We stop and try again later." | Out-File -Append -FilePath $Log
      "ERROR: $($_.Exception.Message)" | Out-File -Append -FilePath $Log
    }
    Start-Sleep -Seconds 10
  }

  "--- $(Get-Date -Format s) Task end ---" | Out-File -Append -FilePath $Log
}
catch {
  "--- $(Get-Date -Format s) Task failed ---" | Out-File -Append -FilePath $Log
  "ERROR: $($_.Exception.Message)" | Out-File -Append -FilePath $Log
  throw
}
finally {
  # Ensure we don't leave the scheduler in a different working directory.
  Set-Location -LiteralPath $ProjectDir
}


