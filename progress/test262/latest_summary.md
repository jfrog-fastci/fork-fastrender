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
  building it first (e.g. `CARGO_TARGET_DIR=target bash scripts/cargo_agent.sh build --manifest-path vendor/ecma-rs/Cargo.toml -p test262-semantic`,
  plus `--release` for `target/release/...`)
  to avoid accidentally using a stale binary.
- Note: `test262-semantic` runs each case on a fresh large-stack thread (see
  `vendor/ecma-rs/test262-semantic/src/vm_js_executor.rs`) so deep-recursion tests should fail
  cleanly with `execution terminated: stack overflow` rather than aborting the host process.
  `LIMIT_STACK=64M` (consumed by `scripts/run_limited.sh`) is still available as a safety net for
  other deeply recursive workloads.

## Overall

| Metric | Count |
| --- | ---: |
| Total cases | 17318 |
| Matched upstream expected | 16006 (92.42%) |
| Mismatched upstream expected | 1312 (7.58%) |
| Timeouts | 0 |
| Skipped | 40 |
| Unexpected mismatches | 664 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 15966 |
| failed | 1312 |
| timed_out | 0 |
| skipped | 40 |

### Expectations (manifest)

| Kind | Count |
| --- | ---: |
| pass | 8706 |
| xfail | 8572 |
| skip | 40 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS | 8042 |
| FAIL (unexpected) | 664 |
| XFAIL | 648 |
| XPASS | 7924 |
| SKIP | 40 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| built-ins | 7185 | 6871 | 314 | 4.37% | 6249 | 0 | 314 | 582 | 40 |
| language | 10128 | 9130 | 998 | 9.85% | 1788 | 664 | 334 | 7342 | 0 |
| staging | 5 | 5 | 0 | 0.00% | 5 | 0 | 0 | 0 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7161 | 971 | 13.56% | 756 | 664 | 307 | 5434 | 0 |
| `built-ins/Set` | 764 | 304 | 39.79% | 390 | 0 | 304 | 70 | 0 |
| `language/expressions` | 2337 | 27 | 1.16% | 1032 | 0 | 27 | 1278 | 0 |
| `built-ins/Array` | 1503 | 6 | 0.40% | 1457 | 0 | 6 | 0 | 40 |
| `built-ins/Map` | 405 | 2 | 0.49% | 403 | 0 | 2 | 0 | 0 |
| `built-ins/Symbol` | 184 | 2 | 1.09% | 44 | 0 | 2 | 138 | 0 |

(Total buckets: 22; buckets with 0 mismatches: 16)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 840 (64.02%)
- VmError::Unimplemented: 472 (35.98%)

### Top 20

| # | Kind | Count | Reason |
| ---: | --- | ---: | --- |
| 1 | VmError::Unimplemented | 413 | `unimplemented: async generator functions` |
| 2 | exception/other | 206 | `value is not callable` |
| 3 | exception/other | 116 | `Expected a Test262Error to be thrown but no exception was thrown at all` |
| 4 | exception/other | 72 | `Expected a TypeError to be thrown but no exception was thrown at all` |
| 5 | exception/other | 66 | `Expected SameValue(«"xCls2"», «"xCls2"») to be false` |
| 6 | exception/other | 60 | `Expected SameValue(«"xCover"», «"xCover"») to be false` |
| 7 | VmError::Unimplemented | 48 | `unimplemented: yield in for-of binding pattern` |
| 8 | exception/other | 42 | `Expected SameValue(«"undefined"», «"function"») to be true` |
| 9 | exception/other | 36 | `Expected a ReferenceError to be thrown but no exception was thrown at all` |
| 10 | exception/other | 23 | `Maximum call stack size exceeded` |
| 11 | exception/other | 14 | `Built-in objects must be extensible. Expected SameValue(«false», «true») to be true` |
| 12 | exception/other | 14 | `Expected true but got false` |
| 13 | exception/other | 14 | `GetSetRecord coerces size Expected SameValue(«0», «1») to be true` |
| 14 | exception/other | 14 | `isConstructor invoked with a non-function value` |
| 15 | VmError::Unimplemented | 11 | `unimplemented: yield in expression type` |
| 16 | exception/other | 10 | `#18: value === undefined. Actual:  value ===value` |
| 17 | exception/other | 10 | `TypedArray view out of bounds` |
| 18 | exception/other | 9 | `Expected SameValue(«1», «undefined») to be true` |
| 19 | exception/other | 8 | `#0: result === "value". Actual:  result ===myObj_value` |
| 20 | exception/other | 8 | `BigInt64Array is not defined` |

Note: `invalid handle (vm-js/src/heap.rs:1911:16)` no longer appears in the curated report (0 occurrences in `target/js/test262*.json`). The previous snapshot had 24 such mismatches. This was fixed by the vm-js GC-rooting work in `6a155a00` (`fix(vm-js): root ArrayBuffer/DataView/TypedArray args across GC`).

To sanity-check for nondeterminism, a `--jobs 1` run also had 0 `invalid handle` occurrences (report: `target/js/test262_curated_jobs1.json`), but hit 1 timeout: `language/statements/for-of/dstr/const-obj-ptrn-prop-id.js#strict`.

## Timed-out tests

_None._

## Appendix: top failing tests (IDs + first-line error)

Sample mismatched cases (ID + first non-empty `error` line), grouped by the largest mismatch buckets.

### `language/statements` (10 shown / 971 mismatches)

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

### `built-ins/Set` (10 shown / 304 mismatches)

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

### `language/expressions` (10 shown / 27 mismatches)

- `language/expressions/comma/tco-final.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/logical-and/tco-right.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/logical-or/tco-right.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/member-expression/computed-reference-null-or-undefined.js#non_strict`: `Expected a TypeError but got a Test262Error`
- `language/expressions/member-expression/computed-reference-null-or-undefined.js#strict`: `Expected a TypeError but got a Test262Error`
- `language/expressions/new/non-ctor-err-realm.js#non_strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`
- `language/expressions/new/non-ctor-err-realm.js#strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`
- `language/expressions/super/call-proto-not-ctor.js#non_strict`: `Expected SameValue(«"undefined"», «"object"») to be true`
- `language/expressions/super/call-proto-not-ctor.js#strict`: `Expected SameValue(«"undefined"», «"object"») to be true`
- `language/expressions/super/prop-dot-cls-val-from-eval.js#non_strict`: `expected super property access not allowed outside methods and class elements`

### `built-ins/Array` (6 shown / 6 mismatches)

- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#non_strict`: `BigInt64Array is not defined`
- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#strict`: `BigInt64Array is not defined`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#non_strict`: `TypedArray view out of bounds`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#strict`: `TypedArray view out of bounds`
- `built-ins/Array/prototype/slice/resizable-buffer.js#non_strict`: `TypedArray view out of bounds`
- `built-ins/Array/prototype/slice/resizable-buffer.js#strict`: `TypedArray view out of bounds`

### `built-ins/Map` (2 shown / 2 mismatches)

- `built-ins/Map/valid-keys.js#non_strict`: `BigInt64Array is not defined`
- `built-ins/Map/valid-keys.js#strict`: `BigInt64Array is not defined`

### `built-ins/Symbol` (2 shown / 2 mismatches)

- `built-ins/Symbol/is-constructor.js#non_strict`: `Expected true but got false`
- `built-ins/Symbol/is-constructor.js#strict`: `Expected true but got false`
