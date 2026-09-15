$log='task_bot_live.log'
if (!(Test-Path $log)) { Write-Host "Log file not found: $log"; exit 1 }
$lines = Get-Content $log -Tail 1000
$norm = $lines | ForEach-Object { ($_ -replace "\x1b\[[0-9;]*[A-Za-z]", "") -replace 'ΓÇö',' - ' -replace '—',' - ' -replace '–',' - ' }
$reasons = @()
foreach ($l in $norm) {
    if ($l -match 'Decision \((Live|Practice)\):') {
        $waitIndex = $l.IndexOf('WAIT')
        if ($waitIndex -ge 0) {
            $rest = $l.Substring($waitIndex + 4).Trim()
            # Remove non-ASCII garbled bytes and leading punctuation
            $rest = $rest -replace '[^\x00-\x7F]+',' '
            $rest = $rest -replace '^[\s\p{P}\-]+',''
            $reasons += $rest.Trim()
        }
    }
}
$counts = $reasons | Group-Object | Sort-Object Count -Descending
$header = "--- ONE-SHOT SUMMARY ($(Get-Date -Format T)) ---"
Add-Content -Path 'task_bot_analysis.log' -Value $header
Add-Content -Path 'task_bot_analysis.log' -Value 'Dominant Inhibitors:'
if ($counts.Count -eq 0) {
    Add-Content -Path 'task_bot_analysis.log' -Value 'Status: No Decisions found in recent lines.'
    Write-Host 'No Decisions found.'
} else {
    $counts | Select-Object -First 3 | ForEach-Object {
        $line = " - [$($_.Count)x] $($_.Name)"
        Add-Content -Path 'task_bot_analysis.log' -Value $line
        Write-Host $line
    }
}
