# Offline WPT DOM corpus (`testharness.js`)

This directory is a **small, curated, fully offline** subset of Web Platform Tests (WPT)
focused on DOM and event-loop behavior.

It is intended to be run by FastRender's `xtask` harness:

```bash
scripts/cargo_agent.sh xtask js wpt-dom
```

The goal is to keep a **stable, fully offline on-disk corpus** with a predictable directory
layout that can grow alongside FastRender's JS + DOM implementation.

## Directory layout

```text
tests/wpt_dom/
  expectations.toml        # Expected statuses (ecma-rs conformance-harness format)
  resources/               # Served as /resources/... in WPT
    testharness.js
    testharnessreport.js
    fastrender_testharness_report.js
    eventtarget_shim.js    # Temporary polyfill for the QuickJS backend
    LICENSE
  tests/                   # Served as /... in WPT
    smoke/                 # Tiny synthetic tests for harness bring-up
      *.html
    event_loop/            # Curated event loop ordering tests
      *.window.js
    events/                # Curated EventTarget/Event semantics tests
      *.window.js
```

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

1. **Pick a testharness test** from upstream WPT (`*.html` and/or `*.window.js`) that
   targets the DOM/web API behavior you are implementing.
2. **Copy it into** `tests/wpt_dom/tests/`, preserving the original relative path from
   WPT root (this makes it easier to sync later and keeps URLs stable).
3. **Vendor any required support files** alongside it (or under `resources/` if the
   upstream test references `/resources/...`).
4. **Keep the suite offline and stable**:
   - avoid real network access (no `https://example.com/...`),
   - avoid dependencies on WPT infra not vendored here (e.g. `/common/`),
   - prefer deterministic timers and avoid flaky races.
5. **Update `expectations.toml`**:
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
`tests/smoke/` corpus. As the WPT DOM suite grows, we should replace these with verbatim
upstream copies and pin the upstream commit hash here.
