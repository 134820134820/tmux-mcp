$ErrorActionPreference = 'Stop'
$path = Join-Path $env:LOCALAPPDATA 'tmux-mcp\events.jsonl'
$records = @{}
foreach ($line in Get-Content -LiteralPath $path) {
    if (-not $line.Trim()) { continue }
    try { $r = $line | ConvertFrom-Json; if ($r.id) { $records[$r.id] = $r } } catch {}
}
$all = @($records.Values)
$times = $all | ForEach-Object { $_.requestedAtMs } | Measure-Object -Minimum -Maximum
"Span: {0} -> {1}" -f [DateTimeOffset]::FromUnixTimeMilliseconds($times.Minimum).LocalDateTime, [DateTimeOffset]::FromUnixTimeMilliseconds($times.Maximum).LocalDateTime

"`n== Records by source/kind/status =="
$all | Group-Object source, kind, status | Sort-Object Count -Descending | Select-Object Count, Name | Format-Table -AutoSize | Out-String -Width 200

"`n== Untimed records by tool/status =="
$all | Where-Object { $null -eq $_.timing } | Group-Object source, tool, status | Sort-Object Count -Descending | Select-Object -First 25 Count, Name | Format-Table -AutoSize | Out-String -Width 200

"`n== SSH process duration per operation (ms) =="
$all | Where-Object { $_.timing } | ForEach-Object { $_.timing.transports } | Where-Object { $_.transport -eq 'ssh' } |
    Group-Object operation | ForEach-Object {
        $d = @($_.Group.durationMs | Sort-Object)
        [PSCustomObject]@{ op = $_.Name; n = $d.Count; min = $d[0]; p50 = $d[[int]($d.Count * 0.5)]; p90 = $d[[Math]::Min($d.Count - 1, [int]($d.Count * 0.9))] }
    } | Sort-Object n -Descending | Format-Table -AutoSize | Out-String -Width 200

"`n== Error texts (first 140 chars) =="
$all | Where-Object { $_.status -in 'failed','rejected' -or $_.result.isError } | ForEach-Object {
    $text = ($_.result.content | Where-Object type -eq 'text' | Select-Object -First 1).text
    if (-not $text) { $text = $_.reason }
    if (-not $text) { $text = ($_.result | ConvertTo-Json -Compress -Depth 3) }
    "{0} | {1}" -f $_.tool, (($text -replace '\s+', ' ').Substring(0, [Math]::Min(140, ($text -replace '\s+', ' ').Length)))
} | Group-Object | Sort-Object Count -Descending | Select-Object -First 30 Count, Name | Format-Table -AutoSize -Wrap | Out-String -Width 260

"`n== send-keys / paste-text samples =="
$all | Where-Object { $_.tool -in 'send-keys','paste-text','press-special-key' -and $_.source -ne '你' } | Sort-Object requestedAtMs | Select-Object -Last 40 | ForEach-Object {
    $a = $_.arguments
    $t = if ($a.keys) { $a.keys } elseif ($a.text) { $a.text } elseif ($a.key) { $a.key } else { '' }
    "{0} {1} enter={2} | {3}" -f $_.tool, $a.paneId, (-not $a.noEnter), (($t -replace '\s+', ' ').Substring(0, [Math]::Min(90, ($t -replace '\s+', ' ').Length)))
}

"`n== execute-command waitMs and statuses =="
$all | Where-Object { $_.tool -eq 'execute-command' } | Group-Object { $_.arguments.waitMs } | Sort-Object Count -Descending | Select-Object -First 10 Count, Name | Format-Table -AutoSize | Out-String
$all | Where-Object { $_.tool -eq 'execute-command' } | ForEach-Object { $_.commandSnapshot.status } | Group-Object | Select-Object Count, Name | Format-Table -AutoSize | Out-String
