@echo off
cd /d F:\claude-work\nvenc-spike
start "" /B nvenc_spike.exe --fps 60 --frames 900 --out mid > mid.out 2>&1
ping -n 6 127.0.0.1 >nul
nvidia-smi --query-gpu=pstate,clocks.gr,clocks.video,clocks.mem,utilization.encoder,utilization.gpu,power.draw --format=csv
ping -n 13 127.0.0.1 >nul
type mid.out | findstr /C:"first-slice"
