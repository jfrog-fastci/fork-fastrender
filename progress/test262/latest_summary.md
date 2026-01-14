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
| Matched upstream expected | 16824 (97.15%) |
| Mismatched upstream expected | 494 (2.85%) |
| Timeouts | 0 |
| Skipped | 40 |
| Unexpected mismatches | 245 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 16784 |
| failed | 494 |
| timed_out | 0 |
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
| PASS | 8601 |
| FAIL (unexpected) | 245 |
| XFAIL | 249 |
| XPASS | 8183 |
| SKIP | 40 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| built-ins | 7185 | 7175 | 10 | 0.14% | 6389 | 0 | 10 | 746 | 40 |
| language | 10128 | 9644 | 484 | 4.78% | 2207 | 245 | 239 | 7437 | 0 |
| staging | 5 | 5 | 0 | 0.00% | 5 | 0 | 0 | 0 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7161 | 473 | 6.61% | 1175 | 245 | 228 | 5513 | 0 |
| `language/expressions` | 2337 | 11 | 0.47% | 1032 | 0 | 11 | 1294 | 0 |
| `built-ins/Array` | 1503 | 6 | 0.40% | 1457 | 0 | 6 | 0 | 40 |
| `built-ins/Object` | 1692 | 4 | 0.24% | 1332 | 0 | 4 | 356 | 0 |
| `built-ins/Boolean` | 101 | 0 | 0.00% | 101 | 0 | 0 | 0 | 0 |
| `built-ins/Error` | 2 | 0 | 0.00% | 2 | 0 | 0 | 0 | 0 |
| `built-ins/Function` | 96 | 0 | 0.00% | 96 | 0 | 0 | 0 | 0 |
| `built-ins/JSON` | 330 | 0 | 0.00% | 330 | 0 | 0 | 0 | 0 |
| `built-ins/Map` | 405 | 0 | 0.00% | 403 | 0 | 0 | 2 | 0 |
| `built-ins/Math` | 654 | 0 | 0.00% | 654 | 0 | 0 | 0 | 0 |

(Total buckets: 22; buckets with 0 mismatches: 18)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 494 (100.00%)
- VmError::Unimplemented: 0 (0.00%)
- termination: 0 (0.00%)

### Top 20

| # | Kind | Count | Reason |
| ---: | --- | ---: | --- |
| 1 | exception/other | 122 | `Expected a Test262Error to be thrown but no exception was thrown at all` |
| 2 | exception/other | 76 | `Expected a TypeError to be thrown but no exception was thrown at all` |
| 3 | exception/other | 50 | `Expected SameValue(«"xCls2"», «"xCls2"») to be false` |
| 4 | exception/other | 44 | `Expected SameValue(«"xCover"», «"xCover"») to be false` |
| 5 | exception/other | 34 | `Expected a ReferenceError to be thrown but no exception was thrown at all` |
| 6 | exception/other | 23 | `Maximum call stack size exceeded` |
| 7 | exception/other | 14 | `Expected SameValue(«0», «1») to be true` |
| 8 | exception/other | 10 | `#18: value === undefined. Actual:  value ===value` |
| 9 | exception/other | 10 | `Expected true but got false` |
| 10 | exception/other | 9 | `Expected SameValue(«1», «undefined») to be true` |
| 11 | exception/other | 8 | `#0: result === "value". Actual:  result ===myObj_value` |
| 12 | exception/other | 8 | `error[PS0002]: expected expression operator` |
| 13 | exception/other | 6 | `Expected a TypeError but got a Test262Error` |
| 14 | exception/other | 6 | `Object` |
| 15 | exception/other | 6 | `TypeError: Cannot convert undefined or null to object` |
| 16 | exception/other | 4 | `#1: __evaluated === 4. Actual:  __evaluated ===4 Expected SameValue(«4», «undefined») to be true` |
| 17 | exception/other | 4 | `Actual [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] and expected [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] should have the same contents. TestIterationAndResize: list of iterated values` |
| 18 | exception/other | 4 | `Cannot convert a BigInt value to a number` |
| 19 | exception/other | 4 | `Expected SameValue(«"inside"», «"outside"») to be true` |
| 20 | exception/other | 4 | `Expected SameValue(«3», «undefined») to be true` |

## Timed-out tests

_None._

## Appendix: top failing tests (IDs + first-line error)

