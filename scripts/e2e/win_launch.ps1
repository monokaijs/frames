# Runs one process for scripts/e2e_matrix.py inside the interactive console
# session (started by a one-shot scheduled task created with /IT), where
# window capture, the user's clipboard, DPAPI, and GUI windows work.
#
# Job file (JSON): { "name", "exe", "args": [..], "env": {..}, "dir", "timeout" }
#   <dir>\<name>.out.log / .err.log   process output (live)
#   <dir>\<name>.approve              driver -> answer "yes" on stdin (pairing)
#   <dir>\<name>.stop                 driver -> terminate the process tree
#   <dir>\<name>.exit                 exit code, written last
param([Parameter(Mandatory)] [string]$Job)
$ErrorActionPreference = 'Stop'
$spec = Get-Content -Raw -Encoding UTF8 $Job | ConvertFrom-Json
$base = Join-Path $spec.dir $spec.name
$env:Path = 'C:\msys64\ucrt64\bin;' + $env:Path
if ($spec.env) {
    foreach ($item in $spec.env.PSObject.Properties) {
        [Environment]::SetEnvironmentVariable($item.Name, [string]$item.Value, 'Process')
    }
}
function Quote([string]$value) {
    if ($value -notmatch '[\s"]') { return $value }
    '"' + ($value -replace '"', '\"') + '"'
}
$command = (@($spec.exe) + @($spec.args) | ForEach-Object { Quote $_ }) -join ' '
# cmd owns the redirection so logs are live files; the child inherits the
# stdin pipe that this script writes approvals to.
$info = New-Object System.Diagnostics.ProcessStartInfo
$info.FileName = "$env:SystemRoot\System32\cmd.exe"
$info.Arguments = '/d /s /c "' + $command + ' > "' + "$base.out.log" + '" 2> "' + "$base.err.log" + '""'
$info.UseShellExecute = $false
$info.RedirectStandardInput = $true
$info.CreateNoWindow = $true
$info.WorkingDirectory = $spec.dir
$process = [System.Diagnostics.Process]::Start($info)
Set-Content -Encoding ASCII "$base.pid" $process.Id
$deadline = (Get-Date).AddSeconds([int]$spec.timeout)
$reason = 'exited'
while (-not $process.HasExited) {
    if (Test-Path "$base.approve") {
        Remove-Item -Force "$base.approve"
        $process.StandardInput.WriteLine('yes')
        $process.StandardInput.Flush()
    }
    if (Test-Path "$base.stop") { $reason = 'stopped'; break }
    if ((Get-Date) -gt $deadline) { $reason = 'timeout'; break }
    Start-Sleep -Milliseconds 100
}
if (-not $process.HasExited) {
    & "$env:SystemRoot\System32\taskkill.exe" /T /F /PID $process.Id | Out-Null
    $process.WaitForExit(10000) | Out-Null
}
$code = if ($process.HasExited) { $process.ExitCode } else { -1 }
Set-Content -Encoding ASCII "$base.exit" "$code $reason"
