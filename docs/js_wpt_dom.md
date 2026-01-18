# JS WPT DOM runner (offline `testharness.js`)

FastRender tracks DOM + event-loop JavaScript conformance via a **curated**, fully-offline subset of
WPT `testharness.js` tests under [`tests/wpt_dom/`](../tests/wpt_dom/).

For the `vmjs` backend, tests are executed inside the `vm-js`-backed [`api::BrowserTab`](../src/api/browser_tab.rs)
embedding (DOM + event loop + script scheduling). For context on which public containers include JS +
an event loop, see [`docs/runtime_stacks.md`](runtime_stacks.md).

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
    - Filter: `dom/**,domparsing/**,html/**,event*/**,observers/**,url/**`
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
  - Per-test timeout for `timeout=long` tests (defaults to 300000ms).
- `--fail-on <all|new|none>`
  - `new` (default): fail only on **unexpected** mismatches (not covered by the manifest).
  - `all`: fail on any mismatch (including expected/xfail/flaky).
  - `none`: always exit 0.
- `--filter <GLOB|SUBSTRING|REGEX>`
  - Filters tests by id.
  - If the value contains glob metacharacters (e.g. `dom/**`, `smoke/**`), it is treated as a glob.
  - Otherwise it is treated as a literal substring match (e.g. `mutation`).
  - Prefix with `re:` to force a regex, or `glob:` to force a glob.
- `--backend <auto|vmjs|vmjs-rendered>`
  - Choose which JS backend to execute with.
  - `vmjs-rendered` executes scripts against a renderer-backed document so layout/geometry tests can
    observe real values.
  - Note: `xtask js wpt-dom` builds the runner with the `vmjs` backends only (`vmjs` +
    `vmjs-rendered`).

## DOM bindings backend selection (vm-js)

FastRender currently has two DOM bindings backends for the `vm-js` host runtime:

- `handwritten` (default): legacy handwritten bindings.
- `webidl`: WebIDL-generated bindings (useful for end-to-end testing/benchmarking the WebIDL path).

To run the WPT DOM runner using the WebIDL DOM bindings backend:

```bash
FASTERENDER_WPT_DOM_BINDINGS_BACKEND=webidl \
  bash scripts/cargo_agent.sh xtask js wpt-dom
```

Or equivalently via the `xtask` flag:

```bash
bash scripts/cargo_agent.sh xtask js wpt-dom --dom-bindings-backend webidl
```

When invoking `wpt_dom` directly, you can also use the CLI flag:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh run -p js-wpt-dom-runner --features vmjs --bin wpt_dom -- \
  --backend vmjs --dom-bindings-backend webidl --filter 'dom/**'
```

## What gets executed

The harness currently runs:

- `*.window.js`
- `*.any.js` (executed in a window-like realm)
- HTML testharness files (`*.html` / `*.htm` / `*.xhtml`)

Other JS variants (worker/serviceworker/sharedworker) are discovered but currently reported as
**skipped**.
