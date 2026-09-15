$lines = Get-Content task_bot_live.log -Tail 200
$i = 0
foreach ($l in $lines) {
    $i++
    $clean = $l -replace "\x1b\[[0-9;]*[A-Za-z]", ""
    $norm = $clean -replace 'ΓÇö',' - ' -replace '—',' - ' -replace '–',' - '
    Write-Host "LINE $i RAW: $l"
    Write-Host "LINE $i CLEAN: $norm"
    if ($norm -match 'Decision \((Live|Practice)\):\s*WAIT\s*-\s*(.+)') {
        Write-Host "MATCH REASON: $($Matches[2])"
    } else {
        Write-Host "NO MATCH"
    }
}
