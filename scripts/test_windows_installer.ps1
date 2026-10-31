param([string]$SourceExe = '', [string]$Version = '')
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $SourceExe) { $SourceExe = Join-Path $root 'dist\windows\Frames.exe' }
$SourceExe = (Resolve-Path -LiteralPath $SourceExe).Path
if (-not $Version) {
    $Version = [regex]::Match((Get-Content (Join-Path $root 'Cargo.toml') -Raw), '(?m)^version = "([^"]+)"').Groups[1].Value
}
$compiler = @(
    'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
    'C:\Program Files\Inno Setup 6\ISCC.exe'
) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
if (-not $compiler) { $compiler = (Get-Command ISCC.exe -ErrorAction Stop).Source }
$id = [guid]::NewGuid().ToString('N')
$appId = "dev.creaton.frames.test-$id"
$shortcutName = "Frames Installer Test $id"
$temporary = Join-Path $root "target\installer-test-$id"
$install = Join-Path $temporary 'installed'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) "$shortcutName.lnk"
$registration = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\${appId}_is1"
New-Item -ItemType Directory -Path $temporary -Force | Out-Null

function Assert-Installer($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Build-Fixture([string]$FixtureVersion, [string]$Name) {
    & $compiler /Q "/DAppVersion=$FixtureVersion" "/DAppSource=$SourceExe" "/DFramesAppId=$appId" "/DShortcutName=$shortcutName" "/O$temporary" "/F$Name" (Join-Path $root 'native\windows\installer.iss')
    if ($LASTEXITCODE) { throw 'Installer fixture compilation failed' }
    return (Join-Path $temporary "$Name.exe")
}
function Invoke-Installer([string]$Executable, [string]$Log, [switch]$Uninstall) {
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', "/LOG=`"$temporary\$Log.log`"")
    if (-not $Uninstall) { $arguments += "/DIR=`"$install`"" }
    return (Start-Process -FilePath $Executable -ArgumentList $arguments -Wait -PassThru -WindowStyle Hidden).ExitCode
}
try {
    $older = Build-Fixture '0.0.0' 'older-setup'
    $current = Build-Fixture $Version 'current-setup'
    Assert-Installer ((Invoke-Installer $older 'install') -eq 0) 'Fresh installation failed'
    $executable = Join-Path $install 'Frames.exe'
    $uninstaller = Join-Path $install 'unins000.exe'
    $sourceHash = (Get-FileHash -LiteralPath $SourceExe).Hash
    Assert-Installer ((Get-FileHash -LiteralPath $executable).Hash -eq $sourceHash) 'Installed payload differs from the built executable'
    Assert-Installer ((Get-ItemProperty -LiteralPath $registration).DisplayName -eq 'Frames') 'Installed Apps name is incorrect'
    Assert-Installer ((Get-ItemProperty -LiteralPath $registration).DisplayVersion -eq '0.0.0') 'Initial version was not registered'
    Assert-Installer (Test-Path -LiteralPath $shortcut) 'Start menu shortcut is missing'
    $shell = New-Object -ComObject WScript.Shell
    Assert-Installer ($shell.CreateShortcut($shortcut).TargetPath -eq $executable) 'Shortcut targets the wrong executable'

    # Model an executable in use without opening a window or touching real sessions.
    $held = [IO.File]::Open($executable, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        Assert-Installer ((Invoke-Installer $current 'locked-upgrade') -ne 0) 'Installer replaced a locked/running application'
        Assert-Installer ((Invoke-Installer $uninstaller 'locked-uninstall' -Uninstall) -ne 0) 'Uninstaller removed a locked/running application'
    } finally { $held.Dispose() }
    Assert-Installer ((Get-FileHash -LiteralPath $executable).Hash -eq $sourceHash) 'Failed upgrade changed the installed application'

    # Confirm that an upgrade repairs stale payloads and uses the same registration.
    Set-Content -LiteralPath $executable -Value 'old installer fixture' -Encoding ascii
    $sentinel = Join-Path $install 'user-data-to-preserve.txt'
    Set-Content -LiteralPath $sentinel -Value 'keep this file' -Encoding ascii
    Assert-Installer ((Invoke-Installer $current 'upgrade') -eq 0) 'Upgrade failed'
    Assert-Installer ((Get-FileHash -LiteralPath $executable).Hash -eq $sourceHash) 'Upgrade left a stale payload'
    Assert-Installer ((Get-ItemProperty -LiteralPath $registration).DisplayVersion -eq $Version) 'Upgrade did not refresh Installed Apps version'
    $metadata = (Get-Item -LiteralPath $executable).VersionInfo
    Assert-Installer ($metadata.ProductName -eq 'Frames') 'Installed executable has the wrong product name'
    $media = Start-Process -FilePath $executable -ArgumentList @('media-check') -WorkingDirectory $install -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $temporary 'media.json') -RedirectStandardError (Join-Path $temporary 'media.log')
    Assert-Installer ($media.ExitCode -eq 0) 'Installed media codecs failed without an external FFmpeg executable'
    $result = Get-Content -LiteralPath (Join-Path $temporary 'media.json') -Raw | ConvertFrom-Json
    Assert-Installer $result.opus_10ms_roundtrip 'Installed Opus roundtrip failed'
    Assert-Installer ((Invoke-Installer $uninstaller 'uninstall' -Uninstall) -eq 0) 'Uninstallation failed'
    Assert-Installer (-not (Test-Path -LiteralPath $executable)) 'Uninstaller left the application executable'
    Assert-Installer (-not (Test-Path -LiteralPath $registration)) 'Uninstaller left the Installed Apps entry'
    Assert-Installer (-not (Test-Path -LiteralPath $shortcut)) 'Uninstaller left the Start menu shortcut'
    Assert-Installer (Test-Path -LiteralPath $sentinel) 'Uninstaller deleted an untracked user file'
    Write-Host 'Frames installer: install, shortcut, locked-file protection, upgrade, bundled codecs, uninstall, and data preservation passed.'
} catch {
    Get-ChildItem -LiteralPath $temporary -Filter '*.log' | ForEach-Object { Get-Content -LiteralPath $_.FullName -Tail 25 }
    throw
} finally {
    $uninstaller = Join-Path $install 'unins000.exe'
    if (Test-Path -LiteralPath $uninstaller) { $null = Invoke-Installer $uninstaller 'cleanup' -Uninstall }
    # This directory contains only this test's UUID-scoped installers and fixture.
    $resolved = [IO.Path]::GetFullPath($temporary)
    $expectedParent = [IO.Path]::GetFullPath((Join-Path $root 'target')) + [IO.Path]::DirectorySeparatorChar
    if (-not $resolved.StartsWith($expectedParent, [StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolved -Leaf) -ne "installer-test-$id") {
        throw 'Refusing to clean up an unexpected installer test directory'
    }
    if (Test-Path -LiteralPath $resolved) { Remove-Item -LiteralPath $resolved -Recurse -Force }
}
