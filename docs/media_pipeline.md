# Media demux + decode pipeline (developer notes)

This document describes the **intended** media pipeline for `<video>/<audio>` support: how we
demux containers, decode codecs, normalize timestamps, and why specific dependencies were chosen.

FastRender currently treats `<video>` as a *replaced element* for layout/intrinsic sizing purposes
(see `docs/conformance.md`), but full playback requires a dedicated demux/decode stack.

## Design goals / constraints

- **Deterministic, self-contained builds**:
  - MSRV is **Rust 1.70** (`Cargo.toml:6`).
  - **No system dependencies** (no `pkg-config` probes; build C libs from bundled sources when
    needed).
  - **No required assembler** (`nasm`/`yasm`). If a codec library has optional assembly, we must have
    a portable “no-asm” build path so CI/minimal hosts work.
- **Good-enough web compatibility** for the dominant web formats:
  - MP4 (H.264 + AAC)
  - WebM (VP9 + Opus)
- **Seekable local-file playback** as the first milestone (streaming/range requests are future work).

## Pipeline overview

At a high level, playback is a set of adapters that turn “bytes on disk/network” into decoded audio
samples + video frames on a common timeline:

```
bytes (file/http)
  ↓
container demux (MP4/WebM)  ──→  Packet { track, data, pts/dts/dur, keyframe }
  ↓
codec decode (H.264/VP9/AAC/Opus)
  ↓
decoded outputs
  - VideoFrame (YUV/RGB + pts_ns)
  - AudioSamples (f32/i16 + pts_ns)
  ↓
sync + scheduling (nanosecond timeline)
  ↓
renderer (video) + audio backend (audio)
```

The key “glue” is that **everything inside the player uses nanoseconds** (`u64`/`i64`) for
timestamps, regardless of the container’s native timebase.

## Container demux

### MP4 / ISO-BMFF: `mp4parse` + sample tables

