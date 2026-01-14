# MDN vertical-writing fixtures triage (page-loop)

Goal fixtures:

* `developer.mozilla.org_en-US_docs_Web_CSS_writing-mode`
* `developer.mozilla.org_en-US_docs_Web_CSS_text-orientation`
* `developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright`

This note inventories the *actual* brokenness/diff hotspots produced by `xtask page-loop` so follow-up work can focus on fixes (iframe live samples + vertical writing/text-orientation/text-combine) without re-triaging.

## Repro

### Preferred (single command per fixture)

```bash
timeout -k 10 600 \
  bash scripts/cargo_agent.sh xtask page-loop \
  --fixture <STEM> \
  --chrome \
  --inspect-dump-json \
  --write-snapshot
```

### If Cargo lock contention makes the 600s wrapper flaky

`xtask page-loop` spends time inside Cargo even when everything is already built. To avoid multi-agent cache-lock stalls, you can run the already-built binaries directly (this is what produced the numbers below):

```bash
stem='<STEM>'
out="target/page_loop/$stem"
rm -rf "$out" && mkdir -p "$out"

# 1) FastRender screenshot + snapshot
FASTR_LAYOUT_PARALLEL=auto \
  bash scripts/run_limited.sh --as 64G -- \
  target/release/render_fixtures \
  --fixtures-dir tests/pages/fixtures \
  --out-dir "$out/fastrender" \
  --fixtures "$stem" \
  --jobs 1 \
  --viewport 1040x1240 \
  --dpr 1 \
  --media screen \
  --timeout 120 \
  --patch-html-for-chrome-baseline \
  --system-fonts \
  --animation-time-ms 4940 \
  --write-snapshot

# 2) inspect_frag JSON dumps (full page)
FASTR_COMPAT_REPLACED_MAX_WIDTH_100=0 \
FASTR_DETERMINISTIC_PAINT=1 \
FASTR_HIDE_SCROLLBARS=1 \
FASTR_LAYOUT_PARALLEL=auto \
FASTR_TEXT_HINTING=1 \
FASTR_TEXT_SNAP_GLYPH_POSITIONS=1 \
FASTR_TEXT_SUBPIXEL_AA=0 \
FASTR_TEXT_SUBPIXEL_AA_GAMMA=1.0 \
FASTR_WEB_FONT_WAIT_MS=1000 \
  bash scripts/run_limited.sh --as 64G -- \
  target/release/inspect_frag "tests/pages/fixtures/$stem/index.html" \
  --deny-network \
  --patch-html-for-chrome-baseline \
  --system-fonts \
  --animation-time-ms 4940 \
  --dump-json "$out/inspect" \
  --viewport 1040x1240 --dpr 1 --media screen --timeout 120

# 3) Chrome baseline (JS off)
bash scripts/run_limited.sh --as 96G -- \
  target/debug/xtask chrome-baseline-fixtures \
  --fixture-dir tests/pages/fixtures \
  --fixtures "$stem" \
  --out-dir "$out/chrome" \
  --viewport 1040x1240 --dpr 1 --timeout 120 --media screen

# 4) Diff report
bash scripts/run_limited.sh --as 64G -- \
  target/release/diff_renders \
  --before "$out/chrome/$stem.png" \
  --after  "$out/fastrender/$stem.png" \
  --html "$out/report.html" \
  --json "$out/report.json" \
  --tolerance 0 \
  --max-diff-percent 0 \
  --sort-by percent
```

## Global findings (applies to all 3 fixtures)

### 1) Live-sample iframes do not load any live-sample content

All three MDN pages include a top-of-page “Try it” iframe with `class="sample-code-frame"`, but in the rendered fragment tree it remains:

* `src: about:blank`
* `srcdoc: null`

So the vertical writing demos / `text-orientation` / `text-combine-upright` examples that MDN normally injects into that iframe are **not present** in either renderer output (Chrome baseline runs with JS disabled by the harness).

**Relevant implementation notes / entrypoints:**

* JS is disabled for page-loop Chrome baselines (and the FastRender side is patched to match) by `src/cli_utils/fixture_html_patch.rs` injecting a CSP meta tag with `script-src 'none'`. This prevents MDN’s JS live-sample bootstrapping from running.
* There is an existing DOM “compat” mutation for iframes that can lift a placeholder `src` from `data-src`/`data-live-path` (`src/dom.rs:4878-4902`), but MDN uses `data-live-path` + `data-live-id` (and `data-live-path` is *not* a local file URL in these fixtures).
* Follow-up direction: add an MDN-specific compat path that synthesizes an iframe `srcdoc` (or rewrites to a local generated HTML file) from the surrounding `<pre class="… live-sample---<id>">` code blocks, keyed by `data-live-id`.

