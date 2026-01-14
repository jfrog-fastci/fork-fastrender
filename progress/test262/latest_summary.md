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
| Total cases | 17318 |
| Matched upstream expected | 16023 (92.52%) |
| Mismatched upstream expected | 1295 (7.48%) |
| Timeouts | 1 |
| Skipped | 40 |
| Unexpected mismatches | 665 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 15983 |
| failed | 1294 |
| timed_out | 1 |
| skipped | 40 |

### Expectations (manifest)

| Kind | Count |
| --- | ---: |
| pass | 8846 |
| xfail | 8432 |
| skip | 40 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS | 8181 |
| FAIL (unexpected) | 665 |
| XFAIL | 630 |
| XPASS | 7802 |
| SKIP | 40 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| built-ins | 7185 | 6864 | 321 | 4.47% | 6388 | 1 | 320 | 436 | 40 |
| language | 10128 | 9154 | 974 | 9.62% | 1788 | 664 | 310 | 7366 | 0 |
| staging | 5 | 5 | 0 | 0.00% | 5 | 0 | 0 | 0 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7161 | 955 | 13.34% | 756 | 664 | 291 | 5450 | 0 |
| `built-ins/Set` | 764 | 302 | 39.53% | 390 | 0 | 302 | 72 | 0 |
| `language/expressions` | 2337 | 19 | 0.81% | 1032 | 0 | 19 | 1286 | 0 |
| `built-ins/Object` | 1692 | 12 | 0.71% | 1332 | 0 | 12 | 348 | 0 |
| `built-ins/Array` | 1503 | 7 | 0.47% | 1456 | 1 | 6 | 0 | 40 |
| `built-ins/Boolean` | 101 | 0 | 0.00% | 101 | 0 | 0 | 0 | 0 |
| `built-ins/Error` | 2 | 0 | 0.00% | 2 | 0 | 0 | 0 | 0 |
| `built-ins/Function` | 96 | 0 | 0.00% | 96 | 0 | 0 | 0 | 0 |
| `built-ins/JSON` | 330 | 0 | 0.00% | 330 | 0 | 0 | 0 | 0 |
| `built-ins/Map` | 405 | 0 | 0.00% | 403 | 0 | 0 | 2 | 0 |

(Total buckets: 22; buckets with 0 mismatches: 17)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 845 (65.25%)
- VmError::Unimplemented: 450 (34.75%)
- termination: 0 (0.00%)

### Top 20

| # | Kind | Count | Reason |
| ---: | --- | ---: | --- |
| 1 | VmError::Unimplemented | 413 | `unimplemented: async generator functions` |
| 2 | exception/other | 206 | `value is not callable` |
| 3 | exception/other | 116 | `Expected a Test262Error to be thrown but no exception was thrown at all` |
| 4 | exception/other | 72 | `Expected a TypeError to be thrown but no exception was thrown at all` |
| 5 | exception/other | 66 | `Expected SameValue(«"xCls2"», «"xCls2"») to be false` |
| 6 | exception/other | 60 | `Expected SameValue(«"xCover"», «"xCover"») to be false` |
| 7 | exception/other | 42 | `Expected SameValue(«"undefined"», «"function"») to be true` |
| 8 | exception/other | 36 | `Expected a ReferenceError to be thrown but no exception was thrown at all` |
| 9 | exception/other | 23 | `Maximum call stack size exceeded` |
| 10 | VmError::Unimplemented | 18 | `unimplemented: yield in array rest pattern` |
| 11 | exception/other | 14 | `Built-in objects must be extensible. Expected SameValue(«false», «true») to be true` |
| 12 | exception/other | 14 | `GetSetRecord coerces size Expected SameValue(«0», «1») to be true` |
| 13 | exception/other | 14 | `isConstructor invoked with a non-function value` |
| 14 | exception/other | 12 | `Expected true but got false` |
| 15 | VmError::Unimplemented | 11 | `unimplemented: yield in expression type` |
| 16 | exception/other | 10 | `#18: value === undefined. Actual:  value ===value` |
| 17 | exception/other | 10 | `Expected SameValue(«0», «1») to be true` |
| 18 | exception/other | 9 | `Expected SameValue(«1», «undefined») to be true` |
| 19 | VmError::Unimplemented | 8 | `unimplemented: yield in assignment target` |
| 20 | exception/other | 8 | `#0: result === "value". Actual:  result ===myObj_value` |

## Timed-out tests

- `built-ins/Array/prototype/indexOf/15.4.4.14-10-1.js#strict`

## Appendix: top failing tests (IDs + first-line error)

At least 50 mismatched cases, grouped by the largest mismatch buckets.

(If the suite only has a few buckets with mismatches, the largest buckets will show more
than `--appendix-per-bucket` entries so the appendix still reaches the minimum count.)

