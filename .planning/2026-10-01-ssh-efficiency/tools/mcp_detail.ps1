$all = Get-CimInstance Win32_Process
foreach ($m in ($all | Where-Object { $_.Name -eq 'tmux-mcp.exe' })) {
    $parent = $all | Where-Object { $_.ProcessId -eq $m.ParentProcessId }
    $parentName = if ($parent) { "$($parent.Name) (pid $($parent.ProcessId), alive)" } else { "pid $($m.ParentProcessId) (gone)" }
    "{0} started {1}" -f $m.ProcessId, $m.CreationDate
    "    parent: $parentName"
    "    args:   $($m.CommandLine.Substring($m.CommandLine.IndexOf('tmux-mcp.exe') + 13))"
    $all | Where-Object { $_.ParentProcessId -eq $m.ProcessId -and $_.Name -eq 'ssh.exe' } | ForEach-Object {
        "    ssh:    $($_.CommandLine.Substring(0, [Math]::Min(140, $_.CommandLine.Length)))"
    }
}
