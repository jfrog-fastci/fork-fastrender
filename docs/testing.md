# Testing

## Assume tests can misbehave

Tests exercise code paths that process hostile inputs and implement complex algorithms. **Any test can hang, explode memory, or refuse to terminate:**

- A layout test might trigger an infinite loop in a new algorithm
- A parsing test might cause exponential backtracking
- A resource test might block forever on a network mock
- A regression test might expose a livelock under specific conditions

**All test commands need hard external limits:**

```bash
# CORRECT — time limit (with SIGKILL fallback) + memory limit + scoped:
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh test -p fastrender --lib

# WRONG — no time limit (test can hang forever, ignoring SIGTERM):
bash scripts/cargo_agent.sh test -p fastrender --lib

# WRONG — timeout without -k (misbehaving code can ignore SIGTERM):
timeout 600 bash scripts/cargo_agent.sh test -p fastrender --lib
```

`-k 10` means: send SIGTERM at timeout, then SIGKILL 10 seconds later if still running. SIGKILL cannot be caught or ignored — it's the only guarantee against pathological code.

**If a test times out, that's a bug to investigate — not a timeout to extend.**

---

FastRender's test suite is primarily Rust unit/integration tests plus a small set of visual fixture tests.

## Test organization

FastRender uses the standard Rust split:

- **Unit tests** live in `src/` alongside the code they test (typically in `#[cfg(test)] mod tests { ... }`).
  - Run: `bash scripts/cargo_agent.sh test -p fastrender --lib`
  - Paint/backdrop rendering regressions live under `src/paint/tests/`.
- **Integration tests** live in `tests/` and exercise the public API / external fixtures.
  - `tests/integration.rs` is the single “normal” integration-test binary, and it pulls in modules under `tests/` (directories and `mod.rs` files).
  - Run: `bash scripts/cargo_agent.sh test -p fastrender --test integration`
- **Special harness** (separate binary):
  - `tests/allocation_failure.rs` — allocation-failure tests (custom `#[global_allocator]`)
  - Run: `bash scripts/cargo_agent.sh test -p fastrender --test allocation_failure`

Rules (post-cleanup):

- **Do not add new top-level `tests/*.rs` files.** Only the two harness entrypoints above are allowed; add new integration tests as modules under `tests/` and include them from `tests/integration.rs`.
- **Do not use `#[path = ...]` shims.** Use normal Rust modules (`mod ...;`) and run subsets via test name filters (see below).
- If a test needs access to internal/private implementation details, it is a **unit test** and belongs in `src/`, not `tests/`.

CI guardrail:

- Run `bash scripts/ci_check_test_architecture.sh` before pushing changes that touch `tests/`. It enforces the 2 integration-test-binary architecture and bans `#[path = "..."]` shims / `Cargo.toml` `[[test]]` entries. See `docs/test_architecture.md`.

## Core tests

- Unit only (fast): `bash scripts/cargo_agent.sh test --quiet -p fastrender --lib`
- Integration only: `bash scripts/cargo_agent.sh test --quiet -p fastrender --test integration`
- Allocation failure: `bash scripts/cargo_agent.sh test --quiet -p fastrender --test allocation_failure`

To run a subset, pass a test-name filter:

```bash
# Unit tests:
bash scripts/cargo_agent.sh test -p fastrender --lib <filter>

# Paint unit tests only:
bash scripts/cargo_agent.sh test -p fastrender --lib paint::tests

# Integration tests:
bash scripts/cargo_agent.sh test -p fastrender --test integration <filter>

# Exact match (avoid accidental substring matches):
bash scripts/cargo_agent.sh test -p fastrender --test integration my::test::name -- --exact
```

Note: `scripts/cargo_agent.sh test` caps `RUST_TEST_THREADS` on very large machines. Override with
`FASTR_RUST_TEST_THREADS` / `RUST_TEST_THREADS` if you need a different setting.

- Full suite (CI feature-set; extremely expensive): `bash scripts/cargo_agent.sh test --features ci` (do **not** run this on agent hosts; see `AGENTS.md`).

## Fonts

Text shaping and many rendering/layout tests assume at least one usable font is available.
CI forces bundled, license-compatible fixtures (`tests/fixtures/fonts/`) so goldens stay
deterministic across platforms. Set `FASTR_USE_BUNDLED_FONTS=1` locally to match CI output.

