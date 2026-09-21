param(
    [string]$Path = (Join-Path $env:LOCALAPPDATA 'tmux-mcp\events.jsonl'),
    [switch]$Json
)
$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $Path)) { throw "Action log not found: $Path" }

# A tracked command updates the same record repeatedly; count each tool call once.
$records = @{}
$invalid = 0
foreach ($line in Get-Content -LiteralPath $Path) {
    if (-not $line.Trim()) { continue }
    try {
        $record = $line | ConvertFrom-Json
        if ($record.id) { $records[$record.id] = $record }
    } catch { $invalid++ }
}
$measured = @($records.Values | Where-Object { $null -ne $_.timing })
function Percentile($samples, [double]$quantile) {
    $sorted = @($samples | Sort-Object)
    if ($sorted.Count -eq 0) { return 0 }
    $sorted[[Math]::Max(0, [int][Math]::Ceiling($sorted.Count * $quantile) - 1)]
}
$rows = @($measured | Group-Object { @($_.timing.target, $_.tool) | ConvertTo-Json -Compress } | ForEach-Object {
    $calls = @($_.Group)
    $transports = @($calls | ForEach-Object { $_.timing.transports })
    [PSCustomObject]@{
        target = $calls[0].timing.target
        tool = $calls[0].tool
        calls = $calls.Count
        errors = @($calls | Where-Object { $_.timing.outcome -eq 'error' }).Count
        waitTimeouts = @($calls | Where-Object { $_.timing.waitTimedOut }).Count
        p50Ms = Percentile @($calls.timing.durationMs) 0.50
        p95Ms = Percentile @($calls.timing.durationMs) 0.95
        maxMs = ($calls.timing.durationMs | Measure-Object -Maximum).Maximum
        beforeDispatchMs = ($calls.timing.beforeDispatchMs | Measure-Object -Sum).Sum
        subprocesses = $transports.Count
        sshProcesses = @($transports | Where-Object { $_.transport -eq 'ssh' }).Count
        subprocessTimeouts = @($transports | Where-Object { $_.outcome -eq 'timeout' }).Count
        queueMs = ($transports.queueMs | Measure-Object -Sum).Sum
        processMs = (($transports | ForEach-Object { $_.durationMs - $_.queueMs }) | Measure-Object -Sum).Sum
        omittedSubprocesses = ($calls.timing.omittedTransports | Measure-Object -Sum).Sum
    }
} | Sort-Object target, tool)
$report = [PSCustomObject]@{
    measuredCalls = $measured.Count
    unmeasuredRecords = $records.Count - $measured.Count
    invalidLines = $invalid
    rows = $rows
}
if ($Json) { $report | ConvertTo-Json -Depth 5 }
else {
    "Measured calls: $($report.measuredCalls); records without timing: $($report.unmeasuredRecords); invalid lines: $invalid"
    $rows | Format-Table target, tool, calls, errors, waitTimeouts, p50Ms, p95Ms, sshProcesses, queueMs, processMs -AutoSize
    'Process times are sums (parallel calls overlap), not SSH handshake measurements. Use -Json for all fields.'
}
