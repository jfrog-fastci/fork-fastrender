# WPT DOM (`testharness.js`) fixtures

This directory is reserved for **Web Platform Tests** that use WPT's
[`testharness.js`](https://web-platform-tests.org/writing-tests/testharness.html)
(DOM / Web API style tests, *not* reftests).

FastRender runs these tests in an offline CLI harness. WPT's upstream
`testharnessreport.js` is UI-focused, so the runner additionally loads a
FastRender-specific reporter shim:

1. WPT `testharness.js`
2. `tests/wpt_dom/resources/fastrender_testharness_report.js`
3. the test file under execution

## Reporter contract

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
