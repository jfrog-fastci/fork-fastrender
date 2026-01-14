# Media pipeline (demux + decode) — status + developer notes

This document tracks the **current in-tree media pipeline** (under `src/media/`) and the intended
direction for `<video>/<audio>` playback.

FastRender currently treats `<video>` as a *replaced element* for layout/intrinsic sizing purposes
(see `docs/conformance.md`). Full playback requires a demux/decode stack plus a concrete
`MediaFrameProvider` implementation to feed decoded frames into paint.

For the intended clocking model (audio as master clock; UI ticks are wake-ups only), see
`docs/media_clocking.md`.

## Implementation status (repo reality)

Legend: ✅ implemented, ⚠️ partial, 🚧 planned, ❌ missing.

| Area | Status | Notes / code |
| --- | --- | --- |
| Common types (`MediaTrackInfo`, `MediaPacket`, `MediaData`, `Decoded*`) | ✅ | `src/media/mod.rs`, `src/media/packet.rs` |
| WebM demux (`WebmDemuxer`) | ✅ | `src/media/demux/webm.rs` (VP9+Opus packets; seek; optional inter-track ordering; track selection/filtering) |
| MP4 demux + packetizer (`Mp4PacketDemuxer`) | ⚠️ | `src/media/demuxer.rs` (H.264+AAC; best-effort VP9 track detection via `mp4parse`; `dts_ns≈pts_ns`, `duration_ns=0`; seek is not keyframe-aware) |
| MP4 seek index helper (`Mp4SeekIndex`) | ✅ | `src/media/mp4.rs::Mp4SeekIndex` (PTS→sample index; used by `Mp4PacketDemuxer::open(path)` when available) |
| AAC decoder | ✅ | `src/media/codecs/aac.rs` (symphonia AAC → `DecodedAudioChunk`) |
| Opus decoder | ✅ | `src/media/codecs/opus.rs` (`audiopus_sys`/libopus; mapping family 0 mono/stereo only) |
| H.264 decoder | ✅ | `src/media/decoder.rs` (OpenH264; MP4 length-prefixed NALs → Annex B) |
| VP9 decode (libvpx) | ✅ | `src/media/decoder.rs::create_video_decoder` (VP9 path) → `src/media/codecs/vp9.rs` → `crates/libvpx-sys-bundled` (feature: `codec_vp9_libvpx` or `media`) |
| Media backends (`MediaBackend`) | ✅ | `src/media/backends/native.rs`; optional `src/media/backends/ffmpeg_cli.rs` behind `media_ffmpeg_cli` |
| A/V sync helper | ✅ | `src/media/av_sync.rs` (+ env overrides) |
| Audio output plumbing | ✅ | `src/media/audio/*` (real output via `audio_cpal`; null backend is default) |
| `<video>` paint hook + frame caching | ⚠️ | Paint can query a `MediaFrameProvider`; `SizeHintMediaFrameProvider` exists (`src/media/frame_provider.rs`) but no full HTMLMediaElement playback loop is wired yet |

## Design goals / constraints (current)

- **MSRV**: Rust **1.70** (`Cargo.toml:6`).
- **CI-friendly by default**:
  - Core media plumbing is in-process (no system FFmpeg dependency).
  - Optional features may require extra dependencies:
    - `browser_ui` (windowed browser): GUI dev packages on Linux (see `docs/browser_ui.md`).
    - `audio_cpal`: real audio output; may require ALSA headers on Linux.
    - `media_ffmpeg_cli`: requires `ffmpeg`/`ffprobe` binaries on PATH.
- **No required assembler in the default build**:
  - VP9 decode uses `crates/libvpx-sys-bundled`, which aims to avoid `nasm`/`yasm` by forcing a
    portable C-only build. See `crates/libvpx-sys-bundled/README.md` for platform caveats.

## Pipeline overview (current shape)

At a high level, playback is a set of adapters that turn “bytes on disk/network” into decoded audio
samples + video frames on a common timeline:

