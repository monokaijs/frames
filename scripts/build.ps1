param([switch]$Debug, [string]$Target = '')
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Push-Location -LiteralPath $root
try {
    if (-not $Target) {
        $Target = 'x86_64-pc-windows-msvc'
        if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue) -and (Test-Path -LiteralPath 'C:\msys64\ucrt64\bin\gcc.exe')) {
            $Target = 'x86_64-pc-windows-gnu'
        }
    }
    if ($Target -eq 'x86_64-pc-windows-gnu' -and (Test-Path -LiteralPath 'C:\msys64\ucrt64\bin\gcc.exe')) {
        $env:Path = 'C:\msys64\ucrt64\bin;' + $env:Path
    }
    $buildArgs = @('build', '--locked', '-p', 'frames', '--target', $Target)
    $profile = 'debug'
    if (-not $Debug) { $buildArgs += '--release'; $profile = 'release' }
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) { throw 'Frames build failed' }
    $destination = Join-Path $root 'dist\windows'
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $root "target\$Target\$profile\frames.exe") -Destination (Join-Path $destination 'Frames.exe') -Force
    Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination $destination -Force
    Copy-Item -LiteralPath (Join-Path $root 'docs') -Destination $destination -Recurse -Force
    $digest = (Get-FileHash -LiteralPath (Join-Path $destination 'Frames.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
    "$digest  Frames.exe" | Set-Content -LiteralPath (Join-Path $destination 'Frames.exe.sha256') -Encoding ascii
    Write-Host "Built $destination\Frames.exe"
    Write-Host 'Install FFmpeg separately, or set FRAMES_FFMPEG to its executable path.'
} finally { Pop-Location }
