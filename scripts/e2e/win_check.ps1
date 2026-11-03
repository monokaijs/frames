# fmt + clippy (-D warnings) for the crates the e2e matrix touches, on the
# Windows test PC. Usage: win_check.ps1 -Source F:\frames-e2e -Target F:\...\target
param([string]$Source, [string]$Target, [string]$Env = 'F:\claude-work\env.ps1')
. $Env
$ErrorActionPreference = 'Continue'
Set-Location $Source
$env:CARGO_TARGET_DIR = $Target
cmd /c 'cargo fmt --all --check 2>&1' | Select-Object -Last 3
"FMT_EXIT=$LASTEXITCODE"
cmd /c 'cargo clippy --release --locked -p frames -p frames-core --all-targets -- -D warnings 2>&1' |
    Select-String -Pattern '^(error|warning)|Finished' -Context 0, 5 | Select-Object -First 30
"CLIPPY_EXIT=$LASTEXITCODE"