```text
bytes (file/http/memory)
  ↓
MediaBackend (native / optional ffmpeg CLI)
  ↓
container demux (native backend)
  - WebM: WebmDemuxer        ✅
  - MP4:  Mp4PacketDemuxer   ⚠️ (timestamp/keyframe gaps; see below)
  ↓
MediaPacket {
  track_id,
  dts_ns, pts_ns, duration_ns,
  data: MediaData::{Owned|Shared},
  is_keyframe
}
  ↓
codec decode
  - AAC ✅   → DecodedAudioChunk
  - Opus ✅  → DecodedAudioChunk
  - H.264 ✅ → DecodedVideoFrame (RGBA8)
  - VP9 ✅   → DecodedVideoFrame (RGBA8; libvpx via `codec_vp9_libvpx`)
  ↓
sync + scheduling (Duration / nanosecond timeline)
  ↓
paint (video) + audio backend (audio)
```

Notes on timestamps:

- `MediaPacket.dts_ns` is the decode timestamp and is expected to be monotonic in demux order.
- `MediaPacket.pts_ns` is the presentation timestamp and **may be non-monotonic** for video streams
  with B-frame reordering. Demuxers must not reorder packets **within a track** by PTS.

## Container demux

### WebM / Matroska: `WebmDemuxer` (implemented)

