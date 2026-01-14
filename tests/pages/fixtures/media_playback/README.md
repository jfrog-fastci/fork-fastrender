# Media playback HTML fixtures

This fixture directory contains small, deterministic HTML pages for exercising:

- `<video>` / `<audio>` element loading and playback, and
- the `HTMLMediaElement` JavaScript API (events + controls like `play()`, `pause()`, `currentTime`,
  `muted`, and `volume`).

## Pages

- `basic_video_mp4.html` – `<video>` pointing at `assets/test_h264_aac.mp4`
- `basic_video_webm.html` – `<video>` pointing at `assets/test_vp9_opus.webm`
- `basic_audio.html` – `<audio>` pointing at `assets/test_opus.webm`
- `js_controls.html` – event logging + JS control buttons (play/pause/seek/mute/volume)
- `autoplay_muted.html` – autoplay+muted smoke test with an on-page indicator
- `playback_rate.html` – checks that `playbackRate` affects `currentTime` progression (via periodic
  `timeupdate` events)

## Assets

The media files under `assets/` are deliberately tiny and contain only synthetic content
(solid colors + silence), so they contain no third-party media. They are dedicated to the public
domain under **CC0-1.0**.

- `assets/test_h264_aac.mp4` is kept identical to `tests/fixtures/media/test_h264_aac.mp4`
- `assets/test_vp9_opus.webm` is kept identical to `tests/fixtures/media/test_vp9_opus.webm`
- `assets/test_opus.webm` is an audio-only Opus WebM used for `<audio>` coverage

### Regenerating `test_opus.webm`

From the repository root:

```bash
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=48000" \
  -t 2 \
  -c:a libopus -b:a 32k -ac 2 -ar 48000 \
  -threads 1 \
  -map_metadata -1 -fflags +bitexact -flags:a +bitexact \
  tests/pages/fixtures/media_playback/assets/test_opus.webm
```
