# Page-loop tooling playbook

This doc is the **practical companion** to [`instructions/pageset_page_loop.md`](../instructions/pageset_page_loop.md): it shows the minimal commands and artifacts for the day-to-day “pick a page → make it good” loop.

Source of truth for flags is always `--help` output (`bash scripts/cargo_agent.sh xtask --help`, `bash scripts/cargo_agent.sh run --release --bin inspect_frag -- --help`), but you should be able to follow this doc end-to-end without guessing paths.

## Happy path: freeze → validate → page-loop

The workflow is:

1. **Freeze** a pageset page into an offline fixture (`tests/pages/fixtures/<stem>/...`).
2. **Validate** that the fixture is truly offline (no `http(s)://` or `//` fetchable URLs).
3. Run **`page-loop`** to render FastRender + overlay + Chrome baseline + diff report.

> Note on viewport/DPR: the fixture capture step may only include responsive subresources needed for the chosen viewport/DPR.
> Keep `--viewport`/`--dpr` consistent between capture and the renders you care about.

### 1) Freeze a pageset fixture (`xtask freeze-page-fixture`)

Pick a pageset page from `src/pageset.rs` and capture it:

```bash
# Use a URL or a pageset cache stem (e.g. example.com or example.com--deadbeef).
bash scripts/cargo_agent.sh xtask freeze-page-fixture \
  --page https://example.com \
  --viewport 1200x800 \
  --dpr 1.0
```

Common follow-ups:

- Re-capture and replace an existing fixture:

  ```bash
  bash scripts/cargo_agent.sh xtask freeze-page-fixture --page https://example.com --overwrite
  ```

- Re-fetch HTML even if cached (`fetches/html/<stem>.html` already exists):

  ```bash
  bash scripts/cargo_agent.sh xtask freeze-page-fixture --page https://example.com --refresh
  ```

Where it writes:

- Imported fixture: `tests/pages/fixtures/<stem>/index.html` (plus local `assets/`/`css/`/etc)
- Intermediate bundle(s): `target/pageset_fixture_bundles/<stem>.tar`

`freeze-page-fixture` ends by running `xtask validate-page-fixtures` for the captured stems.

### 2) Validate fixtures offline (`xtask validate-page-fixtures`)

Re-run validation any time you edit fixture files:

```bash
bash scripts/cargo_agent.sh xtask validate-page-fixtures --only example.com
```

If you captured scripts via `freeze-page-fixture --include-scripts`, validate script URLs too:

```bash
bash scripts/cargo_agent.sh xtask validate-page-fixtures --only example.com --include-scripts
```

### 3) Run the page loop (`xtask page-loop`)

Render FastRender, generate an overlay, run Chrome, and write a diff report:

```bash
bash scripts/cargo_agent.sh xtask page-loop \
  --fixture example.com \
  --viewport 1200x800 \
  --dpr 1.0 \
  --overlay \
  --inspect-dump-json \
  --write-snapshot \
  --chrome
```

Alternative: if you’re starting from a pageset URL/stem and don’t want to think about fixture naming (including stem collisions), use `--pageset`:

```bash
bash scripts/cargo_agent.sh xtask page-loop --pageset https://example.com --overlay --inspect-dump-json --write-snapshot --chrome
```

Tip: add `--dry-run` to print the resolved paths and commands without executing.

### 4) Interpreting page-loop outputs (what to open)

Default output root is `target/page_loop/<stem>/` (override with `--out-dir`).

Key artifacts:

- FastRender render:
  - `target/page_loop/<stem>/fastrender/<stem>.png`
  - `target/page_loop/<stem>/fastrender/<stem>.json` (render metadata)
- Snapshot pipeline dump (when `--write-snapshot`):
  - `target/page_loop/<stem>/fastrender/<stem>/snapshot.json`
- Overlay image (when `--overlay`):
  - `target/page_loop/<stem>/overlay/<stem>.png`
- Pipeline stage dumps (when `--inspect-dump-json`):
  - `target/page_loop/<stem>/inspect/dom.json` (plus `styled.json`, `box_tree.json`, `fragment_tree.json`, `display_list.json`, etc.)
- Chrome render (when `--chrome`):
  - `target/page_loop/<stem>/chrome/<stem>.png`
- Chrome-vs-FastRender diff report (when `--chrome`):
  - `target/page_loop/<stem>/report.html` (open this)
  - `target/page_loop/<stem>/report.json` (machine-readable)

### Comparing two FastRender runs (diffs + snapshots)

`page-loop --chrome` answers “how far are we from Chrome?”. When you’re iterating on a fix, it’s often more useful to compare **FastRender-before vs FastRender-after**:

```bash
# Baseline render + snapshot (no Chrome step).
bash scripts/cargo_agent.sh xtask page-loop \
  --fixture example.com \
  --out-dir target/page_loop_before/example.com \
  --viewport 1200x800 \
  --dpr 1.0 \
  --write-snapshot \
  --no-chrome

# After your code changes:
bash scripts/cargo_agent.sh xtask page-loop \
  --fixture example.com \
  --out-dir target/page_loop_after/example.com \
  --viewport 1200x800 \
  --dpr 1.0 \
  --write-snapshot \
  --no-chrome
```

