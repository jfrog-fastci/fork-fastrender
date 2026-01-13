# Workstream: Video Support

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

---

## The job

Make `<video>` elements work. Users can watch videos in the browser.

This is a significant capability gap—video is ubiquitous on the modern web.

## What counts

A change counts if it lands at least one of:

- **Format support**: A video codec/container is now playable.
- **Playback control**: Play, pause, seek, volume work correctly.
- **HTMLMediaElement API**: JS can control video programmatically.
- **Performance**: Video plays smoothly without dropped frames.

## Scope

### Owned by this workstream

- `<video>` element rendering
- Video decoding (codec support)
- Audio/video synchronization
- HTMLMediaElement JavaScript API
- Media controls UI (play/pause/seek/volume)
- Fullscreen video
- `<audio>` element (shares infrastructure)

### NOT owned

- Streaming protocols (HLS, DASH) → future extension
- DRM/EME → future extension
- WebRTC → separate workstream if needed
- `<canvas>` video capture → future extension

## Priority order

### P0: Basic video playback

1. **Container format parsing**
   - MP4 (most common)
   - WebM (open format)
   
2. **Video codec decoding**
   - H.264/AVC (most common, may need licensing consideration)
   - VP8/VP9 (open, royalty-free)
   - Consider AV1 (modern, royalty-free)

3. **Audio codec decoding**
   - AAC (most common with H.264)
   - Opus (modern, open)
   - Vorbis (open, with WebM)

4. **Frame rendering**
   - Decode video frames
   - Display at correct timing
   - Handle aspect ratio/sizing

### P1: Playback controls

1. **Native controls**
   - Play/pause button
   - Seek bar with preview
   - Volume control
   - Fullscreen toggle
   - Time display

2. **Keyboard controls**
   - Space: play/pause
   - Arrow keys: seek
   - M: mute
   - F: fullscreen

### P2: HTMLMediaElement API

```javascript
// These should work:
video.play();
video.pause();
video.currentTime = 30;
video.volume = 0.5;
video.muted = true;
video.playbackRate = 2.0;

// Events should fire:
video.addEventListener('play', ...);
video.addEventListener('pause', ...);
video.addEventListener('timeupdate', ...);
video.addEventListener('ended', ...);
video.addEventListener('error', ...);
```

### P3: Advanced features

- Poster image before play
- Preload attribute (`none`, `metadata`, `auto`)
- Loop attribute
- Autoplay (with restrictions)
- Picture-in-picture
- Subtitles/captions (`<track>` element)

## Architecture

Developer reference: [`docs/media_pipeline.md`](../docs/media_pipeline.md) describes the chosen
demux+decode dependency stack (MP4/WebM + H.264/VP9/AAC/Opus), timestamp normalization, seeking, and
build constraints.

### Decoding library options

| Library | Pros | Cons |
|---------|------|------|
| ffmpeg (via bindings) | Full format support | Large dependency, licensing |
| gstreamer | Plugin architecture | Complex, system dependency |
| libvpx + libopus | Open codecs only | No H.264 |
| av1-decoder + opus | Modern, open | Limited legacy support |
| System codecs | Native perf, legal clarity | Platform-specific |

**Recommendation**: Start with system codecs (AVFoundation on macOS, Media Foundation on Windows, GStreamer on Linux) for H.264, add libvpx/opus for VP8/VP9/WebM.

### Integration points

```
┌─────────────────────────────────────────────────────────┐
│                    FastRender                           │
├─────────────────────────────────────────────────────────┤
│  DOM: <video> element                                   │
│    ↓                                                    │
│  Layout: Size/position video box                        │
│    ↓                                                    │
│  Paint: Request frame from decoder                      │
│    ↓                                                    │
│  ┌─────────────────────────────────────────────────┐   │
│  │  Video Decoder                                   │   │
│  │  - Parse container (MP4/WebM)                   │   │
│  │  - Decode video frames                          │   │
│  │  - Decode audio samples                         │   │
│  │  - Sync A/V                                     │   │
│  │  - Provide frames to renderer                   │   │
│  └─────────────────────────────────────────────────┘   │
│    ↓                                                    │
│  Compositor: Overlay video frame on page               │
└─────────────────────────────────────────────────────────┘
```

### Audio output

- Need audio output backend
- Options: cpal (cross-platform Rust), system APIs
- Must sync with video frames
- Media clocking model (audio master clock, UI tick is wake-up only): [`docs/media_clocking.md`](../docs/media_clocking.md)

## Testing

### Test videos

Deterministic, license-clean media fixtures live under
[`tests/pages/fixtures/assets/media/`](../tests/pages/fixtures/assets/media/) — see
[`README.md`](../tests/pages/fixtures/assets/media/README.md) for provenance and regeneration.

Create/collect test videos in various formats:
- `test_h264_aac.mp4` - Common web video
- `test_vp9_opus.webm` - Open format
- `test_av1_opus.mp4` - Modern format
- Various resolutions: 360p, 720p, 1080p, 4K

When importing offline page fixtures that need **playable** media (e.g. to exercise `<video>` in the
windowed `browser` UI), note that `xtask import-page-fixture` rewrites media sources to deterministic
empty `assets/missing_<hash>.<ext>` placeholder files by default to keep fixtures small (placeholders
are 0-byte files; they exist so fixtures remain hermetic/offline).

Opt in to vendoring media bytes with:

```bash
bash scripts/cargo_agent.sh xtask import-page-fixture <bundle.tar> <fixture_name> --include-media
```

Safety: vendored media is capped by `--media-max-bytes` (total) and `--media-max-file-bytes` (per
file). Defaults are **5 MiB total** and **2 MiB per file**; set either to `0` to disable the limit
if you intentionally need larger files.

Note: if you need the bundle capture itself to include media bytes (instead of placeholders), capture
with crawl mode so HTML discovery picks up media URLs:

```bash
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bundle_page -- \
  fetch --no-render --prefetch-media <url> --out /tmp/capture.tar
```

If media is skipped due to size, relax `bundle_page`'s caps with `--prefetch-media-max-bytes` and/or
`--prefetch-media-max-total-bytes` (set either to `0` to disable).

### Test pages

```html
<!-- basic_video.html -->
<video src="test.mp4" controls></video>

<!-- js_controlled_video.html -->
<video id="v" src="test.mp4"></video>
<button onclick="document.getElementById('v').play()">Play</button>

<!-- autoplay_muted.html (should work) -->
<video src="test.mp4" autoplay muted></video>
```

### Metrics

- **Format support**: Which codecs/containers work
- **Playback smoothness**: Dropped frames during playback
- **Sync accuracy**: A/V drift over time
- **Seek latency**: Time from seek to frame display

## Relationship to other workstreams

- **live_rendering.md**: Video requires the live render loop (animation frames)
- **js_web_apis.md**: HTMLMediaElement API
- **browser_responsiveness.md**: Video shouldn't impact chrome performance

## Success criteria

Video support is **done** when:
- MP4 (H.264+AAC) plays correctly
- WebM (VP9+Opus) plays correctly
- Native controls work
- JS API works
- Audio is synced
- No dropped frames during playback
