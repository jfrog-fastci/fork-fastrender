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

### 1) Live-sample iframes exist (local `assets/*.html`), but are **offscreen** in the default page-loop viewport

All three MDN pages include one (or more) `iframe.sample-code-frame` “live sample” embeds. In these fixtures, they already point at **local bundled HTML**:

* `src: assets/<hash>.html`
* `srcdoc: null`

However, in the default `xtask page-loop` configuration (`viewport 1040x1240`, screenshot of the *top* of the page), these iframes are **far below the fold** and therefore do not appear in the baseline screenshots/diffs.

Concrete absolute positions (summing fragment ancestry offsets from `inspect/fragment_tree.json`):

* `writing-mode`: iframe y≈**7078px** and y≈**10125px**
* `text-orientation`: iframe y≈**3690px**
* `text-combine-upright`: iframe y≈**3774px**

So the page-loop diffs for these fixtures are not exercising vertical writing / text-orientation / text-combine correctness yet.

**Relevant implementation notes / entrypoints:**

* JS is disabled for page-loop Chrome baselines (and the FastRender side is patched to match) by `src/cli_utils/fixture_html_patch.rs` injecting a CSP meta tag with `script-src 'none'`. This likely prevents MDN’s `<interactive-example>` custom element from hydrating, but the `iframe.sample-code-frame` elements in these fixtures do **not** rely on JS (they already have a real `src`).
* There is an existing DOM “compat” mutation for iframes that can lift a placeholder `src` from `data-src`/`data-live-path` (`src/dom.rs:4886-4945`). These fixtures already have non-placeholder `src`, so that compat block is not needed here (but may matter for other MDN captures).
* To actually test vertical-writing behavior via these MDN pages, we likely need either a “scroll-to-sample” baseline mode or separate fixtures for the sample iframe HTML files.

### 2) All three diffs look like “generic MDN layout” diffs, not vertical-writing feature diffs

All three fixtures have very similar diff percentages (~4.4% pixel diff) and share the same first mismatch pixel `(x=911,y=107)` (within `dl.footer__links`).

That strongly suggests the current diffs are dominated by:

* general layout/paint differences (tables/definition lists/TOC sidebar/footer links),
* residual font/text rasterization differences,
* MDN’s heavy modern CSS + custom element layout,

…rather than `writing-mode`/`text-orientation`/`text-combine-upright` rendering correctness.

**Vertical-writing-related code that is *not meaningfully exercised* until the live-sample iframe content is in-view (i.e. captured by page-loop):**

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

Iframe nodes (from `inspect/dom.json` + `inspect/fragment_tree.json`):

* `iframe#frame_using_multiple_writing_modes.sample-code-frame`
  * `src=assets/6cb36740d41f266bc9b10239b3566fce.html`, `srcdoc=null`
  * absolute bounds in document: `(x≈273.0,y≈7077.9,w=432.0,h=732.0)`
* `iframe#frame_using_writing-mode_with_transforms.sample-code-frame`
  * `src=assets/25d003c583ed8f42996d3c0fd953a75a.html`, `srcdoc=null`
  * absolute bounds in document: `(x≈273.0,y≈10125.4,w=432.0,h=232.0)`

Note: the raw `bounds` stored on the iframe fragments are relative to their local fragment ancestry. When summing ancestry offsets, these iframes land thousands of pixels down the page, so they do not appear in the default page-loop viewport screenshot.

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
  * `src=assets/bbdc8f8a42d1b5971ca198c5d62428f0.html`, `srcdoc=null`
  * absolute bounds in document: `(x≈273.0,y≈3689.9,w=332.0,h=182.0)`

Note: this iframe is also offscreen in the default page-loop viewport.

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
  * `src=assets/489b0c9653c1ac283ee32072caaf5ec2.html`, `srcdoc=null`
  * absolute bounds in document: `(x≈273.0,y≈3774.5,w=282.0,h=232.0)`

Note: this iframe is also offscreen in the default page-loop viewport.

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

