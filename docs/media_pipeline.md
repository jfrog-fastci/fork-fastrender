# Media pipeline (demux + decode) — status + developer notes

This document tracks the **current implementation status** of the media pipeline (under
`src/media/`) and the intended direction for `<video>/<audio>` playback.

FastRender currently treats `<video>` as a *replaced element* for layout/intrinsic sizing purposes
(see `docs/conformance.md`). Full playback requires a dedicated demux/decode stack plus a concrete
`MediaFrameProvider` implementation to feed decoded frames into paint.

For the intended clocking model (audio as master clock; UI ticks are wake-ups only), see
`docs/media_clocking.md`.

## Implementation status (repo reality)

Legend: ✅ implemented, ⚠️ partial, 🚧 planned, ❌ missing.

| Area | Status | Notes / code |
| --- | --- | --- |
| Common types (`MediaTrackInfo`, `MediaPacket`, `DecodedAudioChunk`) | ✅ | `src/media/mod.rs` |
| WebM demux (`WebmDemuxer`) | ✅ | `src/media/demux/webm.rs` (VP9+Opus packets only; seek; optional inter-track PTS reordering) |
| MP4 demux/index (`Mp4Demuxer`) | ⚠️ | `src/media/mp4.rs` parses sample tables + builds a PTS seek index, but does **not** emit `MediaPacket` yet |
| MP4 “packetizer” (sample bytes → `MediaPacket`) | 🚧 | Not implemented yet (would read `Mp4Sample { offset, size }` ranges and parse codec config) |
| AAC decoder | ✅ | `src/media/codecs/aac.rs` (symphonia AAC decoder → `DecodedAudioChunk`) |
| VP9 decoder backend | ⚠️ | Available as a standalone crate: `crates/libvpx-sys-bundled` (`Vp9Decoder` helper), not yet wired into `src/media` |
| Opus decoder | ❌ | Not implemented (no Opus codec crate wired into `Cargo.toml` yet) |
| H.264 decoder | ❌ | Not implemented (no H.264 codec crate wired into `Cargo.toml` yet) |
| A/V sync policy helper | ✅ | `src/media/av_sync.rs` (+ env overrides) |
| Audio output plumbing | ✅ | `src/media/audio/*` (real output via `audio_cpal` feature; null backend is default) |
| `<video>` paint integration | ⚠️ | Paint can query a `MediaFrameProvider`, but the default provider is `NullMediaFrameProvider` (no actual playback yet) |

## Design goals / constraints (current)

- **MSRV**: Rust **1.70** (`Cargo.toml:6`).
- **CI-friendly by default**:
  - Core media plumbing is pure Rust today (`matroska-demuxer`, Symphonia AAC).
  - Optional features may require system dependencies:
    - `browser_ui` (windowed browser): needs GUI dev packages on Linux (see `docs/browser_ui.md`).
    - `audio_cpal`: real audio output; may require ALSA headers on Linux.
- **No required assembler in the default build**:
  - The repo includes a bundled libvpx crate (`crates/libvpx-sys-bundled`) intended to avoid
    `nasm`/`yasm` on common targets by using a portable C-only build.
  - Some targets (notably `x86_64-pc-windows-msvc`) can still require extra tools; see
    `crates/libvpx-sys-bundled/README.md`.
- **First milestone: seekable local-file playback** (streaming/range requests are future work):
  - `WebmDemuxer` requires `Read + Seek`.
  - `Mp4Demuxer` currently parses from an in-memory `&[u8]`.

## Pipeline overview (target shape)

At a high level, playback is a set of adapters that turn “bytes on disk/network” into decoded audio
samples + video frames on a common timeline:

```
bytes (file/http)
  ↓
container demux
  - WebM: WebmDemuxer  ✅
  - MP4:  Mp4Demuxer   ⚠️ (index only; packetizer TODO)
  ↓
MediaPacket { track_id, data, pts_ns, duration_ns, is_keyframe }
  ↓
codec decode
  - AAC ✅  (DecodedAudioChunk)
  - VP9 ⚠️  (backend exists; integration TODO)
  - Opus ❌
  - H.264 ❌
  ↓
decoded outputs
  - audio: DecodedAudioChunk (f32 interleaved, pts_ns)
  - video: (planned) decoded frames → paint-facing ImageData
  ↓
sync + scheduling (Duration / nanosecond timeline)
  ↓
paint (video) + audio backend (audio)
```