Now compare:

- Pixel diffs (`diff_renders` wrapped by xtask):

  ```bash
  bash scripts/cargo_agent.sh xtask diff-renders \
    --before target/page_loop_before/example.com/fastrender \
    --after target/page_loop_after/example.com/fastrender \
    --output target/page_loop_delta/example.com
  # Open: target/page_loop_delta/example.com/diff_report.html
  ```

- Pipeline diffs (`diff_snapshots`):

  ```bash
  bash scripts/cargo_agent.sh run --release --bin diff_snapshots -- \
    --before target/page_loop_before/example.com/fastrender \
    --after target/page_loop_after/example.com/fastrender \
    --html target/page_loop_delta/example.com/diff_snapshots.html \
    --json target/page_loop_delta/example.com/diff_snapshots.json
  # Open: target/page_loop_delta/example.com/diff_snapshots.html
  ```

## Deep debugging with `inspect_frag`

`inspect_frag` is the “turn the guts inside out” tool: it lets you dump/trace the pipeline structures for a document.

### Dump the pipeline as JSON (`--dump-json`)

For an offline fixture:

```bash
bash scripts/cargo_agent.sh run --release --bin inspect_frag -- \
  tests/pages/fixtures/example.com/index.html \
  --dump-json target/inspect_frag/example.com
```

The dump directory contains:

- `dom.json` — parsed DOM (`DomNode`)
- `composed_dom.json` — DOM after composition/normalization passes
- `styled.json` — styled tree (computed styles per node)
- `box_tree.json` — box tree (anonymous wrappers / display mapping)
- `fragment_tree.json` — fragment tree (laid-out geometry; line boxes; floats)
- `display_list.json` — paint commands / stacking contexts (what gets painted)

This is usually the fastest way to answer: “which stage first went wrong?”

### Focus on one subtree (`--filter-selector` / `--filter-id`)

When a full dump is too big, restrict to the first matching element:

```bash
bash scripts/cargo_agent.sh run --release --bin inspect_frag -- \
  tests/pages/fixtures/example.com/index.html \
  --dump-json target/inspect_frag/example.com_nav \
  --filter-selector "#nav"
```

Or by `id=` attribute:

```bash
bash scripts/cargo_agent.sh run --release --bin inspect_frag -- \
  tests/pages/fixtures/example.com/index.html \
  --dump-json target/inspect_frag/example.com_hero \
  --filter-id hero
```

Filters apply to dumps **and** traces/overlays (they trim the pipeline to the matched subtree).

### Trace “where did this text/box go?” (`--trace-text` / `--trace-box`)

Print the fragment ancestry path for the first matching text fragment:

```bash
bash scripts/cargo_agent.sh run --release --bin inspect_frag -- \
  tests/pages/fixtures/example.com/index.html \
  --trace-text "Subscribe"
```

Trace by box id (useful when you’ve identified a problematic box in `box_tree.json` or from a previous trace):

```bash
bash scripts/cargo_agent.sh run --release --bin inspect_frag -- \
  tests/pages/fixtures/example.com/index.html \
  --trace-box 1234
```

`--trace-box` prints a computed-style summary for `box#1234` and then prints a fragment-tree path to the first fragment associated with that box id.

## Turning a page bug into a minimal regression

Live pages motivate fixes, but regressions keep them fixed. Prefer (in order):

1. **Unit tests** (parser/cascade/computed values) where possible.
2. **Layout tests** under `tests/layout/`.
3. **Paint tests** under `tests/paint/`.
4. A **tiny offline page fixture** only when the interaction can’t be expressed in the existing harnesses.

Test organization is non-negotiable:

- Keep tests in the existing harnesses (see `AGENTS.md` “Test organization”).
- Do **not** add new top-level `tests/*.rs` integration test crates.

## Guardrails (do not skip)

- **No page-specific hacks.** Fix primitives; don’t special-case hostnames/selectors.
- **Always use the cargo wrapper:**
  - ✅ `bash scripts/cargo_agent.sh ...`
  - ❌ `cargo ...`
- **Always enforce memory caps when executing renderer binaries:**
  - If you run a built binary directly, wrap it:

    ```bash
    bash scripts/run_limited.sh --as 64G -- target/release/inspect_frag --help
    ```

  - `cargo_agent.sh` and `xtask` already run under `scripts/run_limited.sh`, but the rule still applies if you bypass them.
- **Chrome needs more address space than you think.** If `xtask page-loop --chrome` fails with `Oilpan: Out of memory`, bump the xtask address-space cap:

  ```bash
  FASTR_XTASK_LIMIT_AS=128G bash scripts/cargo_agent.sh xtask page-loop --fixture example.com --chrome
  ```
