# Disable and remove the live-trading scheduled task, with verification
$taskName = "Binance Survival Bot - 30min Bursts"

$task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if (-not $task) {
    Write-Host "Task '$taskName' not found — already removed or never existed."
    exit 0
}

Write-Host "Found task. Current state: $($task.State)"

Disable-ScheduledTask -TaskName $taskName | Out-Null
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false

$check = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($check) {
    Write-Host "FAILED — task still exists. State: $($check.State)"
    exit 1
} else {
    Write-Host "CONFIRMED — task '$taskName' deleted, no next run scheduled."
}