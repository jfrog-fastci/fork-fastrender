# test262 (curated) — latest summary

Committed snapshot of `vm-js` conformance on the curated `test262-semantic` suite.

## Command

```bash
# from repo root
CARGO_TARGET_DIR=../../target timeout -k 10 600 bash scripts/cargo_agent.sh run -p test262-semantic --release -- \
  --harness test262 \
  --suite-path ../../tests/js/test262_suites/curated.toml \
  --manifest ../../tests/js/test262_manifest.toml \
  --timeout-secs 10 \
  --jobs 4 \
  --report-path ../../target/js/test262.json \
  --fail-on new
```

- RegExp-focused suite (separate from the curated suite):
  ```bash
  # from repo root
  CARGO_TARGET_DIR=../../target timeout -k 10 600 bash scripts/cargo_agent.sh run -p test262-semantic --release -- \
    --harness test262 \
    --suite-path ../../tests/js/test262_suites/regexp.toml \
    --manifest ../../tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path ../../target/js/test262_regexp.json \
    --fail-on new
  ```

- RegExp `/v` Unicode sets suite (large generated corpus; kept separate from `regexp.toml`):
  ```bash
  # from repo root
  CARGO_TARGET_DIR=../../target timeout -k 10 600 bash scripts/cargo_agent.sh run -p test262-semantic --release -- \
    --harness test262 \
    --suite-path ../../tests/js/test262_suites/regexp_unicode_sets.toml \
    --manifest ../../tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path ../../target/js/test262_regexp_unicode_sets.json \
    --fail-on new
  ```

- RegExp Unicode property escapes (generated) suite (large; some known slow cases are excluded in the suite file):
  ```bash
  # from repo root
  CARGO_TARGET_DIR=../../target timeout -k 10 600 bash scripts/cargo_agent.sh run -p test262-semantic --release -- \
    --harness test262 \
    --suite-path ../../tests/js/test262_suites/regexp_property_escapes_generated.toml \
    --manifest ../../tests/js/test262_manifest.toml \
    --timeout-secs 10 \
    --jobs 4 \
    --report-path ../../target/js/test262_regexp_property_escapes_generated.json \
    --fail-on new
  ```

- JSON report (not committed): `target/js/test262.json`
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
| Matched upstream expected | 15685 (90.57%) |
| Mismatched upstream expected | 1633 (9.43%) |
| Timeouts | 0 |
| Skipped | 40 |
| Unexpected mismatches | 672 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 15645 |
| failed | 1633 |
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
| XFAIL | 961 |
| XPASS | 7617 |
| SKIP | 40 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| language | 10128 | 8927 | 1201 | 11.86% | 1786 | 666 | 535 | 7141 | 0 |
| built-ins | 7185 | 6753 | 432 | 6.01% | 6237 | 6 | 426 | 476 | 40 |
| staging | 5 | 5 | 0 | 0.00% | 5 | 0 | 0 | 0 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7161 | 1090 | 15.22% | 754 | 666 | 424 | 5317 | 0 |
| `built-ins/Set` | 764 | 306 | 40.05% | 388 | 2 | 304 | 70 | 0 |
| `built-ins/Object` | 1692 | 110 | 6.50% | 1330 | 2 | 108 | 252 | 0 |
| `language/expressions` | 2337 | 103 | 4.41% | 1032 | 0 | 103 | 1202 | 0 |
| `built-ins/Array` | 1503 | 6 | 0.40% | 1457 | 0 | 6 | 0 | 40 |
| `language/directive-prologue` | 62 | 6 | 9.68% | 0 | 0 | 6 | 56 | 0 |
| `built-ins/Map` | 405 | 4 | 0.99% | 401 | 2 | 2 | 0 | 0 |
| `built-ins/String` | 820 | 4 | 0.49% | 814 | 0 | 4 | 2 | 0 |
| `built-ins/Symbol` | 184 | 2 | 1.09% | 44 | 0 | 2 | 138 | 0 |
| `language/block-scope` | 287 | 2 | 0.70% | 0 | 0 | 2 | 285 | 0 |

(Total buckets: 22; buckets with 0 mismatches: 12)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 1017 (62.28%)
- VmError::Unimplemented: 593 (36.31%)
- termination: 23 (1.41%)

### Top 20

| # | Kind | Count | Reason |
| ---: | --- | ---: | --- |
| 1 | VmError::Unimplemented | 413 | `unimplemented: async generator functions` |
| 2 | exception/other | 208 | `value is not callable` |
| 3 | exception/other | 116 | `Expected a Test262Error to be thrown but no exception was thrown at all` |
| 4 | VmError::Unimplemented | 106 | `unimplemented: expression type` |
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
| 20 | exception/other | 10 | `#18: value === undefined. Actual: value ===value` |

## Timed-out tests

_None._