### 2) All three diffs look like “generic MDN layout” diffs, not vertical-writing feature diffs

All three fixtures have very similar diff percentages (~4.4% pixel diff) and share the same first mismatch pixel `(x=911,y=107)` (within `dl.footer__links`).

That strongly suggests the current diffs are dominated by:

* general layout/paint differences (tables/definition lists/TOC sidebar/footer links),
* residual font/text rasterization differences,
* MDN’s heavy modern CSS + custom element layout,

…rather than `writing-mode`/`text-orientation`/`text-combine-upright` rendering correctness.

**Vertical-writing-related code that is *not meaningfully exercised* until live samples render:**

* Writing mode axis mapping / flow-relative coordinates:
  * `src/layout/axis.rs`, `src/layout/engine.rs`, `src/style/types.rs` (`WritingMode`)
* Vertical text orientation:
  * `src/text/pipeline.rs` (`apply_vertical_text_orientation`, `apply_sideways_text_orientation`)
* `text-combine-upright` inline layout grouping:
  * `src/layout/contexts/inline/mod.rs` (`TextCombineUpright`)

## Fixture: `developer.mozilla.org_en-US_docs_Web_CSS_writing-mode`

Artifacts (after running):

* Chrome screenshot: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/chrome/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode.png`
* FastRender screenshot: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/fastrender/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode.png`
* Diff report: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/report.html`
* Diff image: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/report_files/diffs/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode.png`
* Full inspect dumps: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/inspect/{styled,box_tree,fragment_tree,...}.json`
* Iframe-focused inspect dumps: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/inspect_iframe/*.json`

Diff metrics (from `report.json`):

* `diff_percentage`: **4.47%** (`pixel_diff=57,691` / `1,289,600`)

### Live-sample iframe status

Iframe nodes (from `inspect/styled.json` + `inspect/fragment_tree.json`):

* `iframe#frame_using_multiple_writing_modes.sample-code-frame`
  * bounds: `(x=1.0,y=50.6,w=432.0,h=732.0)`
  * `src=about:blank`, `srcdoc=null`
* `iframe#frame_using_writing-mode_with_transforms.sample-code-frame`
  * bounds: `(x=1.0,y=50.6,w=432.0,h=232.0)`
  * `src=about:blank`, `srcdoc=null`

Observation: both sample iframes show up at the same `y=50.6` in the fragment tree (overlapping bounds). Since both are `about:blank`, this overlap does not surface as “vertical writing” content; it *may* still affect downstream flow/layout spacing.

### Top 5 diff clusters (coarse 20×20px tile clustering)

These are the largest connected clusters of differing pixels between Chrome baseline and FastRender screenshots (useful for quickly locating hotspots in the diff PNG).

1. **Main content / definition list area**
   * bbox: `(260,1020)-(740,1160)`
   * representative node: `dd` (`inspect/styled.json` node_id=1570)
   * likely subsystem: block layout + text metrics
     * entrypoints: `src/layout/formatting_context.rs`, `src/text/pipeline.rs`
2. **Header/footer-ish block**
   * bbox: `(260,380)-(760,540)`
   * representative node: `footer` (`node_id=1726`)
   * likely subsystem: flex/flow layout + text rasterization
     * entrypoints: `src/layout/taffy_integration.rs` (flex), `src/text/pipeline.rs`
3. **Right sidebar / table of contents**
   * bbox: `(800,260)-(980,560)`
   * representative node: `aside.reference-layout__toc` (`node_id=1339`)
   * likely subsystem: positioned layout / sticky/scroll + list/text
     * entrypoints: `src/layout/contexts/positioned.rs` (sticky), `src/layout/formatting_context.rs`
4. **Left/top navigation list item**
   * bbox: `(0,160)-(240,300)`
   * representative node: `li` (`node_id=7056`)
   * likely subsystem: list layout + text
     * entrypoints: `src/layout/formatting_context.rs`, `src/style/cascade.rs` (UA defaults)
5. **Misc list item in main column**
   * bbox: `(260,220)-(520,260)`
   * representative node: `li` (`node_id=1955`)
   * likely subsystem: list layout + text

## Fixture: `developer.mozilla.org_en-US_docs_Web_CSS_text-orientation`

Artifacts:

