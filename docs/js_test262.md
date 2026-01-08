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
cargo xtask js test262
```

`cargo xtask js test262` is a thin wrapper over the **ecma-rs**
[`test262-semantic`](../engines/ecma-rs/test262-semantic/README.md) runner. It forwards the actual
test execution/reporting to the submodule and pins the corpus directory via:
`--test262-dir engines/ecma-rs/test262-semantic/data`.

Note: the `engines/ecma-rs` submodule does not commit a `Cargo.lock`, so the child `cargo run -p
test262-semantic ...` invocation is intentionally **not** run with `--locked` (even if you run the
top-level xtask with `--locked`).

By default, the runner writes a JSON report to:

```
target/js/test262.json
```

Run `cargo xtask js test262 --help` for the full, authoritative CLI (this doc only calls out the
flags we use the most).

## Key flags

- `--suite <SUITE>`
  - Select which preset suite to run (the default is the curated suite).
- `--manifest <PATH>`
  - Override the expectations manifest (skip/xfail/flaky) used to classify known gaps.
  - When omitted, xtask defaults to: `tests/js/test262_semantic_expectations.toml`.
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
`tests/js/test262_semantic_expectations.toml`) is how we track known gaps without hiding
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
