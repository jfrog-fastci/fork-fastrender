# test262 (curated) — latest summary

Committed snapshot of `vm-js` conformance on the curated `test262-semantic` suite.

## Command

```bash
# from repo root

# Build the vendored runner first (outside the hard timeout so compilation doesn't eat the budget).
CARGO_TARGET_DIR=target bash scripts/cargo_agent.sh build --manifest-path vendor/ecma-rs/Cargo.toml -p test262-semantic --release

# Run the curated suite under a hard timeout, writing the JSON report.
LIMIT_STACK=64M timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  target/release/test262-semantic \
  --test262-dir vendor/ecma-rs/test262-semantic/data \
  --harness test262 \
  --suite-path tests/js/test262_suites/curated.toml \
  --manifest tests/js/test262_manifest.toml \
  --timeout-secs 10 \
  --jobs 4 \
  --report-path target/js/test262.json \
  --fail-on none
```

- RegExp-focused suite (separate from the curated suite):
  ```bash
  # from repo root
  LIMIT_STACK=64M timeout -k 10 900 bash scripts/run_limited.sh --as 64G -- \
    target/release/test262-semantic \
    --test262-dir vendor/ecma-rs/test262-semantic/data \
    --harness test262 \
    --suite-path tests/js/test262_suites/regexp.toml \
    --manifest tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path target/js/test262_regexp.json \
    --fail-on none
  ```

- RegExp `/v` Unicode sets suite (large generated corpus; kept separate from `regexp.toml`):
  ```bash
  # from repo root
  LIMIT_STACK=64M timeout -k 10 900 bash scripts/run_limited.sh --as 64G -- \
    target/release/test262-semantic \
    --test262-dir vendor/ecma-rs/test262-semantic/data \
    --harness test262 \
    --suite-path tests/js/test262_suites/regexp_unicode_sets.toml \
    --manifest tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path target/js/test262_regexp_unicode_sets.json \
    --fail-on none
  ```

- RegExp Unicode property escapes (generated) suite (large; some known slow cases are excluded in the suite file):
  ```bash
  # from repo root
  LIMIT_STACK=64M timeout -k 10 900 bash scripts/run_limited.sh --as 64G -- \
    target/release/test262-semantic \
    --test262-dir vendor/ecma-rs/test262-semantic/data \
    --harness test262 \
    --suite-path tests/js/test262_suites/regexp_property_escapes_generated.toml \
    --manifest tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path target/js/test262_regexp_property_escapes_generated.json \
    --fail-on none
  ```

- JSON report (not committed): `target/js/test262.json`
- Note: running `target/debug/test262-semantic` (or `target/release/test262-semantic`) directly requires
  building it first (e.g. `CARGO_TARGET_DIR=target bash scripts/cargo_agent.sh build --manifest-path vendor/ecma-rs/Cargo.toml -p test262-semantic`).
- Note: `test262-semantic` runs each case on a fresh large-stack thread (see
  `vendor/ecma-rs/test262-semantic/src/vm_js_executor.rs`) so deep-recursion tests should fail
  cleanly with a JS `RangeError` (call-stack exhaustion) rather than aborting the host process.
  `LIMIT_STACK=64M` (consumed by `scripts/run_limited.sh`) is still available as a safety net for
  other deeply recursive workloads.

## Overall

| Metric | Count |
| --- | ---: |
| Total cases | 17336 |
| Matched upstream expected | 17056 (98.38%) |
| Mismatched upstream expected | 280 (1.62%) |
| Timeouts | 0 |
| Skipped | 42 |
| Unexpected mismatches | 0 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 17014 |
| failed | 280 |
| timed_out | 0 |
| skipped | 42 |

### Expectations (manifest)

| Kind | Count |
| --- | ---: |
| pass | 9825 |
| xfail | 7469 |
| skip | 42 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS | 9825 |
| FAIL (unexpected) | 0 |
| XFAIL | 280 |
| XPASS | 7189 |
| SKIP | 42 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| built-ins | 7185 | 7171 | 14 | 0.19% | 6757 | 0 | 14 | 372 | 42 |
| language | 10146 | 9880 | 266 | 2.62% | 3063 | 0 | 266 | 6817 | 0 |
| staging | 5 | 5 | 0 | 0.00% | 5 | 0 | 0 | 0 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7179 | 247 | 3.44% | 2031 | 0 | 247 | 4901 | 0 |
| `language/expressions` | 2337 | 19 | 0.81% | 1032 | 0 | 19 | 1286 | 0 |
| `built-ins/Array` | 1503 | 6 | 0.40% | 1455 | 0 | 6 | 0 | 42 |
| `built-ins/JSON` | 330 | 6 | 1.82% | 324 | 0 | 6 | 0 | 0 |
| `built-ins/Object` | 1692 | 2 | 0.12% | 1332 | 0 | 2 | 358 | 0 |
| `built-ins/Boolean` | 101 | 0 | 0.00% | 101 | 0 | 0 | 0 | 0 |
| `built-ins/Error` | 2 | 0 | 0.00% | 2 | 0 | 0 | 0 | 0 |
| `built-ins/Function` | 96 | 0 | 0.00% | 96 | 0 | 0 | 0 | 0 |
| `built-ins/Map` | 405 | 0 | 0.00% | 405 | 0 | 0 | 0 | 0 |
| `built-ins/Math` | 654 | 0 | 0.00% | 654 | 0 | 0 | 0 | 0 |

