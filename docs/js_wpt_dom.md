# JS WPT DOM runner (offline `testharness.js`)

FastRender tracks DOM + event-loop JavaScript conformance via a **curated**, fully-offline subset of
WPT `testharness.js` tests under [`tests/wpt_dom/`](../tests/wpt_dom/).

## Run the suite

From the FastRender repo root:

```bash
bash scripts/cargo_agent.sh xtask js wpt-dom
```

By default, this writes a JSON report to:

```text
target/js/wpt_dom.json
```

## Key flags

Run `bash scripts/cargo_agent.sh xtask js wpt-dom --help` for the full CLI. Common flags:

- `--suite <curated|smoke|all>`
  - Select which preset suite to run.
  - `curated` (default) runs the green subset (excludes harness bring-up intentional failures).
- `--wpt-root <DIR>`
  - Defaults to `tests/wpt_dom`.
- `--manifest <PATH>`
  - Defaults to `tests/wpt_dom/expectations.toml`.
  - Uses the ecma-rs `conformance-harness` expectations format (`pass|skip|xfail|flaky`).
- `--report <PATH>`
  - Defaults to `target/js/wpt_dom.json`.
- `--shard <index>/<total>`
  - Run a deterministic shard (0-based index).
- `--timeout-ms <MS>` / `--timeout-secs <SECS>`
  - Per-test timeout (defaults to 5000ms).
- `--long-timeout-ms <MS>` / `--long-timeout-secs <SECS>`
  - Per-test timeout for `timeout=long` tests (defaults to 30000ms).
- `--fail-on <all|new|none>`
  - `new` (default): fail only on **unexpected** mismatches (not covered by the manifest).
  - `all`: fail on any mismatch (including expected/xfail/flaky).
  - `none`: always exit 0.
- `--filter <GLOB|REGEX>`
  - Filter tests by id (e.g. `smoke/**` or `event_loop/**`).
- `--backend <auto|vmjs>`
  - Choose which JS backend to execute with (`vm-js` is the only available backend today).

## What gets executed

The harness currently runs:

- `*.window.js`
- `*.any.js` (executed in a window-like realm)
- HTML testharness files (`*.html` / `*.htm` / `*.xhtml`)

Other JS variants (worker/serviceworker/sharedworker) are discovered but currently reported as
**skipped**.