Internally we normalize timestamps into **nanoseconds** at the demux boundary (`MediaPacket.pts_ns`,
`MediaPacket.duration_ns`). Higher layers often use `std::time::Duration` (which is still
nanosecond-resolution) for clocking and scheduling.

## Container demux

### WebM / Matroska: `WebmDemuxer` (implemented)

Implementation: `src/media/demux/webm.rs` using [`matroska-demuxer`](https://crates.io/crates/matroska-demuxer).

Current behavior:

- Opens any `R: Read + Seek`.
- Enumerates tracks as `MediaTrackInfo` (codec + codec_private bytes + codec_delay).
- Emits `MediaPacket` **only** for:
  - VP9 (`codec_id = "V_VP9"`)
  - Opus (`codec_id = "A_OPUS"`)
  - Other track types/codecs are currently surfaced in `tracks()` but skipped by `next_packet()`.
- Timestamp normalization:
  - Uses `Info.TimecodeScale` (nanoseconds per tick) to compute `pts_ns`.
  - Subtracts Matroska `TrackEntry.codec_delay` from timestamps (per spec).
- Optional inter-track reordering:
  - Enabled by default (`WebmDemuxerOptions::default().inter_track_reordering = true`).
  - Ensures `next_packet()` returns **non-decreasing PTS across tracks** using a small bounded queue
    per track.
- Seeking:
  - `WebmDemuxer::seek(time_ns)` uses `MatroskaFile::seek(...)`.
  - In damaged/unindexed files, seeking may return
    `MediaError::Unsupported("Matroska seek unsupported (no cluster index)")`.

### MP4 / ISO-BMFF: `Mp4Demuxer` (partial; index + seek helper only)

Implementation: `src/media/mp4.rs`.

Despite the name, this is currently best thought of as an **MP4 sample-table parser + seek index**
for non-fragmented MP4s:

- Parses the `moov` box and a minimal subset of sample tables:
  - `mdhd` (timescale)
  - `stts` (decode-time deltas)
  - `ctts` (composition offsets; version 0/1)
  - `stsc` + `stco`/`co64` + `stsz` (sample byte ranges)
  - `stss` (sync sample list) → populates `Mp4Sample.is_sync`
- Builds per-track sample metadata in decode order (`Mp4Sample { offset, size, dts_ticks, duration_ticks, is_sync }`).
- Computes per-sample **PTS in nanoseconds** (`pts_ns_by_sample`) and builds a seek index:
  - Monotonic PTS → binary search directly.
  - Non-monotonic PTS (B-frame style reordering via `ctts`) → binary search over a sorted `(pts, sample_index)` table.
- `Mp4Demuxer::seek(time_ns)` seeks all tracks to the first sample with `pts_ns >= time_ns` and
  records which seek strategy was used (`SeekMethod`).

What is **not** implemented yet:

- No `MediaPacket` emission (no “read sample bytes” iterator).
- No track typing / codec detection in this module (it does not currently parse `hdlr`, `stsd`,
  `avcC`, `esds`, etc).
- No “seek to previous keyframe” behavior yet (even though `is_sync` is available).
- No fragmented MP4 (`moof`/`mdat`) support.

Note: `symphonia-format-isomp4` is present as a **dev-dependency** and is used in unit tests for the
AAC decoder. It is not currently used as the production MP4 demuxer.

## Codec decode backends

### AAC (implemented): `AacDecoder`

Implementation: `src/media/codecs/aac.rs` using:

- `symphonia-core`
- `symphonia-codec-aac`

Input contract:

- The demux layer must provide:
  - AAC access-unit bytes (as `MediaPacket.data`)
  - AudioSpecificConfig extradata (container-provided) for `AacDecoder::new(...)`

Output:

- `DecodedAudioChunk` with interleaved `f32` samples in `[-1.0, 1.0]`, plus `pts_ns`/`duration_ns`.

### VP9 (backend exists, integration TODO): bundled libvpx

Implementation lives in the workspace crate `crates/libvpx-sys-bundled`:

- Builds **vendored libvpx** from source.
- Includes an experimental `Vp9Decoder` helper (`crates/libvpx-sys-bundled/src/vp9_decoder.rs`)
  that can decode to RGBA8.

Current status in the main pipeline:

- `WebmDemuxer` can emit VP9 packets.
- There is not yet glue code in `src/media/` that turns those packets into paint-ready frames.

Build notes:

- Requires a C toolchain and GNU make.
- Aims to avoid `nasm`/`yasm` by disabling x86 SIMD and forcing a portable build; see
  `crates/libvpx-sys-bundled/build.rs` and the crate README for target-specific caveats.

### Opus (missing)

There is currently no Opus decoder wired into `Cargo.toml`. `WebmDemuxer` already exposes the
necessary track metadata (`codec_private` and `codec_delay_ns`) for future Opus integration.

### H.264 / AVC (missing)

There is currently no H.264 decoder wired into `Cargo.toml`. The committed MP4 fixture is H.264 +
AAC, so “MP4 playback” requires adding:

- an H.264 decoder backend, and
- MP4 packetization + codec-config parsing (e.g. `avcC` NAL length prefixes → Annex B start codes, if
  the chosen decoder requires Annex B).

## Timestamp normalization (nanoseconds)

The demux boundary normalizes timestamps into nanoseconds (`MediaPacket.{pts_ns,duration_ns}`).

Current implementations:

- **WebM** (`WebmDemuxer`):
  - `pts_ns = frame.timestamp * Info.TimecodeScale`
  - subtracts `TrackEntry.codec_delay`
- **MP4** (`Mp4Demuxer`):
  - `DTS` is built from `stts` deltas.
  - `PTS` is derived from `DTS + ctts_offset`.
  - Converted to ns using the track `mdhd` timescale with rounding + saturation.

Clocking/scheduling code uses `Duration` (`src/media/clock.rs`, `src/media/av_sync.rs`) but the
unit is still nanoseconds.

## Seeking model (current behavior)

- **WebM**: `WebmDemuxer::seek(time_ns)` seeks to the first frame at/after the target (after
  compensating for codec delay).
- **MP4**: `Mp4Demuxer::seek(time_ns)` selects the first sample with `pts_ns >= time_ns` using a
  binary search index. It does not yet back up to a sync sample/keyframe.

## How to manually test (fixtures)

The repo contains tiny, offline MP4/WebM fixtures and matching HTML pages under `tests/pages/fixtures/`.

Run the windowed browser UI (requires the `browser_ui` feature; see `docs/browser_ui.md` for platform
prereqs):

```bash
# Recommended (applies resource limits):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser
```

Then open these fixture pages (via address bar, or pass as the initial URL argument):

```bash
# MP4 (H.264 + AAC):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- \
    "file://$PWD/tests/pages/fixtures/media_mp4_basic/index.html"

# WebM (VP9 + Opus):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- \
    "file://$PWD/tests/pages/fixtures/media_webm_basic/index.html"
```

Useful runtime toggles while debugging:

- Paint backend selection:
  - `FASTR_PAINT_BACKEND=display_list|legacy` (default: `display_list`; see `docs/env-vars.md`).
- Video A/V sync tolerances (used by `src/media/av_sync.rs`):
  - `FASTR_AV_SYNC_TOLERANCE_MS`
  - `FASTR_AV_SYNC_MAX_LATE_MS`
  - `FASTR_AV_SYNC_MAX_EARLY_MS`

Note: full decode→paint integration is still in progress; today these pages are primarily a smoke
test for `<video>` layout and for future playback wiring.

## Known limitations / TODOs (explicit)

- No end-to-end playback engine is wired into the DOM/paint pipeline yet (`MediaFrameProvider` has no
  real implementation).
- MP4:
  - sample-table parsing exists, but sample packetization is not implemented.
  - codec config parsing (`stsd`/`avcC`/`esds`) is not implemented.
  - keyframe-aware seek is not implemented (even though `stss` is parsed into `Mp4Sample.is_sync`).
- WebM:
  - demux emits VP9+Opus packets only; decode integration is still pending.
- Codecs:
  - AAC decode exists.
  - VP9 decode backend exists as a standalone crate but is not wired into `src/media`.
  - No Opus or H.264 decoder is integrated yet.

## Extending the pipeline

The current codebase provides a small “narrow waist”:

- demuxers should emit `MediaTrackInfo` + `MediaPacket` with `pts_ns`/`duration_ns`,
- decoders should consume `MediaPacket` and emit either:
  - `DecodedAudioChunk` (for audio), or
  - paint-ready frame data (for video), plus a timestamp.

When adding new pieces, keep them deterministic and avoid introducing hard system dependencies into
the default build; prefer optional feature gates when platform libs are required.
