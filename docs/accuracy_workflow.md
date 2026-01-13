# Accuracy Workflow (how to make pages render correctly)

This document describes the systematic workflow for improving rendering accuracy on pageset pages.

## Prerequisites

```bash
# Ubuntu one-time setup (python + fonts + chrome/chromium):
scripts/install_chrome_baseline_deps_ubuntu.sh
```

## The accuracy triage loop

### Step 1: Identify the problem

Run the pageset and check the scoreboard:

```bash
# Run the main pageset loop
timeout -k 10 900 bash scripts/cargo_agent.sh xtask pageset

# Inspect results
timeout -k 10 60 bash scripts/cargo_agent.sh run --release --bin pageset_progress -- report --top 15
```

Look at `progress/pages/*.json` for:
- `status`: "ok", "timeout", "panic", "error"
- `stages_ms`: Which stage is slow?
- `hotspot`: Where is time being spent?
- `notes`: Known issues

### Step 2: Reproduce from cache

```bash
# Ensure cached HTML exists
timeout -k 10 300 bash scripts/cargo_agent.sh run --release --bin fetch_pages

# Render one page
timeout -k 10 300 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin render_pages -- --pages example.com
```

The cached HTML is at `fetches/html/<stem>.html`. Use a fixed viewport/DPR for reproducibility.

### Step 3: Get a correct-engine baseline

```bash
# Generate Chrome baseline from cached HTML (JS disabled)
scripts/chrome_baseline.sh example.com

# Output: fetches/chrome_renders/<stem>.png
```

### Step 4: Compare and diff

```bash
# Generate diff report
timeout -k 10 300 bash scripts/cargo_agent.sh run --release --bin diff_renders -- \
  --before fetches/chrome_renders \
  --after fetches/renders \
  --html target/chrome_vs_fastrender/report.html

# Or use the wrapper script
scripts/chrome_vs_fastrender.sh --pages example.com
```

### Step 5: Classify the error

Look at the diff and identify the root cause category:

| Category | Symptoms | Where to look |
|----------|----------|---------------|
| **Missing content / wrong visibility** | Elements not rendered, wrong `display` | Box tree generation, `display: none/contents` |
| **Wrong layout geometry** | Wrong positions, sizes, overlap | Block/inline/flex/grid/table/positioned contexts |
| **Wrong stacking/clip** | Overlay issues, clipping problems | Stacking contexts, z-index, overflow, clip-path |
| **Text rendering issues** | Wrong fonts, metrics, breaks, alignment | Font fallback, shaping, line breaking, bidi |
| **Image/replaced sizing** | Wrong image sizes, object-fit issues | Replaced element sizing, srcset selection |
| **Resource failures** | Missing CSS, images, fonts | Resource fetching, base URL, @import chains |

### Step 6: Create a deterministic repro

**Preferred**: Create an offline fixture that demonstrates the bug.

```bash
# Capture a self-contained bundle (network):
timeout -k 10 300 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin bundle_page -- \
  fetch https://example.com --out /tmp/capture.tar

# Or capture from warmed pageset caches (offline; requires disk_cache):
timeout -k 10 300 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features disk_cache --bin bundle_page -- \
  cache example.com --out /tmp/capture.tar

# Import the bundle into an offline fixture under tests/pages/fixtures/:
timeout -k 10 300 bash scripts/cargo_agent.sh xtask import-page-fixture /tmp/capture.tar my_fixture

# Media sources are placeholder-only by default (fixtures stay small). Opt in to vendoring playable
# media with --include-media, subject to size budgets (--media-max-bytes default 5 MiB total,
# --media-max-file-bytes default 2 MiB per file; set either to 0 to disable).
#
# Note: if you need media bytes inside the bundle (not just placeholder filenames), capture with
# `bundle_page fetch --no-render/--crawl --prefetch-media` so crawl discovery picks up
# `<video>/<audio>/<source>/<track>` URLs and downloads them into the bundle (subject to
# `--prefetch-media-max-bytes` / `--prefetch-media-max-total-bytes`).
```

Fixtures should be:
- **Minimal** — Remove unnecessary content
- **Offline** — No network dependencies
- **Deterministic** — Use bundled fonts
- **Documented** — Comment what the fixture tests

### Step 7: Implement the fix

1. **Read the spec** — CSS 2.1, Selectors, CSS Values, Positioning, Flexbox, Grid, Painting
2. **Implement correctly** — No hacks, no magic constants, no page-specific code
3. **Keep it small** — Target the specific constraint causing the issue

### Step 8: Add a regression test

Add a test in the appropriate location:

