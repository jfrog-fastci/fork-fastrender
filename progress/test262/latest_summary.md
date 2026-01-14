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
| Total cases | 17437 |
| Matched upstream expected | 17321 (99.33%) |
| Mismatched upstream expected | 116 (0.67%) |
| Timeouts | 0 |
| Skipped | 48 |
| Unexpected mismatches | 0 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 17273 |
| failed | 116 |
| timed_out | 0 |
| skipped | 48 |

### Expectations (manifest)

| Kind | Count |
| --- | ---: |
| pass | 16498 |
| xfail | 891 |
| skip | 48 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS | 16498 |
| FAIL (unexpected) | 0 |
| XFAIL | 116 |
| XPASS | 775 |
| SKIP | 48 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| built-ins | 7185 | 7185 | 0 | 0.00% | 7089 | 0 | 0 | 48 | 48 |
| language | 10247 | 10131 | 116 | 1.13% | 9404 | 0 | 116 | 727 | 0 |
| staging | 5 | 5 | 0 | 0.00% | 5 | 0 | 0 | 0 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7208 | 107 | 1.48% | 6386 | 0 | 107 | 715 | 0 |
| `language/expressions` | 2409 | 9 | 0.37% | 2398 | 0 | 9 | 2 | 0 |
| `built-ins/Array` | 1503 | 0 | 0.00% | 1443 | 0 | 0 | 12 | 48 |
| `built-ins/Boolean` | 101 | 0 | 0.00% | 101 | 0 | 0 | 0 | 0 |
| `built-ins/Error` | 2 | 0 | 0.00% | 2 | 0 | 0 | 0 | 0 |
| `built-ins/Function` | 96 | 0 | 0.00% | 96 | 0 | 0 | 0 | 0 |
| `built-ins/JSON` | 330 | 0 | 0.00% | 316 | 0 | 0 | 14 | 0 |
| `built-ins/Map` | 405 | 0 | 0.00% | 403 | 0 | 0 | 2 | 0 |
| `built-ins/Math` | 654 | 0 | 0.00% | 652 | 0 | 0 | 2 | 0 |
| `built-ins/Number` | 302 | 0 | 0.00% | 302 | 0 | 0 | 0 | 0 |

(Total buckets: 22; buckets with 0 mismatches: 20)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 116 (100.00%)
- VmError::Unimplemented: 0 (0.00%)
- termination: 0 (0.00%)

### Top 20

| # | Kind | Count | Reason |
| ---: | --- | ---: | --- |
| 1 | exception/other | 23 | `Maximum call stack size exceeded` |
| 2 | exception/other | 10 | `#18: value === undefined. Actual:  value ===value` |
| 3 | exception/other | 9 | `Expected SameValue(«1», «undefined») to be true` |
| 4 | exception/other | 8 | `#0: result === "value". Actual:  result ===myObj_value` |
| 5 | exception/other | 6 | `Object` |
| 6 | exception/other | 6 | `TypeError: Cannot convert undefined or null to object` |
| 7 | exception/other | 4 | `#1: __evaluated === 4. Actual:  __evaluated ===4 Expected SameValue(«4», «undefined») to be true` |
| 8 | exception/other | 4 | `Expected SameValue(«"inside"», «"outside"») to be true` |
| 9 | exception/other | 4 | `Expected SameValue(«3», «undefined») to be true` |
| 10 | exception/other | 4 | `Expected a TypeError but got a Test262Error` |
| 11 | exception/other | 3 | `#19: myObj.value === "value". Actual:  myObj.value ===myObj_value` |
| 12 | exception/other | 3 | `Expected SameValue(«6», «undefined») to be true` |
| 13 | exception/other | 2 | `#1: callee === 0. Actual: callee ===1` |
| 14 | exception/other | 2 | `#7.1: Exception.toString()==="URIError: message". Actual: Exception is TypeError: Error options must be an object` |
| 15 | exception/other | 2 | `#7: Exception.toString()==="URIError: message". Actual: Exception is TypeError: Error options must be an object` |
| 16 | exception/other | 2 | `Actual [has:Object, get:Symbol(Symbol.unscopables), get:Object] and expected [has:Object, get:Symbol(Symbol.unscopables), has:Object, get:Object] should have the same contents.` |
| 17 | exception/other | 2 | `Expected SameValue(«"undefined"», «"object"») to be true` |
| 18 | exception/other | 2 | `Expected SameValue(«[object AsyncGenerator]», «[object AsyncGenerator]») to be false` |
| 19 | exception/other | 2 | `Expected SameValue(«[object Generator]», «[object Generator]») to be false` |
| 20 | exception/other | 2 | `Expected a ReferenceError to be thrown but no exception was thrown at all` |

