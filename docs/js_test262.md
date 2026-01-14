# JS test262 semantics runner (curated)

FastRender tracks JavaScript language conformance via a **curated**, offline-friendly subset of
test262 (the upstream ECMAScript conformance suite).

This page documents the end-to-end workflow contributors should follow to run the suite locally,
under deterministic resource limits, and interpret the output.

## 1) Initialize required submodules

From the FastRender repo root:

```bash
# test262 semantics corpus (submodule inside vendored ecma-rs)
git submodule update --init vendor/ecma-rs/test262-semantic/data
```

## 2) Run the curated suite

From the FastRender repo root:

```bash
bash scripts/cargo_agent.sh xtask js test262
```

`bash scripts/cargo_agent.sh xtask js test262` is a thin wrapper over the **ecma-rs**
[`test262-semantic`](../vendor/ecma-rs/test262-semantic/README.md) runner. It forwards the actual
test execution/reporting to the vendored ecma-rs and pins the corpus directory via:
`--test262-dir vendor/ecma-rs/test262-semantic/data`.

By default, xtask runs the suite with the standard **test262 harness** (equivalent to
`--harness test262`): it prepends `harness/assert.js`, `harness/sta.js`, then any additional harness
files explicitly listed in each test’s frontmatter `includes` list. This matches the upstream
execution environment and avoids spurious failures from missing harness globals like `assert`.

If you need a smaller harness while debugging, pass `--harness includes` (alias: `minimal`): it
prepends only frontmatter `includes` (no implicit `assert.js`/`sta.js`). Note that many test262 tests
rely on the default harness without listing it in `includes`, so `includes` is not expected to be
green for most suites.

Note: the vendored `vendor/ecma-rs` does not commit a `Cargo.lock`, so the child invocation inside
that directory is intentionally **not** run with `--locked` (even if you run the top-level xtask with
`--locked`).

By default, the runner writes a JSON report to:

```
target/js/test262.json
```

Alongside the JSON report, xtask also writes a human-readable Markdown summary to:

```
target/js/test262_summary.md
```

Run `bash scripts/cargo_agent.sh xtask js test262 --help` for the full, authoritative CLI (this doc only calls out the
flags we use the most).

## Baseline + monotonic progress

FastRender keeps a **committed** baseline snapshot for the curated suite at:

```
progress/test262/baseline.json
```

`xtask js test262` compares the current run against this baseline and:

- fails CI on **new regressions** (tests that previously matched test262 expectations but now mismatch), unless they are
  explicitly classified in `tests/js/test262_manifest.toml` as `skip`/`xfail`/`flaky`.
- fails CI on **new timeouts/hangs**, even if the affected tests are `xfail` (timeouts waste CI time and often signal
  cancellation bugs). If a test is known to hang, prefer marking it as `skip` in the manifest.

### Updating the baseline intentionally

When you intentionally change expectations or land a large batch of conformance fixes, refresh the committed baseline:

```bash
bash scripts/cargo_agent.sh xtask js test262 --update-baseline
```

This updates (and you should commit) the files under `progress/test262/`:

- `baseline.json` (snapshot used for CI gating)
- `summary.md` (human-readable baseline summary)
- `trend.json` (aggregated per-area counts for easy diffs over time)

#### Fallback workflow (no `xtask` build required)

`xtask` depends on the top-level `fastrender` crate. If `fastrender` fails to compile for unrelated reasons (e.g. during
large refactors), you can still refresh the committed baseline artifacts from a `test262-semantic` JSON report:

```bash
# 1) Produce the JSON report directly via the vendored runner (from repo root):
cd vendor/ecma-rs
bash scripts/cargo_agent.sh run --release -p test262-semantic -- \
  --test262-dir test262-semantic/data \
  --harness test262 \
  --suite-path ../../tests/js/test262_suites/curated.toml \
  --manifest ../../tests/js/test262_manifest.toml \
  --timeout-secs 10 \
  --fail-on none \
  --report-path ../../target/js/test262.json
cd ../..

# 2) Regenerate the committed baseline + summary + trend (from repo root):
python3 tools/test262_update_baseline.py \
  --report target/js/test262.json \
  --baseline progress/test262/baseline.json
```

### Opting out locally

For local iteration where you only want a report+summary without baseline gating:

```bash
bash scripts/cargo_agent.sh xtask js test262 --fail-on none --no-gate
```

Or set `FASTR_TEST262_NO_GATE=1`.

## Key flags

- `--suite <SUITE>`
  - Select which preset suite to run (the default is the curated suite).
  - Presets live under `tests/js/test262_suites/`:
    - `curated` (default): union of the curated thematic suites below.
    - `smoke`: minimal always-green suite for quick local wiring checks.
    - `regexp`: RegExp engine conformance focused subset (named groups, indices, lookbehind, etc).
    - `regexp_unicode_sets`: RegExp `/v` (Unicode sets) focused subset.
    - `regexp_property_escapes_generated`: generated Unicode property escape corpus (large).
    - `regexp_lookbehind`: targeted suite for `regexp-lookbehind` support.
    - `language_statements`: control-flow + declaration statements.
    - `language_functions`: function declarations + generators + async.
    - `language_classes`: basic class declarations + inheritance.
    - `language_scopes`: lexical scope + directive prologue semantics.
    - `builtins_core`: core built-ins (Object/Array/String/Number/Boolean/Symbol).
    - `builtins_json_math`: JSON + Math built-ins.
- `--harness <test262|includes|none>`
  - Control how test sources are assembled:
    - `test262` (default): prepend `assert.js`, `sta.js`, then frontmatter `includes`.
    - `includes` (alias: `minimal`): prepend only frontmatter `includes` (no implicit `assert.js`/`sta.js`).
    - `none`: run the raw test body only (useful for early engine bring-up / debugging).