- When relying on platform fonts, install a basic package (e.g. `fonts-dejavu-core` on
  Ubuntu/Debian). Desktop OSes typically have usable defaults preinstalled.
- The public API exposes `FastRenderConfig::with_font_sources(FontConfig::...)` to pin renders
  to bundled fonts or add additional font directories when needed.

## Media fixtures (video/audio)

Deterministic video/audio assets used by tests and offline fixtures live under
`tests/pages/fixtures/assets/media/`.

For provenance, licensing, size budgets, and regeneration commands, see:
[`tests/pages/fixtures/assets/media/README.md`](../tests/pages/fixtures/assets/media/README.md).

## Style regression tests

Most style/cascade regressions are unit tests in `src/`. Run them with a filter:

- Run style-related unit tests: `bash scripts/cargo_agent.sh test --quiet -p fastrender --lib style::`

These tests cover targeted style/cascade/layout regressions.

## Fixture renders (goldens)

Fixture tests render HTML fixtures under `tests/fixtures/html/*.html` (auto-discovered; top-level only, excluding `tests/fixtures/html/js/**`) and write/read golden PNGs under `tests/fixtures/golden/`. They are compiled into the main integration test binary.

- Run fixtures: `bash scripts/cargo_agent.sh test -p fastrender --test integration fixtures::runner::fixtures_regression_suite -- --exact`
- (Re)generate goldens: `UPDATE_GOLDEN=1 bash scripts/cargo_agent.sh test -p fastrender --test integration fixtures::runner::fixtures_regression_suite -- --exact`
- Run a single fixture (faster): `FIXTURES_FIXTURE=block_simple bash scripts/cargo_agent.sh test -p fastrender --test integration fixtures::runner::fixtures_regression_suite -- --exact`
- Run a subset: `FIXTURES_FILTER=block_simple,flex_direction bash scripts/cargo_agent.sh test -p fastrender --test integration fixtures::runner::fixtures_regression_suite -- --exact`

Rendered output is compared pixel-by-pixel against the checked-in PNG goldens. Failures write artifacts under `target/fixtures_diffs/<fixture>_{actual,expected,diff}.png` for debugging.

### DPR=2 goldens

If a fixture has a `tests/fixtures/golden/<name>_dpr2.png` golden, the harness automatically runs an additional render with `dpr=2` and compares/updates that golden as well. The CSS viewport is inferred from the golden PNG dimensions (`png_size / dpr`); DPR2 goldens must have pixel dimensions divisible by 2.

Comparisons are strict by default. To allow small local differences (fonts, GPU, AA), set a tolerance env var:

- `FIXTURE_TOLERANCE=5` (per-channel tolerance)
- `FIXTURE_MAX_DIFFERENT_PERCENT=0.5` (percent of pixels allowed to differ)
- `FIXTURE_FUZZY=1` (preset: tolerance 10, up to 1% different, no alpha compare, max perceptual distance 0.05; thresholds are empirical)
- `FIXTURE_IGNORE_ALPHA=1` (ignore alpha differences even without fuzzy)
- `FIXTURE_MAX_PERCEPTUAL_DISTANCE=0.05` (allow minor perceptual differences using a windowed-SSIM distance over downsampled luminance; thresholds are empirical)

New columns/transform/form fixtures ship with checked-in goldens; keep these up to date when adjusting layouts.

## Offline page regression suite

- Run: `bash scripts/cargo_agent.sh test -p fastrender --test integration regression::pages::pages_regression_suite -- --exact`
- Refresh goldens: `bash scripts/cargo_agent.sh xtask update-goldens pages` (or `UPDATE_PAGES_GOLDEN=1 bash scripts/cargo_agent.sh test -p fastrender --test integration regression::pages::pages_regression_suite -- --exact`)

This suite renders a curated set of realistic pages under `tests/pages/fixtures/` (flex/grid/table, multicol, pagination, masks/filters, SVG, writing modes, form controls, plus a positioned-child regression) and compares them against goldens in `tests/pages/golden/`.

Artifacts and reports for failures land in `target/pages-output/`:

