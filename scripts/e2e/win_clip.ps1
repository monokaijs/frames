# Clipboard helper for scripts/e2e_matrix.py; run through win_launch.ps1 so it
# uses the interactive session's clipboard.
#   -Mode get  -Path out.txt            save the current text
#   -Mode set  -Path in.txt             replace it (UTF-8 file contents)
#   -Mode wait -Path expected.txt -Seconds N   exit 0 once it equals the file
param([ValidateSet('get', 'set', 'wait')] [string]$Mode, [string]$Path, [int]$Seconds = 10)
Add-Type -AssemblyName System.Windows.Forms
function Read-Text {
    $text = [System.Windows.Forms.Clipboard]::GetText()
    if ($null -eq $text) { '' } else { $text }
}
switch ($Mode) {
    'get' { [System.IO.File]::WriteAllText($Path, (Read-Text)) }
    'set' {
        $text = [System.IO.File]::ReadAllText($Path)
        if ($text.Length -eq 0) { [System.Windows.Forms.Clipboard]::Clear() }
        else { [System.Windows.Forms.Clipboard]::SetText($text) }
    }
    'wait' {
        $expected = [System.IO.File]::ReadAllText($Path)
        $deadline = (Get-Date).AddSeconds($Seconds)
        while ((Get-Date) -lt $deadline) {
            if ((Read-Text) -eq $expected) { exit 0 }
            Start-Sleep -Milliseconds 200
        }
        exit 1
    }
}