At least 50 mismatched cases, grouped by the largest mismatch buckets.

(If the suite only has a few buckets with mismatches, the largest buckets will show more
than `--appendix-per-bucket` entries so the appendix still reaches the minimum count.)

### `language/statements` (30 shown / 473 mismatches)

- `language/statements/async-generator/dflt-params-abrupt.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dflt-params-abrupt.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dflt-params-ref-later.js#non_strict`: `Expected a ReferenceError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dflt-params-ref-later.js#strict`: `Expected a ReferenceError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dflt-params-ref-self.js#non_strict`: `Expected a ReferenceError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dflt-params-ref-self.js#strict`: `Expected a ReferenceError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-init-iter-get-err-array-prototype.js#non_strict`: `Expected a TypeError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-init-iter-get-err-array-prototype.js#strict`: `Expected a TypeError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-init-iter-get-err.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-init-iter-get-err.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-ary-val-null.js#non_strict`: `Expected a TypeError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-ary-val-null.js#strict`: `Expected a TypeError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-id-init-throws.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-id-init-throws.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-id-init-unresolvable.js#non_strict`: `Expected a ReferenceError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-id-init-unresolvable.js#strict`: `Expected a ReferenceError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-id-iter-step-err.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-id-iter-step-err.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-id-iter-val-err.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-id-iter-val-err.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-obj-val-null.js#non_strict`: `Expected a TypeError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-obj-val-null.js#strict`: `Expected a TypeError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-obj-val-undef.js#non_strict`: `Expected a TypeError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elem-obj-val-undef.js#strict`: `Expected a TypeError to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elision-step-err.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-elision-step-err.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-rest-id-elision-next-err.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-rest-id-elision-next-err.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-rest-id-iter-step-err.js#non_strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`
- `language/statements/async-generator/dstr/ary-ptrn-rest-id-iter-step-err.js#strict`: `Expected a Test262Error to be thrown but no exception was thrown at all`

### `language/expressions` (10 shown / 11 mismatches)

- `language/expressions/comma/tco-final.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/logical-and/tco-right.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/logical-or/tco-right.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/member-expression/computed-reference-null-or-undefined.js#non_strict`: `Expected a TypeError but got a Test262Error`
- `language/expressions/member-expression/computed-reference-null-or-undefined.js#strict`: `Expected a TypeError but got a Test262Error`
- `language/expressions/new/non-ctor-err-realm.js#non_strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`
- `language/expressions/new/non-ctor-err-realm.js#strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`
- `language/expressions/super/call-proto-not-ctor.js#non_strict`: `Expected SameValue(«"undefined"», «"object"») to be true`
- `language/expressions/super/call-proto-not-ctor.js#strict`: `Expected SameValue(«"undefined"», «"object"») to be true`
- `language/expressions/tagged-template/tco-call.js#strict`: `Maximum call stack size exceeded`

### `built-ins/Array` (6 shown / 6 mismatches)

- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#non_strict`: `Cannot convert a BigInt value to a number`
- `built-ins/Array/prototype/slice/coerced-start-end-grow.js#strict`: `Cannot convert a BigInt value to a number`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#non_strict`: `Actual [undefined, undefined, undefined, undefined] and expected [1, 2, undefined, undefined] should have the same contents.`
- `built-ins/Array/prototype/slice/coerced-start-end-shrink.js#strict`: `Actual [undefined, undefined, undefined, undefined] and expected [1, 2, undefined, undefined] should have the same contents.`
- `built-ins/Array/prototype/slice/resizable-buffer.js#non_strict`: `Actual [] and expected [0, 1, 2] should have the same contents.`
- `built-ins/Array/prototype/slice/resizable-buffer.js#strict`: `Actual [] and expected [0, 1, 2] should have the same contents.`

### `built-ins/Object` (4 shown / 4 mismatches)

- `built-ins/Object/prototype/toString/symbol-tag-generators-builtin.js#non_strict`: `Expected SameValue(«"[object Generator]"», «"[object Object]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-generators-builtin.js#strict`: `Expected SameValue(«"[object Generator]"», «"[object Object]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-override-instances.js#non_strict`: `Expected SameValue(«"[object Error]"», «"[object test262]"») to be true`
- `built-ins/Object/prototype/toString/symbol-tag-override-instances.js#strict`: `Cannot assign to read-only property`
