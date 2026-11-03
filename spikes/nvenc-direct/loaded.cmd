@echo off
cd /d F:\claude-work\nvenc-spike
start "" /B nvenc_spike.exe --burn 40 > burn.out 2>&1
ping -n 3 127.0.0.1 >nul
start "" /B nvenc_spike.exe --fps 60 --frames 900 --out loaded-noise > l1.out 2>&1
ping -n 7 127.0.0.1 >nul
nvidia-smi --query-gpu=pstate,clocks.gr,clocks.video,utilization.encoder,utilization.gpu,power.draw --format=csv,noheader
ping -n 12 127.0.0.1 >nul
nvenc_spike.exe --detail 8 --hevc --fps 60 --frames 600 --out loaded-hevc-d8 > l2.out 2>&1
type l1.out | findstr /C:"Mbps" /C:"first-slice"
type l2.out | findstr /C:"Mbps" /C:"first-slice"
ping -n 12 127.0.0.1 >nul
type burn.out
