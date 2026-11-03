@echo off
rem Realistic-detail runs (lower QP than the worst-case noise pattern)
nvenc_spike.exe --detail 8 --size 1920x1080 --fps 60 --mbps 30 --out d8-h264-1080p60
nvenc_spike.exe --detail 8 --hevc --size 1920x1080 --fps 60 --mbps 30 --out d8-hevc-1080p60
nvenc_spike.exe --detail 8 --size 1920x1080 --fps 120 --mbps 40 --frames 1200 --out d8-h264-1080p120
nvenc_spike.exe --detail 8 --hevc --size 1920x1080 --fps 120 --mbps 40 --frames 1200 --out d8-hevc-1080p120
nvenc_spike.exe --detail 8 --size 1920x1080 --fps 60 --mbps 30 --no-subframe --out d8-h264-1080p60-nosub