We use [`mp4parse`](https://crates.io/crates/mp4parse) (from Firefox’s media stack) to parse the
MP4 box tree and build a random-access sample index using MP4 **sample tables**:

- `stts` (time-to-sample) → decode-time deltas (DTS timeline)
- `ctts` (composition time offset) → presentation-time offsets (PTS timeline)
- `stss` (sync samples) → keyframe/sample sync points for seeking
- `stsc` + `stco`/`co64` + `stsz` → map sample index → byte range in the file

Rationale:

- Pure Rust parsing of MP4 metadata (no system deps).
- Explicit access to the same indexing primitives browsers use (sample tables), which makes
  implementing *accurate seeking* and *timestamp math* straightforward.
- Keeps demuxing separate from decoding so we can swap/extend codec backends without rewriting the
  container layer.

### WebM / Matroska: `matroska-demuxer`

For WebM we use [`matroska-demuxer`](https://crates.io/crates/matroska-demuxer), which provides a
Matroska parser/demuxer suitable for the WebM subset.

We rely on:

- EBML/segment parsing to identify tracks (VP9/Opus).
- `Cluster`/`Block` timecodes for timestamps.
- `Cues` (when present) for seek-to-nearest-keyframe without scanning the entire file.

Rationale:

- Pure Rust, no system deps.
- WebM is structurally different from ISO-BMFF; using a dedicated Matroska parser keeps the code
  simpler than trying to force a “one demuxer to rule them all” abstraction too early.

## Codec decode backends

The pipeline is intentionally “narrow”: it decodes the dominant web codecs with backends that build
reliably on CI/minimal hosts.

### H.264 / AVC: `openh264` (portable, no-asm fallback)

- Backend: [`openh264`](https://crates.io/crates/openh264) + `openh264-sys2`
- Source: Cisco’s OpenH264 library (built from bundled source via `build.rs`).

Why:

- Avoids FFmpeg (size + licensing + build complexity).
- Has a known “no-asm / portable C” build mode, aligning with the repo constraint that `nasm/yasm`
  must not be required.
- Widely deployed decoder with good baseline profile support (the common web case).

Integration notes:

- MP4 stores H.264 as length-prefixed NAL units (`avcC`); many decoders prefer Annex B start codes.
  The demux layer should normalize the access units into the format expected by the decoder.
- Keyframe info comes from MP4’s `stss` (not from parsing slice headers at demux time).

### VP9: bundled `libvpx` (`libvpx-sys-bundled`)

- Backend: `libvpx` via `crates/libvpx-sys-bundled` (vendored source build; **no system `libvpx`**).

Why:

- De-facto reference VP8/VP9 decoder used widely across browsers.
- Can be built from source in-tree, avoiding system `libvpx`/`pkg-config`.
- We configure a **no-asm build** for CI/minimal hosts (no `nasm`/`yasm`) by disabling x86 SIMD
  feature flags and setting `AS=true` to bypass assembler auto-detection. (libvpx's `configure`
  script does **not** provide a `--disable-asm` flag; see `crates/libvpx-sys-bundled/build.rs`.)

Integration notes:

- The initial target is **8-bit 4:2:0** output (the common WebM/VP9 profile).
- Expose decoded frames as planar YUV and convert to RGB(A) in Rust before compositing.

### AAC: `symphonia-codec-aac`

- Backend: [`symphonia-codec-aac`](https://crates.io/crates/symphonia-codec-aac) (Rust AAC decoder).

Why:

- Pure Rust AAC decode → no C toolchain requirements and no system deps.
- We only need the codec implementation; we intentionally do *not* take Symphonia’s container
  demuxers because we want explicit control over MP4/WebM indexing and seeking.

### Opus: `opus` + `audiopus_sys`

- Backend: [`opus`](https://crates.io/crates/opus) (safe wrapper) + `audiopus_sys` (bundled libopus).

Why:

- libopus is the standard Opus implementation; using a bundled build avoids `pkg-config`/system
  packages.
- Works well with WebM’s Opus-in-Matroska mapping.

Integration notes:

- Respect Opus `pre-skip` from `OpusHead` when aligning timestamps.

## Timestamp normalization (nanoseconds)

Containers use different native timebases; the media pipeline normalizes all timestamps to
nanoseconds:

```
ns = (container_ticks * 1_000_000_000) / timebase
```

Key points:

- **MP4**:
  - Each track has a `mdhd` timescale (ticks/second).
  - `stts` yields per-sample decode-time deltas (DTS).
  - `ctts` (if present) yields composition offsets so `PTS = DTS + ctts_offset`.
- **WebM**:
  - `Info.TimecodeScale` is typically in nanoseconds per tick.
  - Block/cluster timecodes are scaled by `TimecodeScale` to compute `pts_ns`.

Normalizing early avoids repeated “what unit is this timestamp?” bugs, and makes:

- scheduling (`now_ns` comparisons),
- A/V sync,
- HTMLMediaElement API (`currentTime` in seconds ↔︎ `pts_ns`)

consistent across all backends.

## Seeking model

Seeking is fundamentally “seek to a keyframe, then decode forward” for inter-frame codecs.

Proposed behavior:

1. Convert seek target seconds → `target_ns`.
2. Choose a **seek anchor track** (typically the video track).
3. Find the nearest **sync sample / keyframe** at or before `target_ns`.
4. Seek the underlying byte stream to the container byte offset for that keyframe.
5. Reset demux + decoders and decode forward:
   - Drop decoded frames/samples with timestamps `< target_ns`.
   - Present the first frame at/after `target_ns`.

Container specifics:

- **MP4**:
  - Use `stts`/`ctts` to map `target_ns` → sample index.
  - Use `stss` to back up to the nearest sync sample.
  - Use `stsc`/`stco`/`stsz` to compute byte offsets.
- **WebM**:
  - Prefer `Cues` to find the cluster position for a keyframe near `target_ns`.
  - If cues are missing, fall back to scanning clusters (slow, but correct for local files).

## Known limitations / TODOs

These are expected gaps in an initial implementation and should be tracked explicitly:

- **MP4 edit lists** (`edts`/`elst`) are not applied yet (timeline offset/trimming).
- **HDR / 10-bit VP9** (Profile 2, BT.2020/PQ/HLG) is not supported in the first pass; target is
  8-bit SDR output.
- **Encrypted streams** (CENC in MP4, Matroska encryption) are not supported.
- **Streaming / range requests**: initial demux assumes a seekable byte source (local file or fully
  buffered resource), not incremental HTTP range fetching.

## Extending the pipeline

The stack is designed to be extensible without taking on FFmpeg:

- **Add a container**:
  - Implement a demuxer that can (a) enumerate tracks, (b) yield timestamped packets, and (c) seek
    to a keyframe near a target timestamp.
  - Ensure the demuxer produces **nanosecond timestamps** and an `is_keyframe` signal (from the
    container’s indexing metadata when possible).
- **Add a codec**:
  - Implement a decoder that consumes `Packet` and yields decoded audio/video on the same ns
    timeline.
  - Keep decoder APIs independent of the container; container-specific “extradata” should be
    normalized at the demux boundary (e.g. `avcC` → Annex B, OpusHead parsing, etc.).
