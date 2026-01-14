# Media pipeline (demux + decode) — status + developer notes

This document tracks the **current in-tree media pipeline** (under `src/media/`) and the intended
direction for `<video>/<audio>` playback.

FastRender currently treats `<video>` as a *replaced element* for layout/intrinsic sizing purposes
(see `docs/conformance.md`). Full playback requires:

- a demux/decode stack (containers + codecs), and
- a concrete `MediaFrameProvider` implementation to feed decoded frames into paint.

Most building blocks exist (demuxers, codec decoders, decode pipeline, frame provider), but the
DOM/`HTMLMediaElement` playback loop is still being wired up.

For the intended clocking model (audio as master clock; UI ticks are wake-ups only), see
`docs/media_clocking.md`.

## Implementation status (repo reality)

Legend: ✅ implemented, ⚠️ partial, 🚧 planned, ❌ missing.

| Area | Status | Notes / code |
| --- | --- | --- |
| Common types (`MediaTrackInfo`, `MediaPacket`, `MediaData`, `Decoded*`) | ✅ | [`src/media/mod.rs`](../src/media/mod.rs), [`src/media/packet.rs`](../src/media/packet.rs) |
| Decode pipeline (`MediaDecodePipeline`) | ✅ | [`src/media/pipeline.rs`](../src/media/pipeline.rs) (demux→decode wiring; yields `DecodedItem`) |
| WebM demux (`WebmDemuxer`) | ✅ | [`src/media/demux/webm.rs`](../src/media/demux/webm.rs) (feature: `media_webm`/`media`; VP9+Opus; track selection/filtering; codec delay; seek; optional inter-track ordering; rejects encrypted/compressed `ContentEncodings`) |
| MP4 demux + packetizer (`Mp4PacketDemuxer`) | ⚠️ | [`src/media/demuxer.rs`](../src/media/demuxer.rs) (feature: `media_mp4`/`media`; H.264+AAC; best-effort VP9 detection via `mp4parse`; rejects encrypted/protected tracks; best-effort mp4parse-derived DTS/PTS/duration + keyframe seek; falls back to mp4-crate timestamps when sample tables are unavailable) |
| MP4 demux (pure-Rust box parser): `demux::mp4::Mp4Demuxer` | ✅ | [`src/media/demux/mp4.rs`](../src/media/demux/mp4.rs) (in-memory; produces `MediaData::Shared`; parses `avcC`→H.264 extradata + `esds`→AAC ASC; not currently wired into `MediaDecodePipeline` via `MediaDemuxer`) |
| MP4 demux (mp4parse sample-table): `demux::mp4parse::Mp4ParseDemuxer` | ⚠️ | [`src/media/demux/mp4parse.rs`](../src/media/demux/mp4parse.rs) (reader-based sample-table demux; timestamping works; codec `codec_private` extraction is currently incomplete) |
| MP4 sample-table utilities (`Mp4Demuxer`, `Mp4SeekIndex`) | ✅ | [`src/media/mp4.rs`](../src/media/mp4.rs) (feature: `media_mp4`/`media`; `ctts`-aware PTS/DTS computation; currently separate from `Mp4PacketDemuxer`) |
| AAC decoder | ✅ | [`src/media/codecs/aac.rs`](../src/media/codecs/aac.rs) (feature: `codec_aac`/`media`; Symphonia → `DecodedAudioChunk`) |
| Opus decoder | ✅ | [`src/media/codecs/opus.rs`](../src/media/codecs/opus.rs) (feature: `codec_opus`/`media`; `opus` crate / libopus; mapping family 0 mono/stereo only today) |
| H.264 decoder | ✅ | [`src/media/decoder.rs`](../src/media/decoder.rs) (feature: `codec_h264_openh264`/`media`; OpenH264; MP4 length-prefixed NALs → Annex B) |
| VP9 decode (libvpx) | ✅ | [`src/media/decoder.rs`](../src/media/decoder.rs) (feature: `codec_vp9_libvpx`/`media`) → [`src/media/codecs/vp9.rs`](../src/media/codecs/vp9.rs) → [`crates/libvpx-sys-bundled`](../crates/libvpx-sys-bundled) |
| Media backends (`MediaBackend`) | ✅ | Native: [`src/media/backends/native.rs`](../src/media/backends/native.rs); optional CLI fallback: [`src/media/backends/ffmpeg_cli.rs`](../src/media/backends/ffmpeg_cli.rs) behind `media_ffmpeg_cli` |
| `<video>` paint hook + frame caching | ⚠️ | Paint can query a `MediaFrameProvider` (`src/paint/display_list_builder.rs`); `SizeHintMediaFrameProvider` exists ([`src/media/frame_provider.rs`](../src/media/frame_provider.rs)), but no full HTMLMediaElement playback loop is wired yet |
| A/V sync helper | ✅ | [`src/media/av_sync.rs`](../src/media/av_sync.rs) (+ env overrides) |
| Audio output plumbing | ✅ (not wired to HTMLMediaElement yet) | [`src/media/audio/`](../src/media/audio/) (real output via `audio_cpal`; null backend is default) |