## Timed-out tests

_None._

## Appendix: top failing tests (IDs + first-line error)

At least 50 mismatched cases, grouped by the largest mismatch buckets.

(If the suite only has a few buckets with mismatches, the largest buckets will show more
than `--appendix-per-bucket` entries so the appendix still reaches the minimum count.)

### `language/statements` (41 shown / 107 mismatches)

- `language/statements/async-generator/generator-created-after-decl-inst.js#non_strict`: `Expected SameValue(«[object AsyncGenerator]», «[object AsyncGenerator]») to be false`
- `language/statements/async-generator/generator-created-after-decl-inst.js#strict`: `Expected SameValue(«[object AsyncGenerator]», «[object AsyncGenerator]») to be false`
- `language/statements/async-generator/return-undefined-implicit-and-explicit.js#non_strict`: `Test262Error: Actual [tick 1, tick 2, g1 ret, g2 ret, g3 ret, g4 ret] and expected [tick 1, g1 ret, g2 ret, tick 2, g3 ret, g4 ret] should have the same contents. Ticks for implicit and explicit return undefined`
- `language/statements/async-generator/return-undefined-implicit-and-explicit.js#strict`: `Test262Error: Actual [tick 1, tick 2, g1 ret, g2 ret, g3 ret, g4 ret] and expected [tick 1, g1 ret, g2 ret, tick 2, g3 ret, g4 ret] should have the same contents. Ticks for implicit and explicit return undefined`
- `language/statements/async-generator/yield-star-async-next.js#non_strict`: `TypeError: Cannot convert undefined or null to object`
- `language/statements/async-generator/yield-star-async-next.js#strict`: `TypeError: Cannot convert undefined or null to object`
- `language/statements/async-generator/yield-star-async-return.js#non_strict`: `TypeError: Cannot convert undefined or null to object`
- `language/statements/async-generator/yield-star-async-return.js#strict`: `TypeError: Cannot convert undefined or null to object`
- `language/statements/async-generator/yield-star-async-throw.js#non_strict`: `TypeError: Cannot convert undefined or null to object`
- `language/statements/async-generator/yield-star-async-throw.js#strict`: `TypeError: Cannot convert undefined or null to object`
- `language/statements/async-generator/yield-star-normal-notdone-iter-value-throws.js#non_strict`: `Object`
- `language/statements/async-generator/yield-star-normal-notdone-iter-value-throws.js#strict`: `Object`
- `language/statements/async-generator/yield-star-return-notdone-iter-value-throws.js#non_strict`: `Object`
- `language/statements/async-generator/yield-star-return-notdone-iter-value-throws.js#strict`: `Object`
- `language/statements/async-generator/yield-star-return-then-getter-ticks.js#non_strict`: `Test262Error: Actual [start, tick 1, get return, get return, tick 2, get then, tick 3] and expected [start, tick 1, get then, tick 2, get return, get then, tick 3] should have the same contents. Ticks for return with thenable getter`
- `language/statements/async-generator/yield-star-return-then-getter-ticks.js#strict`: `Test262Error: Actual [start, tick 1, get return, get return, tick 2, get then, tick 3] and expected [start, tick 1, get then, tick 2, get return, get then, tick 3] should have the same contents. Ticks for return with thenable getter`
- `language/statements/async-generator/yield-star-throw-notdone-iter-value-throws.js#non_strict`: `Object`
- `language/statements/async-generator/yield-star-throw-notdone-iter-value-throws.js#strict`: `Object`
- `language/statements/block/tco-stmt-list.js#strict`: `Maximum call stack size exceeded`
- `language/statements/block/tco-stmt.js#strict`: `Maximum call stack size exceeded`
- `language/statements/do-while/tco-body.js#strict`: `Maximum call stack size exceeded`
- `language/statements/for-of/typedarray-backed-by-resizable-buffer-shrink-mid-iteration.js#non_strict`: `Expected a TypeError but got a Test262Error`
- `language/statements/for-of/typedarray-backed-by-resizable-buffer-shrink-mid-iteration.js#strict`: `Expected a TypeError but got a Test262Error`
- `language/statements/for-of/typedarray-backed-by-resizable-buffer-shrink-to-zero-mid-iteration.js#non_strict`: `Expected a TypeError but got a Test262Error`
- `language/statements/for-of/typedarray-backed-by-resizable-buffer-shrink-to-zero-mid-iteration.js#strict`: `Expected a TypeError but got a Test262Error`
- `language/statements/for/head-init-expr-check-empty-inc-empty-completion.js#non_strict`: `#1: __evaluated === 4. Actual:  __evaluated ===4 Expected SameValue(«4», «undefined») to be true`
- `language/statements/for/head-init-expr-check-empty-inc-empty-completion.js#strict`: `#1: __evaluated === 4. Actual:  __evaluated ===4 Expected SameValue(«4», «undefined») to be true`
- `language/statements/for/head-init-var-check-empty-inc-empty-completion.js#non_strict`: `#1: __evaluated === 4. Actual:  __evaluated ===4 Expected SameValue(«4», «undefined») to be true`
- `language/statements/for/head-init-var-check-empty-inc-empty-completion.js#strict`: `#1: __evaluated === 4. Actual:  __evaluated ===4 Expected SameValue(«4», «undefined») to be true`
- `language/statements/for/tco-const-body.js#strict`: `Maximum call stack size exceeded`
- `language/statements/for/tco-let-body.js#strict`: `Maximum call stack size exceeded`
- `language/statements/for/tco-lhs-body.js#strict`: `Maximum call stack size exceeded`
- `language/statements/for/tco-var-body.js#strict`: `Maximum call stack size exceeded`
- `language/statements/function/S13.2.2_A17_T3.js#non_strict`: `#4: __obj.getRight() === false. Actual: __obj.getRight()===right`
- `language/statements/function/S13.2.2_A18_T1.js#non_strict`: `#1: callee === 0. Actual: callee ===1`
- `language/statements/function/S13.2.2_A18_T2.js#non_strict`: `#1: callee === 0. Actual: callee ===1`
- `language/statements/function/scope-paramsbody-var-open.js#non_strict`: `Expected SameValue(«"inside"», «"outside"») to be true`
- `language/statements/function/scope-paramsbody-var-open.js#strict`: `Expected SameValue(«"inside"», «"outside"») to be true`
- `language/statements/generators/generator-created-after-decl-inst.js#non_strict`: `Expected SameValue(«[object Generator]», «[object Generator]») to be false`
- `language/statements/generators/generator-created-after-decl-inst.js#strict`: `Expected SameValue(«[object Generator]», «[object Generator]») to be false`
- `language/statements/generators/scope-paramsbody-var-open.js#non_strict`: `Expected SameValue(«"inside"», «"outside"») to be true`

### `language/expressions` (9 shown / 9 mismatches)

- `language/expressions/comma/tco-final.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/logical-and/tco-right.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/logical-or/tco-right.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/new/non-ctor-err-realm.js#non_strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`
- `language/expressions/new/non-ctor-err-realm.js#strict`: `production including Arguments Expected a TypeError but got a different error constructor with the same name`
- `language/expressions/super/call-proto-not-ctor.js#non_strict`: `Expected SameValue(«"undefined"», «"object"») to be true`
- `language/expressions/super/call-proto-not-ctor.js#strict`: `Expected SameValue(«"undefined"», «"object"») to be true`
- `language/expressions/tagged-template/tco-call.js#strict`: `Maximum call stack size exceeded`
- `language/expressions/tagged-template/tco-member.js#strict`: `Maximum call stack size exceeded`
