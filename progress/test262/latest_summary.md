# test262 (curated) — latest summary

Committed snapshot of `vm-js` conformance on the curated `test262-semantic` suite.

## Command

```bash
# from repo root
CARGO_TARGET_DIR=../../target LIMIT_STACK=64M timeout -k 10 900 bash scripts/cargo_agent.sh run -p test262-semantic --release -- \
  --harness test262 \
  --suite-path ../../tests/js/test262_suites/curated.toml \
  --manifest ../../tests/js/test262_manifest.toml \
  --timeout-secs 10 \
  --jobs 4 \
  --report-path ../../target/js/test262_curated_now.json \
  --fail-on none
```

- RegExp-focused suite (separate from the curated suite):
  ```bash
  # from repo root
  CARGO_TARGET_DIR=../../target LIMIT_STACK=64M timeout -k 10 900 bash scripts/cargo_agent.sh run -p test262-semantic --release -- \
    --harness test262 \
    --suite-path ../../tests/js/test262_suites/regexp.toml \
    --manifest ../../tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path ../../target/js/test262_regexp.json \
    --fail-on none
  ```

- RegExp `/v` Unicode sets suite (large generated corpus; kept separate from `regexp.toml`):
  ```bash
  # from repo root
  CARGO_TARGET_DIR=../../target LIMIT_STACK=64M timeout -k 10 900 bash scripts/cargo_agent.sh run -p test262-semantic --release -- \
    --harness test262 \
    --suite-path ../../tests/js/test262_suites/regexp_unicode_sets.toml \
    --manifest ../../tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path ../../target/js/test262_regexp_unicode_sets.json \
    --fail-on none
  ```

- RegExp Unicode property escapes (generated) suite (large; some known slow cases are excluded in the suite file):
  ```bash
  # from repo root
  CARGO_TARGET_DIR=../../target LIMIT_STACK=64M timeout -k 10 900 bash scripts/cargo_agent.sh run -p test262-semantic --release -- \
    --harness test262 \
    --suite-path ../../tests/js/test262_suites/regexp_property_escapes_generated.toml \
    --manifest ../../tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path ../../target/js/test262_regexp_property_escapes_generated.json \
    --fail-on none
  ```

- JSON report (not committed): `target/js/test262_curated_now.json`
- Note: `scripts/cargo_agent.sh` runs the vendored `test262-semantic` workspace from `vendor/ecma-rs/`,
  so the `../../...` paths above are relative to that directory.
- Note: `CARGO_TARGET_DIR=../../target` keeps build artifacts under the repo-root `target/` (avoids
  creating `vendor/ecma-rs/target/`).
- Note: `test262-semantic` runs each case on a fresh large-stack thread (see
  `vendor/ecma-rs/test262-semantic/src/vm_js_executor.rs`) so deep-recursion tests should fail
  cleanly with `execution terminated: stack overflow` rather than aborting the host process.
  `LIMIT_STACK=64M` (consumed by `scripts/run_limited.sh`) is still available as a safety net for
  other deeply recursive workloads.

## Overall

| Metric | Count |
| --- | ---: |
| Total cases | 17318 |
| Matched upstream expected | 15701 (90.66%) |
| Mismatched upstream expected | 1617 (9.34%) |
| Timeouts | 0 |
| Skipped | 40 |
| Unexpected mismatches | 672 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 15661 |
| failed | 1617 |
| timed_out | 0 |
| skipped | 40 |

### Expectations (manifest)

| Kind | Count |
| --- | ---: |
| pass | 8700 |
| xfail | 8578 |
| skip | 40 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS | 8028 |
| FAIL (unexpected) | 672 |
| XFAIL | 945 |
| XPASS | 7633 |
| SKIP | 40 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| built-ins | 7185 | 6753 | 432 | 6.01% | 6237 | 6 | 426 | 476 | 40 |
| language | 10128 | 8943 | 1185 | 11.70% | 1786 | 666 | 519 | 7157 | 0 |
| staging | 5 | 5 | 0 | 0.00% | 5 | 0 | 0 | 0 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7161 | 1078 | 15.05% | 754 | 666 | 412 | 5329 | 0 |
| `built-ins/Set` | 764 | 306 | 40.05% | 388 | 2 | 304 | 70 | 0 |
| `built-ins/Object` | 1692 | 110 | 6.50% | 1330 | 2 | 108 | 252 | 0 |
| `language/expressions` | 2337 | 99 | 4.24% | 1032 | 0 | 99 | 1206 | 0 |
| `built-ins/Array` | 1503 | 6 | 0.40% | 1457 | 0 | 6 | 0 | 40 |
| `language/directive-prologue` | 62 | 6 | 9.68% | 0 | 0 | 6 | 56 | 0 |
| `built-ins/Map` | 405 | 4 | 0.99% | 401 | 2 | 2 | 0 | 0 |
| `built-ins/String` | 820 | 4 | 0.49% | 814 | 0 | 4 | 2 | 0 |
| `built-ins/Symbol` | 184 | 2 | 1.09% | 44 | 0 | 2 | 138 | 0 |
| `language/block-scope` | 287 | 2 | 0.70% | 0 | 0 | 2 | 285 | 0 |

