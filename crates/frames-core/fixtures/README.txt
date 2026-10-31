Decoder probe inputs are single, generated blue 128x128 frames. No screen
content is included. Clients decode these files without probing any encoder.

Generated with FFmpeg:
ffmpeg -f lavfi -i color=c=blue:s=128x128:r=30 -frames:v 1 -an -c:v ENCODER -pix_fmt yuv420p -f FORMAT FILE

FILE           ENCODER       FORMAT
probe.h264     libx264       h264
probe.hevc     libx265       hevc
probe-av1.ivf  libaom-av1     ivf
probe-vp9.ivf  libvpx-vp9     ivf
