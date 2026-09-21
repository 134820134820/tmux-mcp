$ErrorActionPreference = 'Stop'
$fixture = New-TemporaryFile
try {
    $base = @{id='a';tool='capture-pane';timing=@{target='host-intern';durationMs=10;beforeDispatchMs=0;outcome='ok';waitTimedOut=$false;omittedTransports=0;transports=@(@{transport='ssh';queueMs=2;durationMs=8;outcome='ok'})}}
    $other = @{id='b';tool='capture-pane';timing=@{target='host-admin';durationMs=30;beforeDispatchMs=1;outcome='error';waitTimedOut=$false;omittedTransports=0;transports=@()}}
    $lines = @($base, $base, $other, @{id='old';tool='capture-pane'}) | ForEach-Object { $_ | ConvertTo-Json -Depth 6 -Compress }
    [IO.File]::WriteAllLines($fixture.FullName, @($lines) + @('{partial'))
    $report = (& (Join-Path $PSScriptRoot 'summarize-timings.ps1') -Path $fixture.FullName -Json) | ConvertFrom-Json
    if ($report.measuredCalls -ne 2 -or $report.unmeasuredRecords -ne 1 -or $report.invalidLines -ne 1) { throw 'Deduplication or legacy record handling failed' }
    $intern = $report.rows | Where-Object target -eq 'host-intern'
    $admin = $report.rows | Where-Object target -eq 'host-admin'
    if ($intern.calls -ne 1 -or $intern.p95Ms -ne 10 -or $intern.sshProcesses -ne 1 -or $intern.processMs -ne 6) { throw 'Timing aggregation failed' }
    if ($admin.errors -ne 1 -or $admin.subprocesses -ne 0) { throw 'Target isolation or empty transport handling failed' }
    'Timing summary checks passed.'
} finally { Remove-Item -LiteralPath $fixture.FullName }