## Design goals / constraints (current)

- **MSRV**: Rust **1.70** (`Cargo.toml:6`).
- **Feature-gated media stack**:
  - Media support is intentionally behind `--features media` (or sub-features like `media_mp4`,
    `codec_vp9_libvpx`, etc) to keep default builds lean.
  - The windowed browser UI enables `media` via `browser_ui_base` (`Cargo.toml`).
- **CI-friendly by default**:
  - Native playback avoids *system* FFmpeg/GStreamer dependencies.
  - Codec/container features pull in C build dependencies (OpenH264/libvpx/libopus); optional
    `media_ffmpeg_cli` can be used as a fallback if native codecs are unavailable.
  - Optional features may require extra dependencies:
    - `browser_ui` (windowed browser): GUI dev packages on Linux (see `docs/browser_ui.md`).
    - `audio_cpal`: real audio output; may require ALSA headers on Linux.
    - `media_ffmpeg_cli`: requires `ffmpeg`/`ffprobe` binaries on PATH.
- **No required assembler in the default media build**:
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
MediaSession (today: MediaDecodePipeline)
  ↓
container demux
  - WebM: WebmDemuxer        ✅
  - MP4:  Mp4PacketDemuxer   ⚠️ (best-effort sample tables; see below)
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

Implementation: [`src/media/demux/webm.rs`](../src/media/demux/webm.rs) using
[`matroska-demuxer`](https://crates.io/crates/matroska-demuxer).

Current behavior:

- Opens any `R: Read + Seek`.
- Enumerates tracks as `MediaTrackInfo` (codec + codec_private bytes + codec_delay_ns).
- Rejects unsupported Matroska `ContentEncodings` (encryption/compression) up-front with an explicit
  `MediaError::Unsupported` (no DRM/EME and no Matroska content compression support today).
- Emits `MediaPacket` **only** for:
  - VP9 (`codec_id = "V_VP9"`)
  - Opus (`codec_id = "A_OPUS"`)
- Safety: individual encoded packets are rejected if they exceed a hard cap (currently **64 MiB**;
  see `MAX_WEBM_PACKET_BYTES` in `src/media/demux/webm.rs`) to avoid unbounded memory usage on
  corrupted/adversarial files.
- Track selection/filtering:
  - Track metadata is used to pick “primary” audio/video tracks (see
    [`src/media/track_selection.rs`](../src/media/track_selection.rs)).
  - `WebmDemuxerOptions.track_filter` controls whether packets are emitted for only the primary
    tracks or for all supported tracks.
- Timestamp normalization:
  - Uses `Info.TimecodeScale` (nanoseconds per tick) to compute `pts_ns`.
  - Subtracts Matroska `TrackEntry.codec_delay` from timestamps (per spec).
  - `dts_ns` is currently set equal to `pts_ns` on this path.
- Optional inter-track ordering:
  - When enabled (`WebmDemuxerOptions.inter_track_reordering = true`), `next_packet()` yields
    non-decreasing PTS across tracks using a small bounded queue per track.
- Seeking:
  - `WebmDemuxer::seek(time_ns)` uses `MatroskaFile::seek(...)` and compensates for codec delay.
  - In damaged/unindexed files, seeking may return
    `MediaError::Unsupported("Matroska seek unsupported (no cluster index)")`.

### MP4 / ISO-BMFF: `Mp4PacketDemuxer` (implemented; best-effort sample tables)

Implementation: [`src/media/demuxer.rs`](../src/media/demuxer.rs) (`Mp4PacketDemuxer`), built on the
[`mp4`](https://crates.io/crates/mp4) crate plus `mp4parse` metadata.

Current behavior:

- Opens MP4 either from a file (`Mp4PacketDemuxer::open(path)`) or in-memory bytes
  (`Mp4PacketDemuxer::from_bytes(Arc<[u8]>)`).
- Rejects encrypted/protected tracks up-front using mp4parse metadata (no DRM/EME support today).
- Track detection:
  - H.264 (`mp4::MediaType::H264`) → emits packets
  - AAC (`mp4::MediaType::AAC`) → emits packets
  - VP9: best-effort detection by parsing `stsd` via `mp4parse` (because `mp4` does not currently
    expose VP9 via `Mp4Track::media_type()`), then emits packets.
- Timestamping:
  - When mp4parse sample tables are available, the demuxer attaches `dts_ns`, `pts_ns`, and
    `duration_ns` (including `ctts` reordering), and uses `dts_ns` to pick the next packet across
    tracks.
  - When sample tables are not available (e.g. skipped due to caps), it falls back to
    `mp4::Sample.start_time` for `pts_ns` (with `dts_ns == pts_ns` and `duration_ns == 0`).

Codec-private (`MediaTrackInfo.codec_private`) formats produced today:

- **H.264**: a minimal custom format derived from `avcC`, used by `decoder::H264Decoder`:

  ```text
  u8  nal_length_size
  u8  sps_count
  [sps_count] { u16be len, [len] bytes }
  u8  pps_count
  [pps_count] { u16be len, [len] bytes }
  ```

- **AAC**:
  - Prefer `esds`/decoder-specific bytes extracted via mp4parse (an `AudioSpecificConfig` blob).
  - Fall back to synthesizing a minimal **AAC-LC** `AudioSpecificConfig` when mp4parse data is
    unavailable.
- **VP9**: a compact subset of `vpcC` (bit depth / primaries / subsampling + `codec_init` bytes).

Seeking:

- With mp4parse sample tables:
  - `seek(time_ns)` finds the first sample with `pts_ns >= time_ns`.
  - For video tracks, it backs up to a sync sample at-or-before that point (best-effort keyframe seek).
- Without sample tables: falls back to a linear scan.

Known limitations / gaps:

- MP4 sample-table construction is intentionally capped (see `MAX_SAMPLES_PER_TRACK` /
  `MAX_TOTAL_SAMPLES` in `src/media/demuxer.rs`) so corrupted files can’t force unbounded allocations.
  When caps are hit, the demuxer falls back to the mp4-crate timestamp path (reduced timestamp/seek
  fidelity).
- **Large sample allocations**: the `mp4` crate returns sample bytes as owned buffers; it may
  allocate attacker-controlled sample sizes before we can enforce a hard cap. Fixing this likely
  requires a zero-copy demux path and/or pre-size checks (tracked as part of broader MP4 correctness
  work).
- Fragmented MP4 (`moof`/`mdat`) is not supported by this demuxer.

Other MP4 demuxers in-tree (not currently used by `NativeBackend`/`MediaDecodePipeline`):

- [`src/media/demux/mp4.rs`](../src/media/demux/mp4.rs): a pure-Rust MP4 box parser/demuxer that:
  - reads the full file into an `Arc<[u8]>`,
  - emits `MediaPacket` with `MediaData::Shared` ranges,
  - computes both `dts_ns` and `pts_ns` (including `ctts`) and a non-monotonic-PTS seek index, and
  - parses `avcC` into the custom H.264 extradata format expected by `decoder::H264Decoder`, plus
    `esds`→AAC `AudioSpecificConfig`.
- [`src/media/demux/mp4parse.rs`](../src/media/demux/mp4parse.rs): a demuxer built on mp4parse sample
  tables that can read sample bytes from a `Read+Seek` source, but is still missing full codec
  config (`codec_private`) extraction for the decode pipeline.

## Codec decode backends

### AAC (implemented): `AacDecoder` (symphonia)

Implementation: [`src/media/codecs/aac.rs`](../src/media/codecs/aac.rs) (feature: `codec_aac`).

Input contract:

- The demux layer must provide:
  - AAC access-unit bytes (as `MediaPacket.data`)
  - AAC `AudioSpecificConfig` (ASC) bytes (container-provided) for `AacDecoder::new(...)`

Output:

- `DecodedAudioChunk` with interleaved `f32` samples in `[-1.0, 1.0]`, plus `pts_ns`/`duration_ns`.

### Opus (implemented): `OpusDecoder` (libopus via the `opus` crate)

Implementation: [`src/media/codecs/opus.rs`](../src/media/codecs/opus.rs) (feature: `codec_opus`).

- Uses the `opus` crate (libopus).
- Expects Matroska/WebM `codec_private` bytes to start with an `OpusHead` header (RFC7845).
- Applies `pre_skip` trimming so initial decoder priming samples are dropped.
- Output is always **48 kHz** (Opus internal sample clock).

Current limitations:

- Only **channel mapping family 0** is supported.
- Only **mono/stereo** streams are supported (`channels` must be 1 or 2).

### H.264 / AVC (implemented): `H264Decoder` (OpenH264)

Implementation: [`src/media/decoder.rs`](../src/media/decoder.rs) (feature: `codec_h264_openh264`).

Input contract:

- `MediaTrackInfo.codec_private` must be in the custom `avcC`-derived format documented in
  `parse_h264_codec_private(...)` (see source for the exact layout).
- `MediaPacket.data` is expected to contain MP4/AVC **length-prefixed** NAL units (not Annex B start
  codes). The decoder converts packets to Annex B and prepends SPS/PPS before the first decode.

Output:

- `DecodedVideoFrame` with RGBA8 pixels (OpenH264 decodes to YUV and the code converts to RGBA).

### VP9 (implemented): bundled libvpx

Implementation lives in:

- Workspace crate: [`crates/libvpx-sys-bundled`](../crates/libvpx-sys-bundled) (vendored libvpx build
  + wrapper)
- Media wrapper: [`src/media/codecs/vp9.rs`](../src/media/codecs/vp9.rs) (`codecs::vp9::Vp9Decoder`
  → RGBA8 frames)

Current status:

- `WebmDemuxer` can emit VP9 packets.
- `MediaDecodePipeline` uses `src/media/decoder.rs::create_video_decoder` to construct a libvpx-backed
  `codecs::vp9::Vp9Decoder` (feature: `codec_vp9_libvpx` or `media`).
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
  - currently `dts_ns == pts_ns`
- **MP4** (`Mp4PacketDemuxer`):
  - Best-effort: uses mp4parse sample tables (`create_sample_table`) when available, including `ctts`
    reordering and per-sample duration.
  - Fallback: uses `mp4::Sample.start_time` as both PTS+DTS (`dts_ns == pts_ns`) and sets
    `duration_ns == 0`.

Clocking/scheduling code uses `Duration` (`src/media/clock.rs`, `src/media/av_sync.rs`) but the unit
is still nanoseconds.

## Seeking model (current behavior)

- **WebM**: `WebmDemuxer::seek(time_ns)` seeks to the first frame at/after the target (after
  compensating for codec delay).
- **MP4**:
  - With mp4parse sample tables, `Mp4PacketDemuxer::seek(time_ns)` seeks each track to the first
    sample with `pts_ns >= time_ns` and (for video) backs up to a sync sample at-or-before.
  - Without sample tables, seeking falls back to a linear scan.

## How to manually test (fixtures)

The repo contains tiny, offline MP4/WebM fixtures and matching HTML pages:

- Raw media assets: [`tests/fixtures/media/`](../tests/fixtures/media/)
- Playback pages + assets: [`tests/pages/fixtures/media_playback/`](../tests/pages/fixtures/media_playback/)
  (assets live in `tests/pages/fixtures/media_playback/assets/`)
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
# Index page (links to the rest):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- \
    "file://$PWD/tests/pages/fixtures/media_playback/index.html"

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

# JS controls + event logging:
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- \
    "file://$PWD/tests/pages/fixtures/media_playback/js_controls.html"
```

Useful runtime toggles while debugging:

- Paint backend selection:
  - `FASTR_PAINT_BACKEND=display_list|legacy` (default: `display_list`; see `docs/env-vars.md`).
- Video A/V sync tolerances (used by `src/media/av_sync.rs`):
  - `FASTR_AV_SYNC_TOLERANCE_MS`
  - `FASTR_AV_SYNC_MAX_LATE_MS`
  - `FASTR_AV_SYNC_MAX_EARLY_MS`

Note: full end-to-end decode→paint→DOM integration is still in progress. Today these pages are
primarily a smoke test for `<video>/<audio>` layout and for future playback wiring.

## Known limitations / TODOs (explicit)

- There is no end-to-end `HTMLMediaElement` playback engine yet (DOM events/state machine, decode
  scheduling threads, audio output as master clock, etc).
  - Paint *can* display frames if an app supplies a `MediaFrameProvider`, but nothing in-tree wires
    `MediaDecodePipeline`/`MediaPlayer` to the DOM yet.
  - `MediaFrameProvider::audio_frame` is still a stub (`src/media/mod.rs`).
- MP4 (`Mp4PacketDemuxer`):
  - Sample-table timing/seek is best-effort and can fall back when caps are hit (reduced
    timestamp/seek fidelity).
  - Fragmented MP4 is unsupported.
- WebM (`WebmDemuxer`):
  - Seek is best-effort and currently does not account for Matroska `SeekPreRoll` (some codecs may
    require decode before the target PTS after seeking).
- Opus:
  - Only mapping family 0 mono/stereo is supported today (no multichannel mapping tables).
- Audio output:
  - Real audio output is feature-gated (`audio_cpal`) and not yet routed from the media decode
    pipeline into the audio engine/backends.

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
