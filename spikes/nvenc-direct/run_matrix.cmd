@echo off
rem Spike 0-C test matrix (run from F:\claude-work\nvenc-spike)
nvenc_spike.exe --size 1920x1080 --fps 60 --mbps 30 --no-subframe --out h264-1080p60-nosub
nvenc_spike.exe --size 1920x1080 --fps 60 --mbps 30 --slices 1 --no-subframe --out h264-1080p60-1slice
nvenc_spike.exe --hevc --size 1920x1080 --fps 60 --mbps 30 --out hevc-1080p60
nvenc_spike.exe --size 1920x1080 --fps 120 --mbps 40 --frames 1200 --out h264-1080p120
nvenc_spike.exe --hevc --size 1920x1080 --fps 120 --mbps 40 --frames 1200 --out hevc-1080p120
nvenc_spike.exe --size 2560x1440 --fps 60 --mbps 50 --out h264-1440p60
nvenc_spike.exe --hevc --size 2560x1440 --fps 120 --mbps 60 --frames 1200 --out hevc-1440p120
nvenc_spike.exe --size 1920x1080 --fps 60 --mbps 30 --ir periodic --out h264-1080p60-irperiodic