1. **Make the vertical-writing samples visible in the page-loop viewport.**
   * Option A (simplest): add dedicated fixtures that render the iframe `assets/*.html` directly, e.g.:
     * `tests/pages/fixtures/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/assets/6cb36740d41f266bc9b10239b3566fce.html` (writing-mode table)
     * `tests/pages/fixtures/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation/assets/bbdc8f8a42d1b5971ca198c5d62428f0.html` (vertical-rl + text-orientation)
     * `tests/pages/fixtures/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright/assets/489b0c9653c1ac283ee32072caaf5ec2.html` (vertical-rl + text-combine-upright)
   * Option B: extend `xtask page-loop` + the Chrome baseline harness to scroll to a fragment anchor (e.g. `index.html#examples`) or to render a full-page screenshot (fit canvas to content).
2. Once the sample content is in-view, re-run these fixtures and expect diffs to start exercising:
     * vertical writing-mode layout axes (`src/layout/axis.rs`),
     * vertical glyph orientation (`src/text/pipeline.rs`),
     * text combine grouping (`src/layout/contexts/inline/mod.rs`).

## Appendix: direct diffs of the *actual* MDN vertical-writing demos (iframe `assets/*.html`)

Because the live-sample iframes are offscreen in the default `xtask page-loop` viewport, I rendered the iframe HTML assets directly as standalone “mini fixtures” (locally, not committed) and diffed them against a JS-off Chrome baseline.

These runs produce artifacts under:

* `target/page_loop_mdn_vertical_samples/<stem>/...`

### Demo: writing-mode — “Using multiple writing modes”

* Source HTML:
  * `tests/pages/fixtures/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/assets/6cb36740d41f266bc9b10239b3566fce.html`
* Artifacts:
  * `target/page_loop_mdn_vertical_samples/mdn_writing_mode_multiple/report.html`
  * `target/page_loop_mdn_vertical_samples/mdn_writing_mode_multiple/report_files/diffs/mdn_writing_mode_multiple.png`
  * `target/page_loop_mdn_vertical_samples/mdn_writing_mode_multiple/inspect/*.json`
* Diff metrics (from `report.json`):
  * `diff_percentage`: **6.83%** (`pixel_diff=88,038` / `1,289,600`)
  * Overall mismatch bbox (exact pixel bbox of any mismatch): `(x=7,y=12)-(x=808,y=907)`
* Where the diff is:
  * Single large connected diff cluster covering the whole table region (coarse 20×20px tiling).
  * Per-row mismatch counts (using FastRender’s `tr` bounds from `inspect/fragment_tree.json`):
    * `horizontal-tb` row (`tr.text1`): `6,019` mismatching pixels
    * `vertical-lr` row (`tr.text2`): `11,181`
    * `vertical-rl` row (`tr.text3`): `12,272`
    * `sideways-lr` row (`tr.text4`): `12,647`
    * `sideways-rl` row (`tr.text5`): `27,158` (largest)
* Likely subsystem / entrypoints:
  * **Vertical writing-mode axis mapping** (especially `sideways-rl`): `src/layout/axis.rs`, `src/style/types.rs` (`WritingMode`)
  * **Vertical glyph orientation in mixed-script text** (default `text-orientation: mixed`): `src/text/pipeline.rs` (`apply_sideways_text_orientation`, `apply_vertical_text_orientation`)
  * **Table layout interactions**: `src/layout/table.rs`

### Demo: writing-mode — “Using writing-mode with transforms”

* Source HTML:
  * `tests/pages/fixtures/developer.mozilla.org_en-US_docs_Web_CSS_writing-mode/assets/25d003c583ed8f42996d3c0fd953a75a.html`
* Artifacts:
  * `target/page_loop_mdn_vertical_samples/mdn_writing_mode_transforms/report.html`
  * `target/page_loop_mdn_vertical_samples/mdn_writing_mode_transforms/report_files/diffs/mdn_writing_mode_transforms.png`
  * `target/page_loop_mdn_vertical_samples/mdn_writing_mode_transforms/inspect/*.json`