* Chrome screenshot: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation/chrome/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation.png`
* FastRender screenshot: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation/fastrender/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation.png`
* Diff report: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation/report.html`
* Diff image: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation/report_files/diffs/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation.png`
* Full inspect dumps: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation/inspect/*.json`
* Iframe-focused inspect dumps: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation/inspect_iframe/*.json`

Diff metrics:

* `diff_percentage`: **4.40%** (`pixel_diff=56,714` / `1,289,600`)

### Live-sample iframe status

* `iframe#frame_examples.sample-code-frame`
  * bounds: `(x=1.0,y=50.6,w=332.0,h=182.0)`
  * `src=about:blank`, `srcdoc=null`

### Top 5 diff clusters (coarse 20×20px tile clustering)

1. **Main content definition list term**
   * bbox: `(260,380)-(780,560)`
   * representative node: `dt#sideways-right` (`node_id=1447`)
   * likely subsystem: inline/text layout + font metrics
     * entrypoints: `src/layout/contexts/inline/mod.rs`, `src/text/pipeline.rs`
2. **Main content section body**
   * bbox: `(260,1060)-(780,1240)`
   * representative node: `section.content-section` (`node_id=1379`)
   * likely subsystem: block layout + text
3. **Right sidebar / table of contents**
   * bbox: `(800,260)-(980,560)`
   * representative node: `aside.reference-layout__toc` (`node_id=1315`)
   * likely subsystem: positioned layout / sticky + list/text
4. **Top-right nav/footer links**
   * bbox: `(540,100)-(1020,240)`
   * representative node: `dl.footer__links` (`node_id=6748`)
   * likely subsystem: list layout + background/border painting
     * entrypoints: `src/layout/formatting_context.rs`, `src/paint/display_list_renderer.rs`
5. **Left/top navigation list item**
   * bbox: `(0,160)-(480,300)`
   * representative node: `li` (`node_id=6775`)
   * likely subsystem: list layout + text

## Fixture: `developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright`

Artifacts:

* Chrome screenshot: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright/chrome/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright.png`
* FastRender screenshot: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright/fastrender/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright.png`
* Diff report: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright/report.html`
* Diff image: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright/report_files/diffs/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright.png`
* Full inspect dumps: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright/inspect/*.json`
* Iframe-focused inspect dumps: `target/page_loop/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright/inspect_iframe/*.json`

Diff metrics:

* `diff_percentage`: **4.43%** (`pixel_diff=57,143` / `1,289,600`)

### Live-sample iframe status

* `iframe#frame_example_using_all.sample-code-frame`
  * bounds: `(x=1.0,y=50.6,w=282.0,h=232.0)`
  * `src=about:blank`, `srcdoc=null`

### Top 5 diff clusters (coarse 20×20px tile clustering)

1. **Notecard callout**
   * bbox: `(260,380)-(780,580)`
   * representative node: `div.notecard.note` (`node_id=1428`)
   * likely subsystem: block layout + border/background paint
     * entrypoints: `src/layout/formatting_context.rs`, `src/paint/painter.rs`
2. **Code example block**
   * bbox: `(260,600)-(780,660)`
   * representative node: `div.code-example` (`node_id=1569`)
   * likely subsystem: pre/code layout + text metrics
     * entrypoints: `src/layout/contexts/inline/mod.rs`, `src/text/pipeline.rs`
3. **Right sidebar / table of contents**
   * bbox: `(800,260)-(980,560)`
   * representative node: `aside.reference-layout__toc` (`node_id=1314`)
   * likely subsystem: positioned layout / sticky + list/text
4. **Table cell**
   * bbox: `(260,220)-(660,260)`
   * representative node: `td` (`node_id=1485`)
   * likely subsystem: table layout
     * entrypoints: `src/layout/formatting_context.rs` (table formatting), `src/style/user_agent.css` (table defaults)
5. **Left/top navigation list item**
   * bbox: `(0,160)-(240,300)`
   * representative node: `li` (`node_id=6737`)
   * likely subsystem: list layout + text

## Actionable follow-up checklist

1. **Make MDN live samples actually render (unblocks vertical writing triage).**
   * Option A: Extend DOM compatibility mode to synthesize `iframe.srcdoc` for MDN’s `sample-code-frame` from nearby `live-sample---*` code blocks (keyed by `data-live-id`).
     * start at: `src/dom.rs` iframe compat block (`data-live-path` exists today, but MDN needs `data-live-id` too).
   * Option B: Extend fixture capture/rewriting to replace `src=about:blank` with a local sample HTML file and ensure it’s included in the fixture bundle.
     * relevant plumbing: `src/html/asset_discovery.rs` (embedded documents) + fixture bundling tools.
2. If enabling JS in Chrome baselines is desired for MDN, `xtask fixture-chrome-diff` already supports `--js on` (`xtask/src/fixture_chrome_diff.rs`), but `xtask page-loop` does not currently forward a JS mode to `chrome-baseline-fixtures` (`xtask/src/page_loop.rs:build_chrome_baseline_command`).
3. Once live samples render, re-run these fixtures and expect diffs to start exercising:
   * vertical writing-mode layout axes (`src/layout/axis.rs`),
   * vertical glyph orientation (`src/text/pipeline.rs`),
   * text combine grouping (`src/layout/contexts/inline/mod.rs`).

