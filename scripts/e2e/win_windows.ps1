# Lists executables of the current user's processes that own a visible
# window (for host --exclude lists), or with -Pattern, processes whose
# command line contains the pattern (stray-process check). One per line.
param([string]$Pattern)
if ($Pattern) {
    # '|'-separated substrings matched against the command line or executable path.
    $needles = $Pattern.Split('|') | Where-Object { $_ }
    Get-CimInstance Win32_Process |
        Where-Object {
            $process = $_
            $process.ProcessId -ne $PID -and $process.Name -notin @('powershell.exe', 'cmd.exe', 'conhost.exe') -and
            ($needles | Where-Object { ($process.CommandLine -and $process.CommandLine.Contains($_)) -or
                                       ($process.ExecutablePath -and $process.ExecutablePath.StartsWith($_, 'OrdinalIgnoreCase')) })
        } |
        ForEach-Object { "$($_.ProcessId) $($_.Name)" }
} else {
    Get-Process | Where-Object { $_.MainWindowHandle -ne 0 -and $_.Path } |
        Select-Object -ExpandProperty Path -Unique
}