(Total buckets: 22; buckets with 0 mismatches: 17)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 280 (100.00%)
- VmError::Unimplemented: 0 (0.00%)
- termination: 0 (0.00%)

### Top 20

| # | Kind | Count | Reason |
| ---: | --- | ---: | --- |
| 1 | exception/other | 50 | `Expected SameValue(«"xCls2"», «"xCls2"») to be false` |
| 2 | exception/other | 44 | `Expected SameValue(«"xCover"», «"xCover"») to be false` |
| 3 | exception/other | 23 | `Maximum call stack size exceeded` |
| 4 | exception/other | 20 | `Test262Error: Expected SameValue(«2», «1») to be true` |
| 5 | exception/other | 10 | `#18: value === undefined. Actual:  value ===value` |
| 6 | exception/other | 10 | `Expected true but got false` |
| 7 | exception/other | 9 | `Expected SameValue(«1», «undefined») to be true` |
| 8 | exception/other | 8 | `#0: result === "value". Actual:  result ===myObj_value` |
| 9 | exception/other | 8 | `error[PS0002]: expected expression operator` |
| 10 | exception/other | 6 | `Expected a TypeError but got a Test262Error` |
| 11 | exception/other | 6 | `Object` |
| 12 | exception/other | 6 | `TypeError: Cannot convert undefined or null to object` |
| 13 | exception/other | 4 | `#1: __evaluated === 4. Actual:  __evaluated ===4 Expected SameValue(«4», «undefined») to be true` |
| 14 | exception/other | 4 | `Actual [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] and expected [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] should have the same contents. TestIterationAndResize: list of iterated values` |
| 15 | exception/other | 4 | `Expected SameValue(«"bad"», «"ok"») to be true` |
| 16 | exception/other | 4 | `Expected SameValue(«"inside"», «"outside"») to be true` |
| 17 | exception/other | 4 | `Expected SameValue(«0», «2») to be true` |
| 18 | exception/other | 4 | `Expected SameValue(«3», «undefined») to be true` |
| 19 | exception/other | 3 | `#19: myObj.value === "value". Actual:  myObj.value ===myObj_value` |
| 20 | exception/other | 3 | `Expected SameValue(«6», «undefined») to be true` |

## Timed-out tests

_None._

## Appendix: top failing tests (IDs + first-line error)

At least 50 mismatched cases, grouped by the largest mismatch buckets.

(If the suite only has a few buckets with mismatches, the largest buckets will show more
than `--appendix-per-bucket` entries so the appendix still reaches the minimum count.)

### `language/statements` (26 shown / 247 mismatches)

- `language/statements/async-generator/dstr/ary-init-iter-close.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/ary-init-iter-close.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/ary-ptrn-elem-ary-elision-init.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/ary-ptrn-elem-ary-elision-init.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/ary-ptrn-elem-ary-empty-init.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/ary-ptrn-elem-ary-empty-init.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-init-iter-close.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-init-iter-close.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-ptrn-elem-ary-elision-init.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-ptrn-elem-ary-elision-init.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-ptrn-elem-ary-empty-init.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-ptrn-elem-ary-empty-init.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-ptrn-elision.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-ptrn-elision.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-ptrn-rest-ary-elision.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-ary-ptrn-rest-ary-elision.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-obj-ptrn-rest-getter.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/dflt-obj-ptrn-rest-getter.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/obj-ptrn-rest-getter.js#non_strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/dstr/obj-ptrn-rest-getter.js#strict`: `Test262Error: Expected SameValue(«2», «1») to be true`
- `language/statements/async-generator/generator-created-after-decl-inst.js#non_strict`: `Expected SameValue(«[object AsyncGenerator]», «[object AsyncGenerator]») to be false`
- `language/statements/async-generator/generator-created-after-decl-inst.js#strict`: `Expected SameValue(«[object AsyncGenerator]», «[object AsyncGenerator]») to be false`
- `language/statements/async-generator/return-undefined-implicit-and-explicit.js#non_strict`: `Test262Error: Actual [tick 1, tick 2, g1 ret, g2 ret, g3 ret, g4 ret] and expected [tick 1, g1 ret, g2 ret, tick 2, g3 ret, g4 ret] should have the same contents. Ticks for implicit and explicit return undefined`
- `language/statements/async-generator/return-undefined-implicit-and-explicit.js#strict`: `Test262Error: Actual [tick 1, tick 2, g1 ret, g2 ret, g3 ret, g4 ret] and expected [tick 1, g1 ret, g2 ret, tick 2, g3 ret, g4 ret] should have the same contents. Ticks for implicit and explicit return undefined`
- `language/statements/async-generator/yield-star-async-next.js#non_strict`: `TypeError: Cannot convert undefined or null to object`
- `language/statements/async-generator/yield-star-async-next.js#strict`: `TypeError: Cannot convert undefined or null to object`

