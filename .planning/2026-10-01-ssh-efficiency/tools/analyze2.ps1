$ErrorActionPreference = 'Stop'
$path = Join-Path $env:LOCALAPPDATA 'tmux-mcp\events.jsonl'
$records = @{}
foreach ($line in Get-Content -LiteralPath $path) {
    if (-not $line.Trim()) { continue }
    try { $r = $line | ConvertFrom-Json; if ($r.id) { $records[$r.id] = $r } } catch {}
}
$all = @($records.Values | Sort-Object requestedAtMs)

"== tracking_error reasons =="
$all | Where-Object { $_.commandSnapshot.status -eq 'tracking_error' } | ForEach-Object {
    $reason = "$($_.commandSnapshot.reason)" -replace '\s+', ' '
    $reason.Substring(0, [Math]::Min(150, $reason.Length))
} | Group-Object | Sort-Object Count -Descending | Select-Object Count, Name | Format-Table -AutoSize -Wrap | Out-String -Width 260

"== tracking_error by month-day =="
$all | Where-Object { $_.commandSnapshot.status -eq 'tracking_error' } | Group-Object { [DateTimeOffset]::FromUnixTimeMilliseconds($_.requestedAtMs).LocalDateTime.ToString('MM-dd') } | Select-Object Count, Name | Format-Table -AutoSize | Out-String

"== SSH processes per timed execute-command / get-command-result =="
$all | Where-Object { $_.timing -and $_.tool -in 'execute-command','get-command-result' } | ForEach-Object {
    ($_.timing.transports | ForEach-Object operation) -join ','
} | Group-Object | Sort-Object Count -Descending | Select-Object -First 8 Count, Name | Format-Table -AutoSize -Wrap | Out-String -Width 260

"== paste-text argument keys and sample =="
$all | Where-Object { $_.tool -eq 'paste-text' } | Select-Object -Last 3 | ForEach-Object { $_.arguments | ConvertTo-Json -Compress -Depth 3 }

"== What follows a tracking_error within 10 minutes (same source) =="
$errs = $all | Where-Object { $_.commandSnapshot.status -eq 'tracking_error' }
foreach ($e in ($errs | Select-Object -Last 6)) {
    $after = $all | Where-Object { $_.source -eq $e.source -and $_.requestedAtMs -gt $e.requestedAtMs -and $_.requestedAtMs -lt $e.requestedAtMs + 600000 } | Select-Object -First 6
    "{0} {1}: {2}" -f [DateTimeOffset]::FromUnixTimeMilliseconds($e.requestedAtMs).LocalDateTime.ToString('MM-dd HH:mm'), $e.source, (($after | ForEach-Object { "$($_.tool)[$($_.status)]" }) -join ' > ')
}

"== Calls per hour of active work (Claude Code, last 7 days) =="
$cut = [DateTimeOffset]::Now.AddDays(-7).ToUnixTimeMilliseconds()
$recent = $all | Where-Object { $_.requestedAtMs -gt $cut -and $_.source -ne '你' }
"recent calls: $($recent.Count)"
$recent | Group-Object tool | Sort-Object Count -Descending | Select-Object Count, Name | Format-Table -AutoSize | Out-String
