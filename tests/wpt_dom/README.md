# Offline WPT DOM corpus (`testharness.js`)

This directory is a **small, curated, fully offline** subset of Web Platform Tests (WPT)
focused on DOM and event-loop behavior.

It is intended to be run by FastRender's `xtask` harness:

```bash
bash scripts/cargo_agent.sh xtask js wpt-dom
```

The underlying runner (`crates/js-wpt-dom-runner`) executes tests with the `vm-js` runtime.
It supports two execution backends:

- `vmjs` (default): `vm-js` executed against a lightweight window/document host (no renderer/layout).
- `vmjs-rendered`: `vm-js` executed against a renderer-backed document so layout-sensitive tests can
  observe real geometry.

Select the backend via `--backend vmjs|vmjs-rendered` (or `FASTERENDER_WPT_DOM_BACKEND=...`).

To exercise the WebIDL-generated DOM bindings backend end-to-end on the vm-js runner:

```bash
FASTERENDER_WPT_DOM_BINDINGS_BACKEND=webidl bash scripts/cargo_agent.sh xtask js wpt-dom
```

By default, this writes a JSON report to:

```text
target/js/wpt_dom.json
```

See [`docs/js_wpt_dom.md`](../../docs/js_wpt_dom.md) for the full workflow and key flags.

The goal is to keep a **stable, fully offline on-disk corpus** with a predictable directory
layout that can grow alongside FastRender's JS + DOM implementation.

## Runner scope

The current runner executes:

- JavaScript `testharness.js` tests:
  - `*.window.js`
  - `*.any.js` (executed in a window-like realm)
- HTML testharness files:
  - `*.html` / `*.htm` / `*.xhtml`

Other JS variants (worker/serviceworker/sharedworker) are discovered but currently reported as
**skipped**.

## Directory layout

```text
tests/wpt_dom/
  expectations.toml        # Expected statuses (ecma-rs conformance-harness format)
  resources/               # Served as /resources/... in WPT
    testharness.js
    testharnessreport.js
    fastrender_testharness_report.js
    eventtarget_shim.js    # Legacy (unused) EventTarget/Event polyfill kept for historical reference
    LICENSE
  tests/                   # Served as /... in WPT
    smoke/                 # Tiny synthetic tests for harness bring-up
      *.html
    dom/                   # Curated DOM primitives (Node/Element/DocumentFragment, collections/traversal)
      *.window.js
    domparsing/            # Curated DOMParsing tests (innerHTML/outerHTML)
      *.window.js
    html/                  # Curated HTML element + form control tests (HTMLElement, HTMLInputElement, etc.)
      *.window.js
    event_loop/            # Curated event loop ordering tests
      *.window.js
    events/                # Curated EventTarget/Event semantics tests
      *.window.js
    observers/             # Curated observer API smoke tests (geometry-independent)
      *.window.js
    url/                   # Curated URL + URLSearchParams tests
      *.window.js
```

Note: `resources/eventtarget_shim.js` is currently unused by the runner (kept only for historical reference).

## Observers

The `tests/observers/` directory contains **API-level** tests for observer-style Web APIs that are
common in real-world scripts:

- `IntersectionObserver`
- `ResizeObserver`

FastRender's offline WPT DOM runner uses `WindowHostState` and does **not** have a full renderer
layout/geometry engine. As a result, these tests intentionally avoid asserting any specific
geometry values (`boundingClientRect`, intersection ratios, box sizes, etc).

When importing upstream WPT tests in the future, most geometry-dependent observer tests should be
marked `skip` in `expectations.toml` with a reason like:

> requires layout geometry; offline DOM runner uses WindowHostState

## URL mapping (WPT origin → filesystem)

WPT tests assume they are hosted on `https://web-platform.test/` (and friends).
The offline runner should map URLs to the filesystem as:

| URL | Filesystem path |
| --- | --- |
| `https://web-platform.test/<path>` | `tests/wpt_dom/tests/<path>` |
| `https://web-platform.test/resources/<path>` | `tests/wpt_dom/resources/<path>` |

This means test files should use the usual WPT absolute-root resource paths, e.g.:

```html
<script src="/resources/testharness.js"></script>
<script src="/resources/testharnessreport.js"></script>
```

## Adding/importing new tests

Import tests from a local upstream WPT checkout with the dedicated importer:

```bash
# Import a single test (paths are relative to the WPT repo root)
bash scripts/cargo_agent.sh run --bin import_wpt_dom -- \
  --wpt-root ~/src/wpt \
  --test dom/nodes/Node-appendChild.window.js

# Or import a suite via globs (repeatable)
bash scripts/cargo_agent.sh run --bin import_wpt_dom -- \
  --wpt-root ~/src/wpt \
  --suite 'dom/nodes/*.window.js' \
  --suite 'dom/events/**'
```

The importer:

- copies selected `*.html`, `*.window.js`, `*.any.js` tests into `tests/wpt_dom/tests/`,
  preserving upstream relative paths,
- copies referenced support files (e.g. `/resources/...`, `/common/...`) into the appropriate
  local mirror (`tests/` vs `resources/`),
- rewrites `https://web-platform.test/...` and `//web-platform.test/...` URLs to origin-absolute
  paths so the offline runner can resolve them,
- refuses to leave fetchable `http(s)://` or protocol-relative (`//...`) URLs behind (use
  `--strict-offline` for an even stricter scan).

After importing, run the offline corpus validator (fast, CI-safe):

```bash
bash scripts/cargo_agent.sh test -p js-wpt-dom-runner --test corpus_validation
```

Finally, update `expectations.toml`:

- default is `pass` (no entry needed),
- use `skip` for unimplemented tests,
- use `xfail` for known failures (with a reason),
- use `flaky` for intermittently failing tests.

## Reporting integration (FastRender runner)

WPT's upstream `testharnessreport.js` is primarily aimed at browser UI output. FastRender
runs tests in an offline CLI harness and needs a deterministic, machine-readable payload.

The recommended runner loading order is:

1. `resources/testharness.js`
2. `resources/fastrender_testharness_report.js`
3. the test file under execution

The smoke tests also load `testharnessreport.js` so they can be opened in a browser for
manual debugging; the CLI runner can ignore this script if it wants.

### Reporter contract

`resources/fastrender_testharness_report.js` installs `add_result_callback` and
`add_completion_callback` and emits exactly one plain-object payload via:

```js
globalThis.__fastrender_wpt_report(payload);
```

The Rust runner is responsible for defining `__fastrender_wpt_report` and for
serializing the payload to JSON if desired.

Payload shape (high level):

- `file_status`: `"pass" | "fail" | "timeout" | "error"`
- `harness_status`: `"ok" | "error" | "timeout" | "unknown(<n>)"`
- `message`/`stack`: harness-level error info (if any)
- `subtests`: `[{ name, status, message, stack }]`

## Harness resources

Upstream WPT uses `resources/testharness.js` + `resources/testharnessreport.js`.

In this repo these files are currently a **minimal compatible subset** sufficient for the
`tests/smoke/` corpus. As the WPT DOM suite grows, we should replace these with verbatim upstream
copies and pin the upstream commit hash in `UPSTREAM_COMMIT.txt`.

The importer supports syncing the harness snapshot later:

```bash
bash scripts/cargo_agent.sh run --bin import_wpt_dom -- --wpt-root ~/src/wpt --sync-harness --overwrite
```