### `language/statements` (13 shown / 955 mismatches)

- `language/statements/async-function/dflt-params-abrupt.js#non_strict`: `at language/statements/async-function/dflt-params-abrupt.js:207:36`
- `language/statements/async-function/dflt-params-abrupt.js#strict`: `at language/statements/async-function/dflt-params-abrupt.js:209:36`
- `language/statements/async-function/dflt-params-ref-later.js#non_strict`: `Cannot access 'y' before initialization`
- `language/statements/async-function/dflt-params-ref-later.js#strict`: `Cannot access 'y' before initialization`
- `language/statements/async-function/dflt-params-ref-self.js#non_strict`: `Cannot access 'x' before initialization`
- `language/statements/async-function/dflt-params-ref-self.js#strict`: `Cannot access 'x' before initialization`
- `language/statements/async-function/eval-var-scope-syntax-err.js#non_strict`: `null`
- `language/statements/async-function/evaluation-default-that-throws.js#non_strict`: `value is not callable`
- `language/statements/async-function/evaluation-default-that-throws.js#strict`: `value is not callable`
- `language/statements/async-function/evaluation-mapped-arguments.js#non_strict`: `Test262Error: Expected SameValue(«1», «2») to be true`
- `language/statements/async-generator/dflt-params-abrupt.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dflt-params-abrupt.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dflt-params-arg-val-not-undefined.js#non_strict`: `unimplemented: async generator functions`

### `built-ins/Set` (10 shown / 302 mismatches)

- `built-ins/Set/prototype/difference/add-not-called.js#non_strict`: `value is not callable`
- `built-ins/Set/prototype/difference/add-not-called.js#strict`: `value is not callable`
- `built-ins/Set/prototype/difference/allows-set-like-class.js#non_strict`: `value is not callable`
- `built-ins/Set/prototype/difference/allows-set-like-class.js#strict`: `value is not callable`
- `built-ins/Set/prototype/difference/allows-set-like-object.js#non_strict`: `value is not callable`
- `built-ins/Set/prototype/difference/allows-set-like-object.js#strict`: `value is not callable`
- `built-ins/Set/prototype/difference/builtins.js#non_strict`: `Built-in objects must be extensible. Expected SameValue(«false», «true») to be true`
- `built-ins/Set/prototype/difference/builtins.js#strict`: `Built-in objects must be extensible. Expected SameValue(«false», «true») to be true`
- `built-ins/Set/prototype/difference/combines-Map.js#non_strict`: `value is not callable`
- `built-ins/Set/prototype/difference/combines-Map.js#strict`: `value is not callable`

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

### `built-ins/Object` (10 shown / 12 mismatches)

- `built-ins/Object/prototype/toString/symbol-tag-array-builtin.js#non_strict`: `Expected SameValue(«"[object Object]"», «"[object Iterator]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-array-builtin.js#strict`: `Expected SameValue(«"[object Object]"», «"[object Iterator]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-generators-builtin.js#non_strict`: `Expected SameValue(«"[object Generator]"», «"[object Object]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-generators-builtin.js#strict`: `Expected SameValue(«"[object Generator]"», «"[object Object]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-map-builtin.js#non_strict`: `Expected SameValue(«"[object Object]"», «"[object Iterator]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-map-builtin.js#strict`: `Expected SameValue(«"[object Object]"», «"[object Iterator]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-override-instances.js#non_strict`: `Expected SameValue(«"[object Error]"», «"[object test262]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-override-instances.js#strict`: `Cannot assign to read-only property`
- `built-ins/Object/prototype/toString/symbol-tag-set-builtin.js#non_strict`: `Expected SameValue(«"[object Object]"», «"[object Iterator]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-set-builtin.js#strict`: `Expected SameValue(«"[object Object]"», «"[object Iterator]"») to be true`

### `built-ins/Array` (7 shown / 7 mismatches)

- `built-ins/Array/prototype/indexOf/15.4.4.14-10-1.js#strict`: `timeout after 10 seconds`
- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#non_strict`: `Cannot convert a BigInt value to a number`
- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#strict`: `Cannot convert a BigInt value to a number`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#non_strict`: `Actual [undefined, undefined, undefined, undefined] and expected [1, 2, undefined, undefined] should have the same contents.`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#strict`: `Actual [undefined, undefined, undefined, undefined] and expected [1, 2, undefined, undefined] should have the same contents.`
- `built-ins/Array/prototype/slice/resizable-buffer.js#non_strict`: `Actual [] and expected [0, 1, 2] should have the same contents.`
- `built-ins/Array/prototype/slice/resizable-buffer.js#strict`: `Actual [] and expected [0, 1, 2] should have the same contents.`