### `language/expressions` (10 shown / 19 mismatches)

- `language/expressions/comma/tco-final.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/logical-and/tco-right.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/logical-or/tco-right.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/member-expression/computed-reference-null-or-undefined.js#non_strict`: `Expected a TypeError but got a Test262Error`
- `language/expressions/member-expression/computed-reference-null-or-undefined.js#strict`: `Expected a TypeError but got a Test262Error`
- `language/expressions/new/non-ctor-err-realm.js#non_strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`
- `language/expressions/new/non-ctor-err-realm.js#strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`
- `language/expressions/super/call-proto-not-ctor.js#non_strict`: `Expected SameValue(«"undefined"», «"object"») to be true`
- `language/expressions/super/call-proto-not-ctor.js#strict`: `Expected SameValue(«"undefined"», «"object"») to be true`
- `language/expressions/super/prop-expr-getsuperbase-before-topropertykey-getvalue.js#non_strict`: `Expected SameValue(«"bad"», «"ok"») to be true`

### `built-ins/Array` (6 shown / 6 mismatches)

- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#non_strict`: `Actual [4] and expected [4, 0] should have the same contents.`
- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#strict`: `Actual [4] and expected [4, 0] should have the same contents.`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#non_strict`: `Actual [undefined, undefined, undefined, undefined] and expected [1, 2, undefined, undefined] should have the same contents.`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#strict`: `Actual [undefined, undefined, undefined, undefined] and expected [1, 2, undefined, undefined] should have the same contents.`
- `built-ins/Array/prototype/slice/resizable-buffer.js#non_strict`: `Actual [] and expected [0, 1, 2] should have the same contents.`
- `built-ins/Array/prototype/slice/resizable-buffer.js#strict`: `Actual [] and expected [0, 1, 2] should have the same contents.`

### `built-ins/JSON` (6 shown / 6 mismatches)

- `built-ins/JSON/stringify/replacer-array-number-object.js#non_strict`: `Expected SameValue(«"{\"10\":1}"», «"{\"toString\":2}"») to be true`
- `built-ins/JSON/stringify/replacer-array-number-object.js#strict`: `Expected SameValue(«"{\"10\":1}"», «"{\"toString\":2}"») to be true`
- `built-ins/JSON/stringify/space-number-object.js#non_strict`: `Expected SameValue(«"{\n \"a1\": {\n  \"b1\": [\n   1,\n   2,\n   3,\n   4\n  ],\n  \"b2\": {\n   \"c1\": 1,\n   \"c2\": 2\n  }\n },\n \"a2\": \"a2\"\n}"», «"{\n   \"a1\": {\n      \"b1\": [\n         1,\n         2,\n         3,\n         4\n      ],\n      \"b2\": {\n         \"c1\": 1,\n         \"c2\": 2\n      }\n   },\n   \"a2\": \"a2\"\n}"») to be true`
- `built-ins/JSON/stringify/space-number-object.js#strict`: `Expected SameValue(«"{\n \"a1\": {\n  \"b1\": [\n   1,\n   2,\n   3,\n   4\n  ],\n  \"b2\": {\n   \"c1\": 1,\n   \"c2\": 2\n  }\n },\n \"a2\": \"a2\"\n}"», «"{\n   \"a1\": {\n      \"b1\": [\n         1,\n         2,\n         3,\n         4\n      ],\n      \"b2\": {\n         \"c1\": 1,\n         \"c2\": 2\n      }\n   },\n   \"a2\": \"a2\"\n}"») to be true`
- `built-ins/JSON/stringify/value-number-object.js#non_strict`: `Expected SameValue(«"[42]"», «"[2]"») to be true`
- `built-ins/JSON/stringify/value-number-object.js#strict`: `Expected SameValue(«"[42]"», «"[2]"») to be true`

### `built-ins/Object` (2 shown / 2 mismatches)

- `built-ins/Object/prototype/toString/symbol-tag-override-instances.js#non_strict`: `Expected SameValue(«"[object Error]"», «"[object test262]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-override-instances.js#strict`: `Cannot assign to read-only property`
