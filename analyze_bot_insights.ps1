# analyze_bot_insights.ps1
# Real-time analyzer for binance_survival_bot
# Monitors task_bot_live.log and provides heuristic-based refinement advice.

$LogFile = "task_bot_live.log"
$WaitReasons = @{}
$PriceHistory = @()
$LastSummaryTime = Get-Date

# File to store analyzer summaries
$AnalysisLog = "task_bot_analysis.log"
# Write an initial header so file starts with a timestamped session marker
Add-Content -Path $AnalysisLog -Value "`n===================================================="
Add-Content -Path $AnalysisLog -Value "      BOT INSIGHT ANALYZER START ($(Get-Date -Format u))      "
Add-Content -Path $AnalysisLog -Value "====================================================`n"

if (!(Test-Path $LogFile)) {
    Write-Host "Error: $LogFile not found. Start the bot first." -ForegroundColor Red
    exit
}

Write-Host "`n====================================================" -ForegroundColor Cyan
Write-Host "      BOT INSIGHT ANALYZER (HIGH-INTENSITY)       " -ForegroundColor Cyan
Write-Host "====================================================`n" -ForegroundColor Cyan
Write-Host "Monitoring logs... (Summary every 2 minutes)`n"

Get-Content $LogFile -Tail 0 -Wait | ForEach-Object {
    $Line = $_
    # Strip common ANSI escape sequences from colored log lines
    $cleanLine = $Line -replace "\x1b\[[0-9;]*[A-Za-z]", ""

    # 1. Parse BTC Price
    if ($Line -match "BTC price: ([\d.]+) USDT") {
        $Price = [double]$Matches[1]
        $PriceHistory += [PSCustomObject]@{Time = (Get-Date); Price = $Price }
        # Keep 5 minutes of price history (at 2s heartbeats, that's ~150 points)
        if ($PriceHistory.Count -gt 150) { $PriceHistory = $PriceHistory[-150..-1] }
    }

    # 2. Parse Wait Reasons
    # Matches: Decision (Live|Practice): WAIT - <Reason>
    # Normalize common garbled/em-dash encodings to ASCII hyphen for robust matching
    $normalizedLine = $cleanLine -replace 'ΓÇö',' - ' -replace '—',' - ' -replace '–',' - '
    if ($normalizedLine -match 'Decision \((Live|Practice)\):') {
        $waitIndex = $normalizedLine.IndexOf('WAIT')
        if ($waitIndex -ge 0) {
            $rest = $normalizedLine.Substring($waitIndex + 4).Trim()
            # Remove non-ASCII garbled bytes and leading punctuation/whitespace
            $rest = $rest -replace '[^\x00-\x7F]+',' '
            $rest = $rest -replace '^[\s\p{P}\-]+',''
            $Reason = $rest.Trim()
            if ($WaitReasons.ContainsKey($Reason)) {
                $WaitReasons[$Reason]++
            }
            else {
                $WaitReasons[$Reason] = 1
            }
        }
    }

    # 3. Detect Technical Errors
    if ($Line -match "rejected the key") {
        Write-Host "CRITICAL: API Key rejected. Check your .env or Binance settings." -ForegroundColor Red
    }
    if ($Line -match "Failed fetching") {
        Write-Host "WARNING: Data fetch failure detected." -ForegroundColor Yellow
    }

    # 4. Periodic Insight Generation (Every 2 minutes)
    if ((Get-Date).Subtract($LastSummaryTime).TotalMinutes -ge 2) {
        Write-Host "--- INSIGHT SUMMARY ($(Get-Date -Format T)) ---" -ForegroundColor Yellow
        Add-Content -Path $AnalysisLog -Value "--- INSIGHT SUMMARY ($(Get-Date -Format T)) ---"
        
        # Display Top Wait Reasons
        Write-Host "Dominant Inhibitors:"
        Add-Content -Path $AnalysisLog -Value "Dominant Inhibitors:"
        $WaitReasons.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First 3 | ForEach-Object {
            $line = " - [$($_.Value)x] $($_.Key)"
            Write-Host $line
            Add-Content -Path $AnalysisLog -Value $line
        }

        # Strategic Advice
        if ($PriceHistory.Count -gt 30) {
            $StartPrice = $PriceHistory[0].Price
            $EndPrice = $PriceHistory[-1].Price
            $PriceDiff = $EndPrice - $StartPrice
            
            # Recommendation: Velocity too strict?
            if ($PriceDiff -gt 50 -and $WaitReasons.ContainsKey("Not enough upward velocity")) {
                $ad1 = "[ADVICE] BTC rose +$($PriceDiff) USDT in 2min, but bot is consistently 'waiting for velocity'."
                $ad2 = "         -> Suggestion: Consider lowering velocity_1m threshold in signals.rs."
                Write-Host $ad1 -ForegroundColor Cyan
                Write-Host $ad2 -ForegroundColor Cyan
                Add-Content -Path $AnalysisLog -Value $ad1
                Add-Content -Path $AnalysisLog -Value $ad2
            }

            # Recommendation: RSI too conservative?
            if ($PriceDiff -gt 20 -and $WaitReasons.ContainsKey("RSI out of range (55-80)")) {
                $ad3 = "[ADVICE] Strong price action, but RSI is blocking entry."
                $ad4 = "         -> Suggestion: Expand RSI range or check if 1m RSI is lagging."
                Write-Host $ad3 -ForegroundColor Cyan
                Write-Host $ad4 -ForegroundColor Cyan
                Add-Content -Path $AnalysisLog -Value $ad3
                Add-Content -Path $AnalysisLog -Value $ad4
            }
        }

        # Health Check
        if ($WaitReasons.Count -eq 0) {
            $status = "Status: Bot seems to be in a transition state (no Decisions logged recently)."
            Write-Host $status -ForegroundColor Gray
            Add-Content -Path $AnalysisLog -Value $status
        }
        
        Add-Content -Path $AnalysisLog -Value "------------------------------`n"
        $LastSummaryTime = Get-Date
        $WaitReasons.Clear() # Fresh stats for next window
    }
}
