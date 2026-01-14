# Media fixtures

This directory contains tiny, deterministic media files used by demux/decode unit tests.

The fixtures are generated from FFmpeg's built-in sources (`lavfi`) so they contain no
third‑party images/audio.

## Fixtures

### `test_h264_aac.mp4`

- Container: MP4
- Video: H.264 (Constrained Baseline), 64×64, 1 fps, 2 frames (red then blue)
- Audio: AAC-LC, stereo, 48 kHz, silence, ~2s

### `test_h264_b_frames_aac.mp4`

- Container: MP4
- Video: H.264 (Main, includes B-frames), 16×16, 1 fps, 4 frames
- Audio: AAC-LC, stereo, 48 kHz, silence, ~4s

### `test_vp9_opus.webm`

- Container: WebM
- Video: VP9, 64×64, 1 fps, 2 frames (red then blue)
- Audio: Opus, stereo, 48 kHz, silence, ~2s

### `vp9_in_mp4.mp4`

- Container: MP4
- Video: VP9, 16×16, 1 fps, 1 frame (red)
- Audio: none

## Regeneration

All commands below are deterministic given the same FFmpeg build.

From the repository root:

```bash
mkdir -p tests/fixtures/media

# H.264 + AAC in MP4.
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "color=c=red:s=64x64:r=1:d=1" \
  -f lavfi -i "color=c=blue:s=64x64:r=1:d=1" \
  -f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=48000" \
  -filter_complex "[0:v][1:v]concat=n=2:v=1:a=0,format=yuv420p[v]" \
  -map "[v]" -map 2:a \
  -t 2 \
  -c:v libx264 -pix_fmt yuv420p -profile:v baseline -level 3.0 -crf 35 -preset veryslow -threads 1 \
  -g 1 -keyint_min 1 -sc_threshold 0 \
  -c:a aac -b:a 32k -ac 2 -ar 48000 \
  -movflags +faststart -map_metadata -1 -fflags +bitexact -flags:v +bitexact -flags:a +bitexact \
  tests/fixtures/media/test_h264_aac.mp4

# H.264 (B-frames) + AAC in MP4.
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=size=16x16:rate=1:duration=4" \
  -f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=48000" \
  -map 0:v -map 1:a \
  -t 4 \
  -c:v libx264 -pix_fmt yuv420p -profile:v main -level 3.0 -crf 35 -preset veryslow -threads 1 \
  -g 4 -keyint_min 4 -sc_threshold 0 -bf 2 \
  -c:a aac -b:a 32k -ac 2 -ar 48000 \
  -movflags +faststart -map_metadata -1 -fflags +bitexact -flags:v +bitexact -flags:a +bitexact \
  tests/fixtures/media/test_h264_b_frames_aac.mp4

# VP9 + Opus in WebM.
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "color=c=red:s=64x64:r=1:d=1" \
  -f lavfi -i "color=c=blue:s=64x64:r=1:d=1" \
  -f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=48000" \
  -filter_complex "[0:v][1:v]concat=n=2:v=1:a=0,format=yuv420p[v]" \
  -map "[v]" -map 2:a \
  -t 2 \
  -c:v libvpx-vp9 -b:v 0 -crf 40 -g 1 -threads 1 -row-mt 0 \
  -c:a libopus -b:a 32k -ac 2 -ar 48000 \
  -map_metadata -1 -fflags +bitexact -flags:v +bitexact -flags:a +bitexact \
  tests/fixtures/media/test_vp9_opus.webm

# VP9 in MP4.
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "color=c=red:s=16x16:r=1:d=1" \
  -frames:v 1 -an \
  -c:v libvpx-vp9 -b:v 0 -crf 40 -g 1 -threads 1 -row-mt 0 -pix_fmt yuv420p -tag:v vp09 \
  -movflags +faststart -map_metadata -1 -fflags +bitexact -flags:v +bitexact \
  tests/fixtures/media/vp9_in_mp4.mp4
```

## Licensing

These files are generated from synthetic sources (solid colors + silence) and contain no
third‑party content. They are dedicated to the public domain under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/).