(Total buckets: 22; buckets with 0 mismatches: 12)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 1026 (63.45%)
- VmError::Unimplemented: 568 (35.13%)
- termination: 23 (1.42%)

### Top 20

| # | Kind | Count | Reason |
| ---: | --- | ---: | --- |
| 1 | VmError::Unimplemented | 413 | `unimplemented: async generator functions` |
| 2 | exception/other | 208 | `value is not callable` |
| 3 | exception/other | 116 | `Expected a Test262Error to be thrown but no exception was thrown at all` |
| 4 | VmError::Unimplemented | 90 | `unimplemented: expression type` |
| 5 | exception/other | 84 | `Expected a TypeError to be thrown but no exception was thrown at all` |
| 6 | exception/other | 76 | `Cannot convert undefined or null to object` |
| 7 | exception/other | 66 | `Expected SameValue(«"xCls2"», «"xCls2"») to be false` |
| 8 | exception/other | 60 | `Expected SameValue(«"xCover"», «"xCover"») to be false` |
| 9 | VmError::Unimplemented | 48 | `unimplemented: yield in for-of binding pattern` |
| 10 | exception/other | 44 | `GetIterator: value is not iterable` |
| 11 | exception/other | 42 | `Expected SameValue(«"undefined"», «"function"») to be true` |
| 12 | exception/other | 35 | `Expected a ReferenceError to be thrown but no exception was thrown at all` |
| 13 | termination | 23 | `execution terminated: stack overflow` |
| 14 | exception/other | 16 | `Expected true but got false` |
| 15 | exception/other | 16 | `desc.writable Expected SameValue(«true», «false») to be true` |
| 16 | exception/other | 14 | `Built-in objects must be extensible. Expected SameValue(«false», «true») to be true` |
| 17 | exception/other | 14 | `GetSetRecord coerces size Expected SameValue(«0», «1») to be true` |
| 18 | exception/other | 14 | `isConstructor invoked with a non-function value` |
| 19 | VmError::Unimplemented | 11 | `unimplemented: yield in expression type` |
| 20 | exception/other | 10 | `#18: value === undefined. Actual:  value ===value` |

Note: `invalid handle (vm-js/src/heap.rs:1911:16)` no longer appears in the curated report (0 occurrences in `target/js/test262*.json`). The previous snapshot had 24 such mismatches. This was fixed by the vm-js GC-rooting work in `6a155a00` (`fix(vm-js): root ArrayBuffer/DataView/TypedArray args across GC`).

To sanity-check for nondeterminism, a `--jobs 1` run also had 0 `invalid handle` occurrences (report: `target/js/test262_curated_jobs1.json`), but hit 1 timeout: `language/statements/for-of/dstr/const-obj-ptrn-prop-id.js#strict`.

## Timed-out tests

_None._

## Appendix: top failing tests (IDs + first-line error)

At least 50 mismatched cases, grouped by the largest mismatch buckets.

### `language/statements` (10 shown / 1078 mismatches)

- `language/statements/async-function/dflt-params-abrupt.js#non_strict`: `at language/statements/async-function/dflt-params-abrupt.js:207:36`
- `language/statements/async-function/dflt-params-abrupt.js#strict`: `at language/statements/async-function/dflt-params-abrupt.js:209:36`
- `language/statements/async-function/dflt-params-ref-later.js#non_strict`: `Cannot access 'y' before initialization`
- `language/statements/async-function/dflt-params-ref-later.js#strict`: `Cannot access 'y' before initialization`
- `language/statements/async-function/dflt-params-ref-self.js#non_strict`: `Cannot access 'x' before initialization`
- `language/statements/async-function/dflt-params-ref-self.js#strict`: `Cannot access 'x' before initialization`
- `language/statements/async-function/eval-var-scope-syntax-err.js#non_strict`: `null`
- `language/statements/async-function/evaluation-default-that-throws.js#non_strict`: `value is not callable`
- `language/statements/async-function/evaluation-default-that-throws.js#strict`: `value is not callable`
- `language/statements/async-function/evaluation-mapped-arguments.js#non_strict`: `Expected SameValue(«1», «2») to be true`

