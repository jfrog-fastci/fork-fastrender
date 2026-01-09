# Headless Chrome viewport height padding (`88px`)

When using Chrome/Chromium in headless screenshot mode (`--headless` / `--headless=new` +
`--screenshot`), the `--window-size=WxH` flag controls the **outer** window size, but the
CSS/layout viewport height is consistently smaller.

In practice this shows up as:

- a persistent white bar at the bottom of screenshots when you request `--window-size=WxH`, or
- mismatched viewport sizes between FastRender output and Chrome baselines (fixtures + pageset
  ground truth).

## Workaround used in this repo

Both of these entrypoints apply the same workaround:

- `scripts/chrome_baseline.sh` (pageset baseline screenshots)
- `bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures` / `scripts/chrome_fixture_baseline.sh` (offline fixtures)

The workaround is:

1. Request a taller outer window: `--window-size=<w>,<h + pad_px>`
2. Let Chrome produce a screenshot PNG of that outer window.
3. Crop the PNG back down to exactly `<w>x<h>`.

The default `pad_px` is **88px** (`HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX`).

## Configuration

The pad value is empirically derived and may vary across Chrome versions / OS packaging.

Override it by setting:

```bash
export HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX=88
```

Both the bash scripts and the Rust `xtask` implementation respect this environment variable.

## Verification (real Chrome)

To validate the pad/crop logic against a real Chrome/Chromium binary (no golden updates):

```bash
CHROME_BIN=/path/to/chrome \
  scripts/verify_chrome_baseline_viewport.sh
```

By default it runs the smoke test at multiple DPR values (`DPRS=1.0,1.333`) so we also exercise
viewport-to-pixel rounding. Override as needed:

```bash
CHROME_BIN=/path/to/chrome \
  DPRS=1.0,2.0 \
  scripts/verify_chrome_baseline_viewport.sh
```

The script renders a tiny test page with a solid red bar pinned to the bottom of the viewport and
asserts:

- the output PNG dimensions match `VIEWPORT` exactly, and
- the bottom strip is red (a heuristic that catches both under-padding and over-padding).
- the metadata sidecar records the expected `chrome_window` and `chrome_window_padding_css` values.

It also renders a representative offline fixture HTML (`tests/pages/fixtures/br_linebreak/index.html`)
to validate that the cached-HTML path (meta sidecar + `<base href=...>` injection) still produces
the exact requested dimensions.

If it fails on your machine, rerun with a different `HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX`
(binary search works well) until it passes.