- Per-failure PNGs: `<golden_name>_{actual,expected,diff}.png`
- Aggregated report: `report.html` + `report.json`

Comparison defaults to strict pixel matching but respects the same knobs as the fixture harness with `PAGES_TOLERANCE`, `PAGES_MAX_DIFFERENT_PERCENT`, `PAGES_FUZZY=1`, `PAGES_IGNORE_ALPHA=1`, and `PAGES_MAX_PERCEPTUAL_DISTANCE=0.05`.

Per-fixture tolerance overrides live in `tests/pages/overrides.toml` (values are treated as minimums, i.e. `max(existing, override)`).

The suite is fail-fast by default. To collect a richer aggregated report:

- `PAGES_REPORT=1` — always write `target/pages-output/report.{html,json}` (even if there are no failures)
- `PAGES_FAIL_FAST=0` — keep going after failures (still fails at the end)
- `PAGES_MAX_FAILURES=<N>` — stop after N failures and write a report (still fails at the end)

### Chrome baselines + evidence reports (offline fixtures)

When doing accuracy work, it’s often useful to compare an offline fixture render against Chrome using the **same** deterministic inputs (no network). Chrome baselines are **local-only artifacts** and are not committed.

#### Headless Chrome + `RLIMIT_AS` (virtual address space)

Several xtask workflows in this section spawn headless Chrome (`page-loop --chrome`, `chrome-baseline-fixtures`, `fixture-chrome-diff`, and `refresh-progress-accuracy` when it refreshes Chrome baselines).

On Linux we run most commands under an address-space limit (`RLIMIT_AS`) to protect agent hosts from pathological allocations (see [resource-limits.md](resource-limits.md)). Modern headless Chrome reserves a very large virtual address space up front (**>64GiB**, ~**75GiB** on Chrome 143), even when its RSS is still small. If `RLIMIT_AS` is too low, Chrome can fail immediately with an error containing:

```
Oilpan: Out of memory
```

`bash scripts/cargo_agent.sh` keeps the default cap at `64G` for normal Cargo commands, but bumps it to `96G` for xtask runs (`bash scripts/cargo_agent.sh xtask ...`; configurable via `FASTR_XTASK_LIMIT_AS`).

If you hit an Oilpan OOM (or are running a particularly large fixture set), rerun the command with a higher xtask address-space cap:

```bash
FASTR_XTASK_LIMIT_AS=128G bash scripts/cargo_agent.sh xtask fixture-chrome-diff
# Or disable the address-space cap entirely (less safe on shared hosts):
FASTR_XTASK_LIMIT_AS=unlimited bash scripts/cargo_agent.sh xtask page-loop --fixture bbc.co.uk --chrome
```

If you are invoking `scripts/run_limited.sh` directly, override its default with `--as ...` or `LIMIT_AS=...`:

```bash
LIMIT_AS=128G bash scripts/run_limited.sh -- \
  bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures
```

For determinism, the fixture baseline step patches the HTML before loading it in Chrome:

- Forces a light color scheme and white `html/body` background (to match FastRender’s default white canvas and avoid platform dark-mode defaults).
- Injects a CSP that disables JS by default and blocks `http(s)` subresources so the run stays offline.

Pass `bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures --allow-dark-mode` to opt out of the light-mode/background enforcement when debugging dark-mode pages.

