## Media fixtures (video/audio)

This directory is reserved for **small, deterministic media assets** (MP4/WebM, and eventually
audio-only formats) used by FastRender tests and offline fixtures.

Goals:

- **Deterministic CI**: tests must run fully offline with byte-for-byte stable assets committed to
  the repo.
- **CI friendliness**: assets should be tiny (see size budgets below) so cloning and decoding costs
  stay low.
- **Clear provenance**: assets should be generated from synthetic sources (in-house) whenever
  possible to avoid third-party licensing ambiguity.

### What’s currently in the repo

At the moment, there are **no generated “golden” media fixtures** under
`tests/pages/fixtures/assets/media/`.

Some captured offline page fixtures contain **0-byte placeholder MP4 files** named `missing_*.mp4`.
These are not decodable videos; they exist purely to keep offline fixtures hermetic when the
original network video was not fetched during capture:

- `tests/pages/fixtures/berkeley.edu/assets/missing_6cc347e11cda55d8ded4a916f9817b6c.mp4`
- `tests/pages/fixtures/etsy.com/assets/missing_d41b3622d5991c1fa21a69d21886d3a9.mp4`
- `tests/pages/fixtures/etsy.com/assets/missing_e40bbc5aeab99fac2acb7b036fb50dce.mp4`
- `tests/pages/fixtures/gitlab.com/assets/missing_fb2254525735c355e96f2a34f404f758.mp4`
- `tests/pages/fixtures/ikea.com/assets/missing_417341bf664f8ad949380cec423a00c8.mp4`
- `tests/pages/fixtures/ndtv.com/assets/missing_06e1ed3f7d203e45932ea89142e757e5.mp4`
- `tests/pages/fixtures/ndtv.com/assets/missing_3c365773b006ebfc48efdf73012b442a.mp4`
- `tests/pages/fixtures/w3.org/assets/missing_8fb78f2fabeac50177d74ac8a9f7297a.mp4`

When real media decoding/rendering tests are added, their assets should live in this directory.

### Licensing / provenance

Media assets placed in this directory **must** be generated from synthetic sources (e.g. FFmpeg
`lavfi` inputs like `testsrc2`, `color`, `sine`, `anullsrc`) unless there is a strong reason not
to.

- **Provenance**: generated in-house from synthetic sources.
- **Copyright license**: CC0 / Public Domain (intended; we do not claim copyright restrictions on
  generated test patterns).

If you ever need to add a third-party sample file (strongly discouraged), include the upstream
source URL, commit hash/version, and license text in this directory and document it in this README.

### Size budgets

Keep these assets tiny; large binaries slow down CI and make local iteration painful.

- **Target per file**: ≤ **100 KiB**
- **Hard cap per file**: ≤ **250 KiB** (needs justification in PR/commit message)
- **Target total for this directory**: ≤ **1 MiB**

Preferred knobs for small size:

- Duration: **≤ 1s**
- Resolution: **≤ 64×64**
- Frame rate: **≤ 10 fps**
- No audio unless the test explicitly needs it

### How tests should reference these assets

- Prefer **committed binary files** referenced by offline fixtures (e.g. HTML under
  `tests/pages/fixtures/**`) or by integration tests reading from disk.
- Avoid `data:` URLs / base64-embedded video in HTML; it bloats fixture sources and makes changes
  hard to review.
- If a Rust test needs a temporary file, it can embed the bytes via `include_bytes!()` and write
  them to a temp directory at runtime (see how font tests do this under `tests/fixtures/fonts/`).

### Reproducible generation (FFmpeg)

These commands generate *small* synthetic videos with stripped metadata. Exact byte output can vary
by FFmpeg/libx264/libvpx version, so the repo treats the committed binaries as the source of truth.
The commands below are the recommended way to regenerate/update assets when needed.

Prerequisites:

- `ffmpeg`
- `ffprobe` (usually shipped with FFmpeg)

Inspect an existing file:

```bash
ffprobe -hide_banner -show_streams -show_format -of json <file>
```

#### Example: tiny H.264 MP4 (no audio)

```bash
ffmpeg -hide_banner -y \
  -f lavfi -i "testsrc2=size=64x64:rate=10" \
  -t 1 \
  -map_metadata -1 \
  -vf "format=yuv420p" \
  -c:v libx264 -preset veryslow -crf 35 -pix_fmt yuv420p \
  -movflags +faststart \
  out.mp4
```

#### Example: tiny VP8 WebM (no audio)

```bash
ffmpeg -hide_banner -y \
  -f lavfi -i "testsrc2=size=64x64:rate=10" \
  -t 1 \
  -map_metadata -1 \
  -c:v libvpx -crf 40 -b:v 0 \
  out.webm
```

#### Example: tiny VP9+Opus WebM (with audio)

```bash
ffmpeg -hide_banner -y \
  -f lavfi -i "testsrc2=size=64x64:rate=10" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -t 1 -shortest \
  -map_metadata -1 \
  -c:v libvpx-vp9 -crf 40 -b:v 0 \
  -c:a libopus -b:a 24k \
  out_vp9_opus.webm
```

### Update workflow

When adding or updating an asset in this directory:

1. Generate with the recommended FFmpeg commands (or justify deviations).
2. Verify container/codec details with `ffprobe`.
3. Check file sizes (`ls -lh`) and keep within budgets.
4. Update this README’s “What’s currently in the repo” section to list the new files (filename +
   container/codec/duration/resolution).
5. Run the relevant tests/fixture renders that use the asset.