- `--manifest <PATH>`
  - Override the expectations manifest (skip/xfail/flaky) used to classify known gaps.
  - When omitted, xtask defaults to: `tests/js/test262_manifest.toml`.
- `--shard <index>/<total>`
  - Run a deterministic shard of the corpus (0-based index).
  - Example: `--shard 0/8` to run the first shard out of 8.
- `--timeout-secs <SECS>`
  - Per-test timeout (seconds). Used to prevent pathological inputs from hanging the run.
- `--fail-on <all|new|none>`
  - Controls which mismatches produce a non-zero exit code:
    - `new` (default): fail only on **unexpected** mismatches (not covered by the manifest).
    - `all`: fail on any mismatch (including expected/xfail/flaky).
    - `none`: always exit 0 (useful for generating reports while iterating).
- `--summary <PATH>`
  - Override where to write the Markdown summary (defaults to `target/js/test262_summary.md`).
- `--baseline <PATH>`
  - Override which baseline snapshot to compare against (defaults to `progress/test262/baseline.json`).
- `--update-baseline`
  - Refresh the committed baseline snapshot + summaries.
- `--no-gate`
  - Disable baseline regression/timeout gating (useful for local debugging).

## Running subsets

Run a thematic suite directly:

```bash
bash scripts/cargo_agent.sh xtask js test262 --suite language_statements

# RegExp-focused suite (named groups, indices, lookbehind, property escapes, String match/matchAll)
bash scripts/cargo_agent.sh xtask js test262 --suite regexp
```

Further narrow any suite using `--filter` (glob or regex on test ids):

```bash
# Only JSON tests (within the broader curated suite)
bash scripts/cargo_agent.sh xtask js test262 --suite curated --filter 'built-ins/JSON/**'

# Only `for-of` statement tests
bash scripts/cargo_agent.sh xtask js test262 --suite language_statements --filter 'language/statements/for-of/**'
```

## Debugging negative parse (early error) mismatches quickly

When working on the parser / early-error pipeline, a common regression is:

```
negative expectation mismatch: expected parse SyntaxError, got runtime ...
```

Running the full curated suite can take several minutes, so `xtask` provides a fast path that:

1. Expands the curated suite globs to concrete ids
2. Filters to tests with `negative: { phase: parse, type: SyntaxError }`
3. Writes a temporary suite TOML under `target/js/`
4. Rebuilds `test262-semantic` under `timeout -k 10 600` (to avoid stale results)
5. Executes the built `vendor/ecma-rs/target/debug/test262-semantic` binary directly under `timeout -k 10 600` and writes a JSON report
6. Prints a deterministic list of mismatching ids

From the FastRender repo root:

```bash
bash scripts/cargo_agent.sh xtask js test262-negative-parse
```

Artifacts:

- Generated suite: `target/js/test262_negative_parse_suite.toml`
- JSON report: `target/js/test262_negative_parse.json`

## Interpreting the output (expected vs unexpected)

The suite contains tests with an **expected outcome**. The runner records the **actual outcome**
and marks the test as a **mismatch** when they disagree.

### Deep recursion / stack overflow tests

Some test262 tests intentionally recurse extremely deeply (notably proper tail calls / TCO
feature tests). Until FastRender implements PTC, these tests are expected to fail with a
stack-overflow `RangeError` (e.g. “Maximum call stack size exceeded”).

What we specifically want to avoid is a **host abort** (Rust `fatal runtime error: stack overflow`)
or misclassifying stack overflow as a **timeout**. CI includes a dedicated deep-recursion smoke step
to ensure the runner always produces a JSON report and `timed_out` stays at 0 for these cases.

The expectations manifest (passed via `--manifest`, or defaulting to
`tests/js/test262_manifest.toml`) is how we track known gaps without hiding
regressions:

- **Unexpected mismatch**: not covered by the manifest.
  - Treat this as either a regression or a newly-discovered conformance issue.
  - With `--fail-on new` (default), *any* unexpected mismatch fails the command.
- **Expected mismatch**: covered by the manifest (typically `xfail`).
  - Acknowledged known gap; should have a clear `reason` and ideally a `tracking_issue`.
  - Does **not** fail the command with `--fail-on new`.
- **Flaky mismatch**: covered by the manifest as `flaky`.
  - Indicates nondeterministic behavior; prioritize making the engine deterministic or tighten the
    test harness so we can reclassify it.

If a test was previously marked `xfail` but now matches the expected outcome (an “xpass”), consider
removing or narrowing the corresponding manifest entry so future regressions aren’t hidden.

## Updating `tests/js/test262_manifest.toml`

When the JS engine behavior changes, update the expectations manifest so the default
`--fail-on new` mode continues to:

- fail on **regressions** (previously-green tests that now mismatch), and
- avoid failing on **known gaps** (still-incomplete semantics).

Recommended workflow:

1. Generate a fresh JSON report without failing the command:

   ```bash
   bash scripts/cargo_agent.sh xtask js test262 --fail-on none
   ```

2. Inspect `target/js/test262.json` and update `tests/js/test262_manifest.toml`:
   - Prefer **exact `id`** entries when promoting newly-green tests to `pass` (keeps regressions
     unexpected).
   - Prefer `glob` over `regex` when you must classify a broad set of known failures.
   - Always include a short `reason` (and add `tracking_issue` when you have a link).

3. Re-run the suite with the default behavior to confirm there are no unexpected mismatches:

   ```bash
   bash scripts/cargo_agent.sh xtask js test262
   ```