* Diff metrics:
  * `diff_percentage`: **2.55%** (`pixel_diff=32,895` / `1,289,600`)
  * Overall mismatch bbox: `(x=7,y=0)-(x=915,y=233)` (table region)
* Per-cell mismatch (FastRender `td` bounds):
  * Cell 1 (`span.vertical-lr`, no transform): `16.15%` diff within cell bbox
  * Cell 2 (`span.vertical-lr.rotated`, `transform: rotate(180deg)`): `14.94%`
  * Cell 3 (`span.sideways-lr`): `9.35%`
  * Cell 4 (`span.only-rotate`, `inline-size: fit-content; transform: rotate(-90deg)`): `10.82%`
* Notable: within the union bbox of the `span.sideways-lr` glyphs, the mismatch rate is much lower (~**1.23%**) than the `vertical-lr`/transform cases.
* Likely subsystem / entrypoints:
  * **writing-mode + transform composition** (vertical-lr + rotate): layout axis mapping + transform application during paint
    * layout: `src/layout/axis.rs`, `src/layout/contexts/inline/mod.rs`
    * paint/transforms: `src/paint/display_list_builder.rs`, `src/paint/painter.rs`

### Demo: text-orientation — upright

* Source HTML:
  * `tests/pages/fixtures/developer.mozilla.org_en-US_docs_Web_CSS_text-orientation/assets/bbdc8f8a42d1b5971ca198c5d62428f0.html`
* Artifacts:
  * `target/page_loop_mdn_vertical_samples/mdn_text_orientation_upright/report.html`
  * `target/page_loop_mdn_vertical_samples/mdn_text_orientation_upright/report_files/diffs/mdn_text_orientation_upright.png`
* Diff metrics:
  * `diff_percentage`: **0.13%** (`pixel_diff=1,719` / `1,289,600`)
* Diff shape:
  * Multiple small diff clusters, mostly over the vertical text glyphs (plus a tiny strip near the right edge).
* Likely subsystem / entrypoints:
  * Probably **font rasterization/text metrics** differences rather than a major vertical-layout bug.
  * Vertical orientation logic lives in `src/text/pipeline.rs`.

### Demo: text-combine-upright — all (vertical-rl)

* Source HTML:
  * `tests/pages/fixtures/developer.mozilla.org_en-US_docs_Web_CSS_text-combine-upright/assets/489b0c9653c1ac283ee32072caaf5ec2.html`
* Artifacts:
  * `target/page_loop_mdn_vertical_samples/mdn_text_combine_upright/report.html`
  * `target/page_loop_mdn_vertical_samples/mdn_text_combine_upright/report_files/diffs/mdn_text_combine_upright.png`
  * `target/page_loop_mdn_vertical_samples/mdn_text_combine_upright/inspect/{box_tree,fragment_tree}.json`
* Diff metrics:
  * `diff_percentage`: **0.21%** (`pixel_diff=2,718` / `1,289,600`)
* Key observation: diff clusters appear as *two vertical strips*, one near the left edge and one near the right edge.
  * This pattern is consistent with the vertical text column being positioned on the **wrong side** (Chrome has ink where FastRender has background, and vice-versa).
* Inspect evidence (FastRender):
  * The `<html>` block box ends up with a **physical width of only ~53.8px** (`inspect/fragment_tree.json`), instead of filling the `1040px` viewport.
  * The `<p>` box is a narrow column at approximately `x≈8..46` (left side).
  * This strongly suggests a **root/initial containing block sizing + vertical-rl anchoring bug** when `writing-mode` is applied to the root element.
* Likely subsystem / entrypoints:
  * Root/viewport containing block sizing under vertical writing modes:
    * `src/layout/engine.rs` (initial constraints / fragmentainer axes)
    * `src/layout/axis.rs`, `src/style/types.rs` (`WritingMode`)
  * Once the root writing-mode placement is correct, re-check whether `text-combine-upright` itself mismatches:
    * `src/layout/contexts/inline/mod.rs` (`TextCombineUpright`)