Implementation: `src/media/demux/webm.rs` using [`matroska-demuxer`](https://crates.io/crates/matroska-demuxer).

Current behavior:

- Opens any `R: Read + Seek`.
- Enumerates tracks as `MediaTrackInfo` (codec + codec_private bytes + codec_delay_ns).
- Emits `MediaPacket` **only** for:
  - VP9 (`codec_id = "V_VP9"`)
  - Opus (`codec_id = "A_OPUS"`)
- Track selection/filtering:
  - Track metadata is used to pick “primary” audio/video tracks (see `src/media/track_selection.rs`).
  - `WebmDemuxerOptions.track_filter` controls whether packets are emitted for only the primary
    tracks or for all supported tracks.
- Timestamp normalization:
  - Uses `Info.TimecodeScale` (nanoseconds per tick) to compute `pts_ns`.
  - Subtracts Matroska `TrackEntry.codec_delay` from timestamps (per spec).
- Optional inter-track ordering:
  - When enabled (`WebmDemuxerOptions.inter_track_reordering = true`), `next_packet()` yields
    non-decreasing PTS across tracks using a small bounded queue per track.
- Seeking:
  - `WebmDemuxer::seek(time_ns)` uses `MatroskaFile::seek(...)`.
  - In damaged/unindexed files, seeking may return
    `MediaError::Unsupported("Matroska seek unsupported (no cluster index)")`.

### MP4 / ISO-BMFF: `Mp4PacketDemuxer` (implemented; known timestamp/seek gaps)

Implementation: `src/media/demuxer.rs::Mp4PacketDemuxer` (built on the `mp4` crate).

Current behavior:

- Opens any `R: Read + Seek + Send` (or via the convenience `Mp4PacketDemuxer::open(path)`).
- Enumerates tracks as `MediaTrackInfo` and emits `MediaPacket`s in timestamp order across tracks
  (it peeks the next sample from each active track and returns the smallest `pts_ns`).
- Track detection:
  - H.264 (`mp4::MediaType::H264`) → emits packets
  - AAC (`mp4::MediaType::AAC`) → emits packets
  - VP9: detected by parsing `stsd` via `mp4parse` (because `mp4` does not currently expose VP9 via
    `Mp4Track::media_type()`), then emits packets.

Codec-private (`MediaTrackInfo.codec_private`) formats produced today:

- **H.264**: a minimal custom format derived from `avcC`, used by `decoder::H264Decoder`:

  ```text
  u8  nal_length_size
  u8  sps_count
  [sps_count] { u16be len, [len] bytes }
  u8  pps_count
  [pps_count] { u16be len, [len] bytes }
  ```

- **AAC**: a synthesized **AAC-LC** `AudioSpecificConfig` (ASC) derived from the MP4 track sample
  rate + channel count.
- **VP9**: a compact subset of `vpcC` (bit depth / primaries / subsampling + `codec_init` bytes).

Seeking:

- `Mp4PacketDemuxer::open(path)` makes a best-effort attempt to build an `Mp4SeekIndex` by reading
  the `moov` box once and calling `src/media/mp4.rs::Mp4SeekIndex::from_bytes(...)`. When present,
  `seek(time_ns)` becomes O(log n) per track without scanning packets.
  - The index build is intentionally capped (see `MAX_BOX_BYTES_FOR_INDEX` in `demuxer.rs`) so we
    don’t allocate attacker-controlled huge `moov` boxes.
- When the index is unavailable (e.g. demuxer constructed from a generic reader / bytes), seek falls
  back to a linear scan.

Known limitations / gaps:

- **Timestamp correctness**: the `mp4` crate does not currently expose composition timestamps, so we
  currently treat `sample.start_time` as both `dts_ns` and `pts_ns` (`dts_ns == pts_ns`). This is
  wrong for streams with `ctts` / B-frame reordering.
- **Duration**: `MediaPacket.duration_ns` is currently set to `0`.
- **Keyframe-aware seek**: seek does **not** back up to the previous sync sample (`is_keyframe`); it
  seeks to the first sample with `pts_ns >= time_ns`, which can land in the middle of a GOP.
- **Fragmented MP4** (`moof`/`mdat`) is not supported by this demuxer.

Related utilities:

- `src/media/mp4.rs` contains a separate “sample table” parser (`Mp4Demuxer`) plus the
  `Mp4SeekIndex` helper used above. This code is useful when we need exact `stts`/`ctts`-derived
  PTS/DTS in the future.

## Codec decode backends

### AAC (implemented): `AacDecoder` (symphonia)

Implementation: `src/media/codecs/aac.rs` using:

- `symphonia-core`
- `symphonia-codec-aac`

Input contract:

- The demux layer must provide:
  - AAC access-unit bytes (as `MediaPacket.data`)
  - AAC `AudioSpecificConfig` (ASC) bytes (container-provided) for `AacDecoder::new(...)`

Output:

- `DecodedAudioChunk` with interleaved `f32` samples in `[-1.0, 1.0]`, plus `pts_ns`/`duration_ns`.

### Opus (implemented): `OpusDecoder` (libopus via `audiopus_sys`)

Implementation: `src/media/codecs/opus.rs`.

- Uses the `audiopus_sys` FFI bindings (libopus).
- Expects Matroska/WebM `codec_private` bytes to start with an `OpusHead` header (RFC7845).
- Applies `pre_skip` trimming so initial decoder priming samples are dropped.
- Output is always **48 kHz** (Opus internal sample clock).

Current limitations:

- Only **channel mapping family 0** is supported.
- Only **mono/stereo** streams are supported (`channels` must be 1 or 2).

### H.264 / AVC (implemented): `H264Decoder` (OpenH264)

Implementation: `src/media/decoder.rs` (`H264Decoder`).

Input contract:

- `MediaTrackInfo.codec_private` must be in the custom `avcC`-derived format documented in
  `parse_h264_codec_private(...)` (see source for the exact layout).
- `MediaPacket.data` is expected to contain MP4/AVC **length-prefixed** NAL units (not Annex B start
  codes). The decoder converts packets to Annex B and prepends SPS/PPS before the first decode.

Output:

- `DecodedVideoFrame` with RGBA8 pixels (OpenH264 decodes to YUV and the code converts to RGBA).

### VP9 (implemented): bundled libvpx

Implementation lives in:

- Workspace crate: `crates/libvpx-sys-bundled` (vendored libvpx build + wrapper)
- Media wrapper: `src/media/codecs/vp9.rs` (`codecs::vp9::Vp9Decoder` → RGBA8 frames)

Current status:

- `WebmDemuxer` can emit VP9 packets.
- `MediaDecodePipeline` uses `src/media/decoder.rs::create_video_decoder` to construct a libvpx-backed
  `codecs::vp9::Vp9Decoder` (requires `codec_vp9_libvpx` or `media`).
- `src/media/player.rs` also uses `codecs::vp9` directly for a minimal WebM/VP9 playback loop.

Build notes:

- Requires a C toolchain and GNU make.
- Aims to avoid `nasm`/`yasm` by disabling x86 SIMD and forcing a portable build; see
  `crates/libvpx-sys-bundled/build.rs` and the crate README for target-specific caveats.

## Timestamp normalization (nanoseconds)

The demux boundary normalizes timestamps into nanoseconds (`MediaPacket.{dts_ns,pts_ns,duration_ns}`).

Current implementations:

- **WebM** (`WebmDemuxer`):
  - `pts_ns = frame.timestamp * Info.TimecodeScale`
  - subtracts `TrackEntry.codec_delay`
- **MP4** (`Mp4PacketDemuxer`):
  - `pts_ns` is derived from `mp4::Sample.start_time` and `mdhd.timescale`.
  - Currently `dts_ns == pts_ns` and `duration_ns == 0` (see limitations above).

Clocking/scheduling code uses `Duration` (`src/media/clock.rs`, `src/media/av_sync.rs`) but the unit
is still nanoseconds.

## Seeking model (current behavior)

- **WebM**: `WebmDemuxer::seek(time_ns)` seeks to the first frame at/after the target (after
  compensating for codec delay).
- **MP4**: `Mp4PacketDemuxer::seek(time_ns)` seeks each track to the first sample with
  `pts_ns >= time_ns`. When a prebuilt `Mp4SeekIndex` is available, it uses binary search;
  otherwise it falls back to a linear scan. It does not yet back up to a sync sample/keyframe.

## How to manually test (fixtures)

The repo contains tiny, offline MP4/WebM fixtures and matching HTML pages:

- Raw media assets: `tests/fixtures/media/`
- Playback pages + assets: `tests/pages/fixtures/media_playback/` (assets live in
  `tests/pages/fixtures/media_playback/assets/`)
- Legacy “single page” fixtures: `tests/pages/fixtures/media_mp4_basic/`,
  `tests/pages/fixtures/media_webm_basic/`

The `media_playback/assets/` files are kept in sync with `tests/fixtures/media/` (see
`tests/pages/fixtures/media_playback/README.md`).

Run the windowed browser UI (requires the `browser_ui` feature; see `docs/browser_ui.md` for
platform prereqs):

```bash
# Recommended (applies resource limits):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser
```

Then open these fixture pages:

```bash
# MP4 (H.264 + AAC):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- \
    "file://$PWD/tests/pages/fixtures/media_playback/basic_video_mp4.html"

# WebM (VP9 + Opus):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- \
    "file://$PWD/tests/pages/fixtures/media_playback/basic_video_webm.html"

# Audio-only WebM (Opus):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- \
    "file://$PWD/tests/pages/fixtures/media_playback/basic_audio.html"
```

Useful runtime toggles while debugging:

- Paint backend selection:
  - `FASTR_PAINT_BACKEND=display_list|legacy` (default: `display_list`; see `docs/env-vars.md`).
- Video A/V sync tolerances (used by `src/media/av_sync.rs`):
  - `FASTR_AV_SYNC_TOLERANCE_MS`
  - `FASTR_AV_SYNC_MAX_LATE_MS`
  - `FASTR_AV_SYNC_MAX_EARLY_MS`

Note: full end-to-end decode→paint→DOM integration is still in progress; today these pages are
primarily a smoke test for `<video>` layout and for future playback wiring.

## Known limitations / TODOs (explicit)

- There is no end-to-end HTMLMediaElement playback engine yet (DOM events/state machine, decode
  scheduling threads, audio output as master clock, etc). Paint can display frames if an app supplies
  a `MediaFrameProvider`, but nothing in-tree wires `MediaDecodePipeline`/`MediaPlayer` to the DOM.
- MP4 (`Mp4PacketDemuxer`):
  - `dts_ns`/`pts_ns` are currently treated as equal; streams with `ctts`/B-frames need proper
    DTS/PTS handling.
  - `MediaPacket.duration_ns` is currently `0`.
  - Seeking is not keyframe-aware (does not seek to previous sync sample).
  - Fragmented MP4 is unsupported.
- WebM (`WebmDemuxer`):
  - Seek is best-effort and currently does not account for Matroska `SeekPreRoll` (some codecs may
    require decode before the target PTS after seeking).
- Opus:
  - Only mapping family 0 mono/stereo is supported today (no multichannel mapping tables).

## Extending the pipeline

The codebase provides a small “narrow waist”:

- demuxers should emit `MediaTrackInfo` + `MediaPacket` with `dts_ns`/`pts_ns`/`duration_ns`,
- decoders should consume `MediaPacket` and emit either:
  - `DecodedAudioChunk` (for audio), or
  - decoded video frames (RGBA/YUV) plus a timestamp,
- paint-facing layers should be non-blocking and read from a cache (`MediaFrameProvider`).

When adding new pieces, keep them deterministic and avoid introducing hard system dependencies into
the default build; prefer optional feature gates when platform libs or external binaries are
required.