| Test type | Location | When to use |
|-----------|----------|-------------|
| Unit test (preferred) | `src/layout/**`, `src/paint/**`, `src/style/**` (in `#[cfg(test)] mod tests { ... }`) | Most correctness fixes (layout geometry, painting order, style computation) |
| Public API integration test | `tests/api/**` (a module of `tests/integration.rs`) | End-to-end behavior through the public `FastRender` API |
| Fixture test | `tests/pages/**` (a module of `tests/integration.rs`) | Offline HTML/page fixtures that reproduce complex interactions |
| WPT reftest | `tests/wpt/**` (a module of `tests/integration.rs`) | Spec-aligned coverage |

### Step 9: Verify and repeat

```bash
# Run a unit test (tests live in `src/`)
timeout -k 10 300 bash scripts/cargo_agent.sh test -p fastrender --lib my_new_test

# Run an integration test (tests live under `tests/` and are wired through `tests/integration.rs`)
timeout -k 10 300 bash scripts/cargo_agent.sh test -p fastrender --test integration my_new_test

# Re-render the page
timeout -k 10 300 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin render_pages -- --pages example.com

# Re-diff
scripts/chrome_vs_fastrender.sh --pages example.com
```

Repeat until the diff shows improvement.

## Deterministic fixture evidence loop (preferred)

For systematic accuracy work, use the offline fixture loop:

```bash
# 1) Render fixtures with FastRender
timeout -k 10 600 bash scripts/cargo_agent.sh run --release --bin render_fixtures

# 2) Generate Chrome baseline PNGs for fixtures
timeout -k 10 600 bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures

# 3) Generate combined diff report
timeout -k 10 600 bash scripts/cargo_agent.sh xtask fixture-chrome-diff

# Report: target/fixture_chrome_diff/report.html
```

Benefits:
- **Offline** — No network variability
- **Repeatable** — Same inputs every time
- **Bundled fonts** — Stable across machines
- **Trackable** — Fixtures are committed

## Auto-capture failing fixtures

When pages fail, automatically capture offline fixtures for investigation:

```bash
timeout -k 10 900 bash scripts/cargo_agent.sh xtask pageset --capture-missing-failure-fixtures
```

This scans `progress/pages/*.json` and imports missing fixtures under `tests/pages/fixtures/`.

## Accuracy metrics in progress files

Each `progress/pages/<stem>.json` may include accuracy metrics when computed:

```json
{
  "accuracy": {
    "baseline": "chrome",
    "diff_pixels": 12345,
    "diff_percent": 1.23,
    "perceptual": 0.42,
    "tolerance": 0,
    "max_diff_percent": 0.0,
    "perceptual_metric": "ssim_windowed_v2",
    "computed_at_commit": "abcdef0"
  }
}
```

`diff_pixels` / `diff_percent` are **raw pixel mismatch** metrics (after applying `tolerance`). In
practice they can be noisy on real pages: tiny anti-aliasing differences, font rasterization, and
small per-channel rounding differences can flip large numbers of pixels even when the images look
visually correct.

`perceptual` is a SSIM-derived perceptual distance (currently a **windowed SSIM over downsampled
luminance**), where `0.0` means identical. It is usually a better indicator of **visually
meaningful** differences, so prefer it when prioritizing which pages are actually “broken”. The
exact implementation can change over time; newer artifacts may include `perceptual_metric` and
`computed_at_commit` to help interpret mixed historical values without requiring a repo-wide
refresh.

Note: SSIM is *not* immune to text rendering noise. On pages that are extremely text-dense, small
per-glyph rasterization differences (subpixel AA, hinting, antialiasing, font fallback) can be
amplified across many windows and still produce a relatively high `perceptual` score even when the
page looks correct at a glance. Use the diff report visually and rely on spec-driven debugging when
deciding if a page is actually “broken”.

See [`progress/pages/README.md`](../progress/pages/README.md) for the full schema and migration
guidance.

## What to implement (capability buildout)

When accuracy failures reveal missing features, implement them properly:

### CSS values + computed style
- Parsing/serialization
- Shorthands
- `calc()` / `var()`
- Percentage bases
- `initial` / `inherit` / `unset`
- Correct error handling

### Selectors + cascade
- Correct specificity
- `:has()` / `:nth-*` semantics
- Shadow DOM selectors
- UA defaults matching modern expectations

### Layout correctness
- Block/inline formatting contexts
- Tables (CSS 2.1 §17)
- Flex/grid (via Taffy with correct inputs)
- Positioned layout
- Line breaking, bidi ordering, baseline alignment

### Painting correctness
- Stacking contexts, z-index
- Clipping, overflow
- Border-radius, transforms, filters
- Backgrounds, borders, outlines, shadows

### Text rendering
- Font fallback
- Shaping, variations, color fonts
- Decorations
- Vertical writing modes
- Ligatures, emoji

### Replaced elements/media
- Sizing rules
- `object-fit` / `object-position`
- Responsive images (`srcset`, `sizes`, `<picture>`)
- SVG embedding

Always tie the work back to a pageset page and add a regression where possible.
