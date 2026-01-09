# JS test262 semantics runner (curated)

FastRender tracks JavaScript language conformance via a **curated**, offline-friendly subset of
test262 (the upstream ECMAScript conformance suite).

This page documents the end-to-end workflow contributors should follow to run the suite locally,
under deterministic resource limits, and interpret the output.

## 1) Initialize required submodules

From the FastRender repo root:

```bash
# JS engine (submodule)
git submodule update --init engines/ecma-rs

# test262 semantics corpus (nested submodule inside ecma-rs)
git -C engines/ecma-rs submodule update --init test262-semantic/data

# Alternative (pull all nested submodules in ecma-rs):
# git -C engines/ecma-rs submodule update --init --recursive
```

## 2) Run the curated suite

From the FastRender repo root:

```bash
bash scripts/cargo_agent.sh xtask js test262
```

`bash scripts/cargo_agent.sh xtask js test262` is a thin wrapper over the **ecma-rs**
[`test262-semantic`](../engines/ecma-rs/test262-semantic/README.md) runner. It forwards the actual
test execution/reporting to the submodule and pins the corpus directory via:
`--test262-dir engines/ecma-rs/test262-semantic/data`.

Note: the `engines/ecma-rs` submodule does not commit a `Cargo.lock`, so the child invocation inside
the submodule is intentionally **not** run with `--locked` (even if you run the top-level xtask with
`--locked`).

By default, the runner writes a JSON report to:

```
target/js/test262.json
```

Run `bash scripts/cargo_agent.sh xtask js test262 --help` for the full, authoritative CLI (this doc only calls out the
flags we use the most).

## Key flags

- `--suite <SUITE>`
  - Select which preset suite to run (the default is the curated suite).
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

## Interpreting the output (expected vs unexpected)

The suite contains tests with an **expected outcome**. The runner records the **actual outcome**
and marks the test as a **mismatch** when they disagree.

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