### `built-ins/Set` (10 shown / 306 mismatches)

- `built-ins/Set/Symbol.species/return-value.js#non_strict`: `error[PS0002]: expected identifier`
- `built-ins/Set/Symbol.species/return-value.js#strict`: `error[PS0002]: expected identifier`
- `built-ins/Set/prototype/difference/add-not-called.js#non_strict`: `value is not callable`
- `built-ins/Set/prototype/difference/add-not-called.js#strict`: `value is not callable`
- `built-ins/Set/prototype/difference/allows-set-like-class.js#non_strict`: `value is not callable`
- `built-ins/Set/prototype/difference/allows-set-like-class.js#strict`: `value is not callable`
- `built-ins/Set/prototype/difference/allows-set-like-object.js#non_strict`: `value is not callable`
- `built-ins/Set/prototype/difference/allows-set-like-object.js#strict`: `value is not callable`
- `built-ins/Set/prototype/difference/builtins.js#non_strict`: `Built-in objects must be extensible. Expected SameValue(«false», «true») to be true`
- `built-ins/Set/prototype/difference/builtins.js#strict`: `Built-in objects must be extensible. Expected SameValue(«false», «true») to be true`

### `built-ins/Object` (10 shown / 110 mismatches)

- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-118.js#non_strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-118.js#strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-120.js#non_strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-120.js#strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-121.js#non_strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-121.js#strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-122.js#non_strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-122.js#strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-123.js#non_strict`: `Cannot convert undefined or null to object`
- `built-ins/Object/getOwnPropertyDescriptor/15.2.3.3-4-123.js#strict`: `Cannot convert undefined or null to object`

### `language/expressions` (10 shown / 99 mismatches)

- `language/expressions/comma/tco-final.js#strict`: `execution terminated: stack overflow`
- `language/expressions/in/private-field-presence-accessor.js#non_strict`: `Expected SameValue(«false», «true») to be true`
- `language/expressions/in/private-field-presence-accessor.js#strict`: `Expected SameValue(«false», «true») to be true`
- `language/expressions/in/private-field-presence-method.js#non_strict`: `Expected SameValue(«false», «true») to be true`
- `language/expressions/in/private-field-presence-method.js#strict`: `Expected SameValue(«false», «true») to be true`
- `language/expressions/logical-and/tco-right.js#strict`: `execution terminated: stack overflow`
- `language/expressions/logical-or/tco-right.js#strict`: `execution terminated: stack overflow`
- `language/expressions/member-expression/computed-reference-null-or-undefined.js#non_strict`: `Expected a TypeError but got a Test262Error`
- `language/expressions/member-expression/computed-reference-null-or-undefined.js#strict`: `Expected a TypeError but got a Test262Error`
- `language/expressions/new/non-ctor-err-realm.js#non_strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`

### `built-ins/Array` (6 shown / 6 mismatches)

- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#non_strict`: `BigInt64Array is not defined`
- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#strict`: `BigInt64Array is not defined`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#non_strict`: `TypedArray view out of bounds`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#strict`: `TypedArray view out of bounds`
- `built-ins/Array/prototype/slice/resizable-buffer.js#non_strict`: `TypedArray view out of bounds`
- `built-ins/Array/prototype/slice/resizable-buffer.js#strict`: `TypedArray view out of bounds`

### `language/directive-prologue` (6 shown / 6 mismatches)

- `language/directive-prologue/10.1.1-14-s.js#non_strict`: `Expected a SyntaxError to be thrown but no exception was thrown at all`
- `language/directive-prologue/10.1.1-30-s.js#non_strict`: `Expected a SyntaxError but got a ReferenceError`
- `language/directive-prologue/10.1.1-5-s.js#non_strict`: `Expected a SyntaxError to be thrown but no exception was thrown at all`
- `language/directive-prologue/10.1.1-8-s.js#non_strict`: `Expected a SyntaxError to be thrown but no exception was thrown at all`
- `language/directive-prologue/14.1-4-s.js#non_strict`: `Expected true but got false`
- `language/directive-prologue/14.1-5-s.js#non_strict`: `Expected true but got false`
