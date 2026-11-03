# Builds the Windows side of scripts/e2e_matrix.py (GNU target via MSYS2, as
# on the Frames test PC). Usage: win_build.ps1 -Source F:\frames-e2e -Target F:\...\target
param([string]$Source, [string]$Target, [string]$Env = 'F:\claude-work\env.ps1')
. $Env
$ErrorActionPreference = 'Continue'
Set-Location $Source
$env:CARGO_TARGET_DIR = $Target
cmd /c 'cargo build --release --locked -p frames --bin frames --example e2e_probe --example control_probe 2>&1' | Select-Object -Last 4
$frames = $LASTEXITCODE
cmd /c 'cargo build --release --locked -p frames-native --example window_fixture 2>&1' | Select-Object -Last 2
"BUILD_EXIT=$([Math]::Max($frames, $LASTEXITCODE))"
