# Tiny WebM test asset (VP9 + Opus)

This fixture directory contains a very small WebM file intended for **offline** manual/automated
media playback testing:

- `test_vp9_opus.webm` (VP9 video + Opus audio, ~1s, 16×16 px).

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
  -c:v libvpx-vp9 -b:v 0 -crf 45 -g 10 -pix_fmt yuv420p -threads 1 -row-mt 0 \
  -c:a libopus -b:a 16k -ac 1 -ar 8000 \
  -map_metadata -1 -fflags +bitexact -flags:v +bitexact -flags:a +bitexact \
  test_vp9_opus.webm
```

