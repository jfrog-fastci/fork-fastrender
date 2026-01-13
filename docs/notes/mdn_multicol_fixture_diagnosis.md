# MDN multicol fixture: demo block inspection

Pageset fixture: `developer.mozilla.org_en-US_docs_Web_CSS_CSS_multicol_layout_Using_multi-column_layouts`

This fixture shows a high perceptual diff in progress runs, so I inspected the **six inlined multi-column demo blocks**:

- `#col`
- `#wid`
- `#col_short`
- `#columns_4`
- `#columns_12`
- `#column_gap`

## Method

Instead of running `xtask page-loop` end-to-end (which is mostly orchestration around `render_fixtures` + `inspect_frag`), I ran the underlying inspector directly for each element:

```bash
target/debug/inspect_frag tests/pages/fixtures/developer.mozilla.org_en-US_docs_Web_CSS_CSS_multicol_layout_Using_multi-column_layouts/index.html \
  --patch-html-for-chrome-baseline \
  --dump-json target/page_loop/mdn_multicol/<id>/inspect \
  --render-overlay target/page_loop/mdn_multicol/<id>/overlay.png \
  --filter-id <id> \
  --viewport 1040x1240 --dpr 1.0 --media screen
```

For each `<id>`, I extracted “used” column geometry from `fragment_tree.json`:

- **Column count**: distinct `fragmentainer_index` values seen under the container fragment.
- **Column start positions**: min `x` per `fragmentainer_index`.
- **Used `column-gap` + column inline size**: derived from container width and the start delta:
  - `delta = start[i+1] - start[i]` (≈ `column_width + column_gap`)
  - `gap = (N * delta) - container_width`
  - `col_width = delta - gap`

This is effectively the `FragmentationOptions` the engine ended up using (even though the debug snapshot JSON does not currently serialize `ComputedStyle::{column_count,column_width,column_gap}` directly).

## Findings (expected vs observed)

All six demo blocks produce the expected column fragmentation and used gaps/widths:

- `#col` (`column-count: 2`)
  - Observed: **2 columns**, starts `[0, 246]`, used gap **16px** (≈ default `1em`), column width **230px**.
  - Column heights (max `y+height` per fragmentainer): `{0: 268, 1: 240}` (balanced-ish; second column ~1 line shorter).

- `#wid` (`column-width: 100px`)
  - Observed: **4 columns**, starts `[0, 123, 246, 369]`, used gap **16px**, column width **107px** (≥ 100px; matches typical “fit as many as possible” algorithm).
  - Heights: `{0: 252, 1: 280, 2: 252, 3: 280}`.

- `#col_short` (`columns: 12em`)
  - Observed: **2 columns**, starts `[0, 246]`, used gap **16px**, column width **230px** (≥ 12em ≈ 192px at 16px font size).
  - Heights: `{0: 224, 1: 224}`.

- `#columns_4` (`columns: 4`)
  - Observed: **4 columns**, starts `[0, 123, 246, 369]`, used gap **16px**, column width **107px**.
  - Heights: `{0: 252, 1: 280, 2: 252, 3: 280}`.

- `#columns_12` (`columns: 12 8em`)
  - Observed: **3 columns** (fits 3×8em + gaps in the 476px container), starts `[0, 164, 328]`, used gap **16px**, column width **148px** (≥ 8em ≈ 128px).
  - Heights: `{0: 252, 1: 252, 2: 252}`.

- `#column_gap` (`column-count: 5; column-gap: 2em`)
  - Observed: **5 columns**, starts `[0, 101.6, 203.2, 304.8, 406.4]`, used gap **32px** (= 2em ⇒ 1em ≈ 16px), column width **69.6px**.
  - Heights: `{0: 336, 1: 336, 2: 336, 3: 336, 4: 336}`.

## Conclusion

No obvious multicol-specific brokenness was found in the six inlined demo blocks:

- Inline style parsing for `column-count`, `column-width`, `columns`, and `column-gap` is taking effect (column fragmentainers are present).
- Column geometry (count/width/gap) matches expected “Chrome-like” results for the fixed container width in this fixture.

If this pageset fixture still has a high overall diff, the root cause is likely elsewhere on the MDN page, not in these multicol demos.

Relevant implementation touchpoints (for future multicol issues):

- Column geometry + multicol layout entrypoint: `src/layout/contexts/block/mod.rs` (`layout_multicolumn`, `compute_column_geometry`)
- Fragmentation: `src/layout/fragmentation.rs`
- Inline style parsing: `src/style/cascade.rs::cached_inline_style_declarations`