```bash
# One-command evidence report (runs FastRender render + Chrome baseline + diff):
bash scripts/cargo_agent.sh xtask fixture-chrome-diff
# Report: target/fixture_chrome_diff/report.html
# Defaults to the curated pages_regression fixture set from tests/regression/pages.rs.
# Pass --all-fixtures to render everything under tests/pages/fixtures instead.

# Convenience wrapper for the same command (delegates to `bash scripts/cargo_agent.sh xtask fixture-chrome-diff` and
# inherits its validations/selection logic):
scripts/chrome_vs_fastrender_fixtures.sh

# 1) Render the offline fixture(s) with FastRender (offline; bundled fonts):
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin render_fixtures -- --out-dir target/fixture_chrome_diff/fastrender

# 2) Produce local Chrome baseline PNGs for those fixture(s):
bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures --out-dir target/fixture_chrome_diff/chrome

# 3) Generate a combined Chrome-vs-FastRender HTML report under target/.
# Re-runs steps 1-2 by default. Pass `--no-chrome` to reuse `target/fixture_chrome_diff/chrome`.
# Pass `--no-build` to reuse an existing `diff_renders` binary under the selected Cargo profile
# (`target/release` by default; pass `--debug` to use `target/debug` for faster rebuilds).
bash scripts/cargo_agent.sh xtask fixture-chrome-diff

# 4) Sync deterministic pixel/perceptual diff telemetry into `progress/pages/*.json`:
bash scripts/cargo_agent.sh xtask sync-progress-accuracy --report target/fixture_chrome_diff/report.json

# `pageset_progress report --rank-accuracy` now reflects the deterministic fixture diffs.
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin pageset_progress -- report --rank-accuracy
```

Preferred workflow for refreshing the committed accuracy telemetry:

```bash
# Runs `fixture-chrome-diff`, then syncs `<out>/report.json` into progress/pages/*.json, and prints
# the current top-10 worst accuracy entries.
bash scripts/cargo_agent.sh xtask refresh-progress-accuracy

# Refresh only the top-N worst accuracy pages from the existing progress JSON:
bash scripts/cargo_agent.sh xtask refresh-progress-accuracy --from-progress progress/pages --top-worst-accuracy 10

# Refresh in deterministic shards so multiple workers/machines can run in parallel:
bash scripts/cargo_agent.sh xtask refresh-progress-accuracy --from-progress progress/pages --only-failures --top-worst-accuracy 1000000 --min-diff-percent 0 --shard 0/8 --keep-going --out-dir target/refresh_progress_accuracy_0_8
```

When driving this from pageset runs, you can select the relevant fixtures directly from the
committed `progress/pages/*.json` artifacts:

```bash
# Diff fixtures for pages that currently fail in progress/pages/*.json:
bash scripts/cargo_agent.sh xtask fixture-chrome-diff --from-progress progress/pages --only-failures

# Diff the top N worst accuracy pages (requires progress files generated with
# `pageset_progress run --accuracy ...`):
bash scripts/cargo_agent.sh xtask fixture-chrome-diff --from-progress progress/pages --top-worst-accuracy 10
```

Viewport defaults differ depending on which baseline you are trying to match:

- **Offline fixture workflows** (`xtask fixture-chrome-diff`, `xtask page-loop`, and the `pages_regression` test suite) default to `viewport=1040x1240`, `dpr=1.0`.
- **Pageset progress accuracy refresh** (`xtask refresh-progress-accuracy`, which updates committed `progress/pages/*.json`) defaults to `viewport=1200x800`, `dpr=1.0` to match the pageset progress Chrome baselines.

Override `--viewport WxH` on the fixture workflows when you want to compare against the other baseline.
For example:

```bash
# Run deterministic fixture diffs at the pageset progress baseline viewport (1200x800):
bash scripts/cargo_agent.sh xtask fixture-chrome-diff --viewport 1200x800

# If you intentionally want to refresh committed progress accuracy using the fixture viewport
# (1040x1240) instead of the pageset baseline, run the underlying steps manually:
bash scripts/cargo_agent.sh xtask fixture-chrome-diff --viewport 1040x1240 --out-dir target/fixture_chrome_diff_1040x1240
bash scripts/cargo_agent.sh xtask sync-progress-accuracy --report target/fixture_chrome_diff_1040x1240/report.json --progress-dir progress/pages
```

For determinism, the Chrome baseline step disables CSS animations/transitions by default (via an injected `<style>` block). This both reduces screenshot frame-timing noise and keeps Chrome baselines aligned with FastRender’s “no animation” model. Opt out for debugging with:

- `bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures --allow-animations`

### Determinism audits (offline fixtures)

When diagnosing paint nondeterminism (often scheduling-dependent under high parallelism), there are two complementary harnesses:

- Multi-process run-to-run diffs with an HTML report: `bash scripts/cargo_agent.sh xtask fixture-determinism`
- In-process repeat/shuffle harness (captures raw `Pixmap::data()` bytes; can save per-variant PNGs):

Tip: pass `--debug` to `fixture-determinism` (or `fixture-chrome-diff`) to skip `--release` for the
FastRender/diff steps when you want faster rebuilds while iterating locally (slower runtime).

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin render_fixtures -- \
  --fixtures preserve_3d_stack --jobs 8 \
  --repeat 10 --shuffle --fail-on-nondeterminism --save-variants
```

Artifacts and PR guidance:

- Report: `target/fixture_chrome_diff/report.html` (plus `report.json` and per-fixture PNG/log/metadata artifacts under `target/fixture_chrome_diff/{chrome,fastrender,...}`).
  - FastRender writes `<out>/fastrender/<fixture>.json` alongside each PNG with render settings (viewport/DPR/media/fit-canvas-to-content/timeout, fonts) and status/timing.
  - When reusing an existing FastRender output dir (`bash scripts/cargo_agent.sh xtask fixture-chrome-diff --no-fastrender`), xtask validates that the per-fixture metadata matches the current `--viewport/--dpr/--media/--fit-canvas-to-content/--timeout` and font config. Mismatches fail fast to prevent misleading diffs against stale renders.
  - If a metadata file is missing (or incomplete), xtask warns and continues by default for backwards compatibility; pass `--require-fastrender-metadata` to fail instead.
- Re-run without invoking Chrome (reuse existing renders under `target/fixture_chrome_diff/chrome`): `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --no-chrome`.
- Exit non-zero when diffs are found (useful for gating local scripts): `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --fail-on-differences`.
- Write FastRender pipeline snapshots for later `diff_snapshots`: `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --write-snapshot` (writes under `target/fixture_chrome_diff/fastrender/<fixture>/snapshot.json`).
- **Do not commit** Chrome baseline PNGs or diff reports; they are local artifacts. Attach the generated report directory (or at least `report.html` + the referenced PNGs) to your PR description instead.
- **Do commit** new/updated fixtures under `tests/pages/fixtures/<fixture>/` when they are part of the regression story.

#### Comparing two Chrome-vs-FastRender reports (delta)

When iterating on correctness, it’s often useful to quantify “did accuracy vs Chrome improve overall?” between two runs. You can compare two `diff_renders` reports (including `fixture-chrome-diff`’s `report.json`) with `compare_diff_reports`:

```bash
# On a baseline commit:
bash scripts/cargo_agent.sh xtask fixture-chrome-diff --out-dir target/fixture_chrome_diff_before

# On your current commit:
bash scripts/cargo_agent.sh xtask fixture-chrome-diff --out-dir target/fixture_chrome_diff_after

# Summarize deltas (improvements/regressions) between the two reports:
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin compare_diff_reports -- \
  --baseline target/fixture_chrome_diff_before/report.json \
  --new target/fixture_chrome_diff_after/report.json \
  --json target/fixture_chrome_diff_delta/report.json \
  --html target/fixture_chrome_diff_delta/report.html

# Optional gating (exit non-zero on regressions):
#   --fail-on-regression --regression-threshold-percent 0.05
# Optional filtering (only compare selected entries by name regex):
#   --include '^my_fixture_name$'
```

Note: the delta tool expects the two reports to use the same diff settings (`--tolerance`, `--max-diff-percent`, `--max-perceptual-distance`, `--ignore-alpha`) and the same sharding settings when applicable (e.g. both generated with `--shard 0/4`). If your `report.json` lives in a different directory than the corresponding `report.html`, pass `--baseline-html` / `--new-html` so diff thumbnails resolve correctly.

The same delta tool also works with cached-page reports from `scripts/chrome_vs_fastrender.sh` (best-effort / non-deterministic because live subresources can change):

```bash
# On a baseline commit:
scripts/chrome_vs_fastrender.sh --out-dir target/chrome_vs_fastrender_before --pages example.com

# On your current commit:
scripts/chrome_vs_fastrender.sh --out-dir target/chrome_vs_fastrender_after --pages example.com

# Summarize deltas between the two runs:
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin compare_diff_reports -- \
  --baseline target/chrome_vs_fastrender_before/report.json \
  --new target/chrome_vs_fastrender_after/report.json \
  --json target/chrome_vs_fastrender_delta/report.json \
  --html target/chrome_vs_fastrender_delta/report.html
```

#### CI option (no local Chrome required)

If you can’t install Chrome/Chromium locally, the repository provides an **optional** GitHub Actions workflow that generates the same deterministic fixture-vs-Chrome diff report and uploads it as an artifact:

- Workflow: `.github/workflows/chrome_fixture_diff.yml`
- Artifact: `fixture_chrome_diff_ci`
- Report path inside the artifact: `target/fixture_chrome_diff_ci/report.html`

This is intended as a convenient way to attach evidence to PRs without requiring every contributor to have Chrome installed locally.

### Importing new offline page fixtures

Use `bundle_page` to capture a page once, then convert that bundle into a deterministic fixture consumable by `pages_regression`:

1. Capture a bundle:
   - Online (network): `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bundle_page -- fetch <url> --out /tmp/capture.tar` (or a directory path)
        - If a page crashes or times out during capture, add `--no-render` to crawl HTML + CSS for subresources without doing a full render.
        - Note: crawl mode also discovers media sources (`<video src>`, `<audio src>`, `<source src>`, `<track src>`). Add `--prefetch-media` (alias `--include-media`) if you need media bytes in the bundle; render-mode capture may not fetch media sources yet.
   - Offline (from warmed pageset caches): `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --features disk_cache --bin bundle_page -- cache <stem> --out /tmp/capture.tar`
        - Reads HTML from `fetches/html/<stem>.html` and subresources from the disk-backed cache under `fetches/assets/` (override with `--asset-cache-dir` / `--cache-dir`).
2. Import: `bash scripts/cargo_agent.sh xtask import-page-fixture /tmp/capture.tar <fixture_name> [--output-root tests/pages/fixtures --overwrite --dry-run --include-media]`
3. Validate the imported fixture is fully offline (no fetchable `http(s)` URLs left behind): `bash scripts/cargo_agent.sh xtask validate-page-fixtures --only <fixture_name>`
4. Add the new fixture to `tests/regression/pages.rs` and generate a golden if you want it covered by the suite.

The importer rewrites all HTML/CSS references to hashed files under `assets/` and refuses to leave `http(s)` URLs behind, so the resulting directory is fully offline. A synthetic bundle for testing lives under `tests/fixtures/bundle_page/simple`, and `tests/pages/fixtures/bundle_import_example/` shows the expected output produced by the importer.

Note: media sources (`<video src>`, `<audio src>`, `<source src>`, `<track src>`) are rewritten to deterministic empty `assets/missing_<hash>.<ext>` placeholder files by default so fixtures stay small. Use `--include-media` to vendor playable media, subject to size budgets (`--media-max-bytes` default **5 MiB** total, `--media-max-file-bytes` default **2 MiB** per file; set either to `0` to disable).

Tip: if you already have a warmed pageset disk cache, `bash scripts/cargo_agent.sh xtask pageset --capture-missing-failure-fixtures` can automatically capture/import missing fixtures for pages that currently fail in `progress/pages/*.json` (it uses `bundle_page cache` + `bash scripts/cargo_agent.sh xtask import-page-fixture` under the hood).

Tip: once a pageset run has populated `progress/pages/*.json` with `accuracy` metrics, you can also auto-capture/import fixtures for the most visually incorrect **ok** pages:

```bash
# Populate progress JSON + per-page accuracy metrics, then capture/import offline fixtures for the
# worst-diff ok pages (missing fixtures only by default).
bash scripts/cargo_agent.sh xtask pageset --disk-cache --capture-worst-accuracy-fixtures -- --accuracy

# Iterate locally with deterministic fixture-vs-Chrome diffs:
bash scripts/cargo_agent.sh xtask fixture-chrome-diff
```

The standalone workflow (`bash scripts/cargo_agent.sh xtask capture-accuracy-fixtures`) is also available when you want to tune selection thresholds (`--min-diff-percent`, `--top`) or reuse an existing warmed cache directory.

## test262 parser harness (optional)

When working on JavaScript parsing (via the vendored `vendor/ecma-rs`), the repository provides an **optional** GitHub Actions workflow that runs the `test262` parser harness and uploads a JSON report artifact:

- Local workflow: see [js_test262_parser.md](js_test262_parser.md) (`bash scripts/cargo_agent.sh xtask js test262-parser`).

- Workflow: `.github/workflows/test262_parser.yml`
- Artifact: `test262_parser_report`
- Report path inside the artifact: `target/js/test262-parser.json`

## WPT harness (local, visual)

There is a self-contained WPT-style runner under `tests/wpt/` for local “render and compare” tests. It does not talk to upstream WPT and never fetches from the network.

- Run: `bash scripts/cargo_agent.sh xtask test wpt` (or `bash scripts/cargo_agent.sh test --quiet -p fastrender --test integration wpt::wpt_local_suite_passes -- --exact`)
- Run a scoped subset (fast iteration) via `WPT_FILTER`:
  - `WPT_FILTER=layout/floats bash scripts/cargo_agent.sh test --quiet -p fastrender --test integration wpt::wpt_local_suite_passes -- --exact`
  - `WPT_FILTER=paint/stacking bash scripts/cargo_agent.sh test --quiet -p fastrender --test integration wpt::wpt_local_suite_passes -- --exact`
  - `WPT_FILTER=paint/overflow bash scripts/cargo_agent.sh test --quiet -p fastrender --test integration wpt::wpt_local_suite_passes -- --exact`
  - `WPT_FILTER=css/tables bash scripts/cargo_agent.sh test --quiet -p fastrender --test integration wpt::wpt_local_suite_passes -- --exact`
- Each rendered document is given a per-document `file://` base URL (the test HTML path for the test render, and the reference HTML path for the reference render) so relative resources like `support/*.css`, images, and fonts resolve reliably regardless of the current working directory.
- `WptRunnerBuilder::build()` defaults to an offline renderer (`ResourcePolicy` with `http/https` disabled). Advanced callers can still inject a custom renderer via `.renderer(...)`.

- Discovery reads sidecar metadata next to tests:
  - `.html.ini` files set expectations (`expected: FAIL`), `disabled` reasons, timeouts, viewport, and DPR.
  - `<link rel="match" | rel="mismatch">` inside HTML declares reftest references without touching the manifest.
  - The legacy `tests/wpt/manifest.toml` is still honored; set `HarnessConfig::with_discovery_mode(DiscoveryMode::MetadataOnly)` to ignore it when adding new offline WPT dumps.
- New curated tests should start as `expected = "fail"` in `tests/wpt/manifest.toml` until the underlying primitive is implemented. Once a test starts passing, flip it to `expected = "pass"` (the harness treats “unexpected pass” as a failure so CI stays honest).
- The `wpt_local_suite_passes` smoke-test suite is strict by default: visual tests compare against checked-in PNGs under `tests/wpt/expected/` and fail on diffs. Set `UPDATE_WPT_EXPECTED=1` (or run `bash scripts/cargo_agent.sh xtask update-goldens wpt`) to regenerate/update those goldens. (Optional: `WPT_EXPECTED_DIR` overrides the baseline directory for local experimentation.)
- Artifacts always land in `target/wpt-output/<id>/{actual,expected,diff}.png` with `report.html` + `report.json` for debugging and tooling.
- Viewport/DPR are fixed per-test from metadata. CI can pin fonts for deterministic renders via `HarnessConfig::with_font_dir`/`WptRunnerBuilder::font_dir` (for example, point at `tests/fonts/`).
- The runner supports parallel execution and per-test timeouts (see `HarnessConfig`).
  - Each render is executed with a per-document timeout via `RenderOptions::with_timeout(...)`, derived from the manifest/INI metadata (falling back to `HarnessConfig::default_timeout_ms`).
  - These timeouts are *aborting* (best-effort cooperative deadlines inside the renderer) and are intended to prevent pathological inputs from hanging the entire WPT run.
- Comparisons use the shared image comparison module (same as fixtures/ref tests) with configurable tolerance, alpha handling, pixel difference thresholds, and perceptual distance thresholds to reduce platform noise.
  - Defaults are strict: `tolerance=0`, `max_different_percent=0.0`, `compare_alpha=true`, and no perceptual threshold.
  - Local overrides (env vars):
    - `WPT_TOLERANCE=5` (per-channel tolerance)
    - `WPT_MAX_DIFFERENT_PERCENT=0.5` (percent of pixels allowed to differ)
    - `WPT_FUZZY=1` (preset: tolerance 10, up to 1% different, no alpha compare, max perceptual distance 0.05; thresholds are empirical)
    - `WPT_IGNORE_ALPHA=1` (ignore alpha differences even without fuzzy)
    - `WPT_MAX_PERCEPTUAL_DISTANCE=0.05` (allow minor perceptual differences using a windowed-SSIM distance over downsampled luminance; thresholds are empirical)

### WPT importer (offline)

Use `import_wpt` to bring small slices of upstream WPT into `tests/wpt/tests/` without curating each support file by hand. The importer is entirely file-based and rewrites absolute URLs so tests work offline.

- Example (against a local WPT checkout): `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --bin import_wpt -- --wpt-root ~/code/wpt --suite css/css-text/white-space --out tests/wpt/tests`
- `--suite` can be repeated and supports directories, individual files, and globs (e.g. `--suite css/css-text/* --suite html/semantics/forms/the-input-element/input-type-number.html`).
- Preview changes without writing: add `--dry-run`
- Update existing files/manifest entries: add `--overwrite`
- Control metadata: `--manifest <path>` overrides the default `tests/wpt/manifest.toml`; `--no-manifest` skips updates
- Allow leaving network URLs in imported HTML/CSS (not recommended; disables offline validation): `--allow-network`
- Enable extra strict offline validation (optional): `--strict-offline`
  - In addition to the default “no fetchable network URLs” checks, this scans the rewritten HTML/CSS for any remaining `http(s)://` or protocol-relative (`//`) URL strings (excluding `data:` URLs) and fails the import if any are found.
  - Useful for catching unusual leftover references that the targeted validators may miss (for example, network-looking strings outside typical `src=`/`href=`/`srcset=`/CSS `url()` contexts).
- Offline behavior (important for deterministic tests):
  - Root-relative URLs (e.g. `/resources/foo.png`) and `web-platform.test` URLs are rewritten to file-relative paths inside the imported tree.
  - Rewrites cover common fetchable HTML/CSS URL contexts, including:
    - HTML: `src`, fetchable `href` contexts such as `<link href=...>` (navigation links like `<a href=...>` are ignored), `poster`, `object[data]`, `srcset`, and `imagesrcset`.
    - SVG: `href` / `xlink:href` on fetchable elements (e.g. `<image>`, `<use>`, `<feImage>`); navigation links like `<a xlink:href>` are ignored.
    - CSS: `url(...)` and `@import`.
  - Sidecar metadata files (e.g. `.html.ini`) are copied alongside tests when present so the local WPT harness can apply expectations/viewport/DPR.
  - The importer is strict: missing referenced files fail the import rather than silently leaving network URLs behind.

A tiny synthetic WPT-like tree lives under `tests/wpt/_import_testdata/` and is exercised in CI. You can sanity check the importer locally via:

```
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --bin import_wpt -- \
  --wpt-root tests/wpt/_import_testdata/wpt \
  --suite css/simple \
  --out /tmp/fastrender-wpt-import \
  --manifest /tmp/fastrender-wpt-import/manifest.toml
```

## Fuzzing (CSS parsing and selectors)

Structured fuzzers live under `fuzz/` and target crash-prone areas in CSS parsing, selector matching, and custom property resolution.

- Install tooling once: `bash scripts/cargo_agent.sh install cargo-fuzz`
- Note: `cargo-fuzz` defaults to AddressSanitizer, which reserves a very large virtual address
  space for shadow memory. `scripts/cargo_agent.sh` automatically bumps RLIMIT_AS for `fuzz`
  subcommands; override with `FASTR_FUZZ_LIMIT_AS` / `FASTR_CARGO_LIMIT_AS` if needed.
- Quick local run with a short time budget: `bash scripts/cargo_agent.sh fuzz run css_parser -- -runs=1000`
- Selector matching and variable/calc parsing are covered by the `selectors` and `vars_and_calc` targets.
- Seed corpora with real-world CSS live in `tests/fuzz_corpus/`; pass them as extra corpus paths (e.g. `bash scripts/cargo_agent.sh fuzz run selectors fuzz/corpus/selectors tests/fuzz_corpus`).

The fuzz harnesses are optimized for fast iterations and can be run in CI with a low `-max_total_time` when needed.
