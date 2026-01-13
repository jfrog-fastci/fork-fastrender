# Tiny WebM test asset (VP9 + Opus)

This fixture directory contains a very small WebM file intended for **offline** manual/automated
media playback testing:

- `test_vp9_opus.webm` (VP9 video + Opus audio, 64×64, 1 fps, 2 frames: red then blue, ~2s).

This file is intentionally kept identical to `tests/fixtures/media/test_vp9_opus.webm` so unit tests
can share the same deterministic content.

## Licensing

The audio/video content is generated from synthetic sources (solid colors + silence), so it contains
no third-party media. It is dedicated to the public domain under **CC0-1.0**.

## How it was generated

See `tests/fixtures/media/README.md` for the exact FFmpeg command lines used to generate this file.
