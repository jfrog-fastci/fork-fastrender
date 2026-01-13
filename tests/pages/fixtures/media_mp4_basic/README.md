# Tiny MP4 test asset (H.264 + AAC)

This fixture directory contains a very small MP4 file intended for **offline** manual/automated
media playback testing:

- `test_h264_aac.mp4` (H.264 video + AAC audio, ~1s, 16×16 px).

## Licensing

The audio/video content is generated from synthetic sources (FFmpeg `testsrc` + `sine`), so it
contains no third-party media. It is dedicated to the public domain under **CC0-1.0**.

## How it was generated

The committed file was generated with FFmpeg using a command similar to:

```sh
ffmpeg -y \
  -f lavfi -i testsrc=size=16x16:rate=10:duration=1 \
  -f lavfi -i sine=frequency=440:sample_rate=8000:duration=1 \
  -map 0:v:0 -map 1:a:0 \
  -c:v libx264 -profile:v baseline -level 1.0 -pix_fmt yuv420p \
  -preset veryslow -crf 35 -g 10 -keyint_min 10 -sc_threshold 0 -threads 1 \
  -c:a aac -b:a 16k -ac 1 -ar 8000 \
  -movflags +faststart -map_metadata -1 -fflags +bitexact -flags:v +bitexact -flags:a +bitexact \
  test_h264_aac.mp4
```

