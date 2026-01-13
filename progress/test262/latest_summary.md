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

- JSON report (not committed): `target/js/test262.json`
- Note: `scripts/cargo_agent.sh` runs the vendored `test262-semantic` workspace from `vendor/ecma-rs/`,
  so the `../../...` paths above are relative to that directory.
- Note: `CARGO_TARGET_DIR=../../target` keeps build artifacts under the repo-root `target/` (avoids
  creating `vendor/ecma-rs/target/`).

## Overall

| Metric | Count |
| --- | ---: |
| Total cases | 15378 |
| Matched upstream expected | 12701 (82.59%) |
| Mismatched upstream expected | 2677 (17.41%) |
| Timeouts | 2 |
| Skipped | 8 |
| Unexpected mismatches | 0 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 11587 |
| failed | 3781 |
| timed_out | 2 |
| skipped | 8 |

### Expectations (manifest)

| Kind | Count |
| --- | ---: |
| pass | 1768 |
| xfail | 13602 |
| skip | 8 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS | 1768 |
| FAIL (unexpected) | 0 |
| XFAIL | 2677 |
| XPASS | 10925 |
| SKIP | 8 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| language | 10086 | 7993 | 2093 | 20.75% | 1028 | 0 | 2093 | 6965 | 0 |
| built-ins | 5291 | 4707 | 584 | 11.04% | 740 | 0 | 584 | 3959 | 8 |
| staging | 1 | 1 | 0 | 0.00% | 0 | 0 | 0 | 1 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7131 | 1687 | 23.66% | 0 | 0 | 1687 | 5444 | 0 |
| `language/expressions` | 2325 | 207 | 8.90% | 1028 | 0 | 207 | 1090 | 0 |
| `language/block-scope` | 287 | 191 | 66.55% | 0 | 0 | 191 | 96 | 0 |
| `built-ins/Array` | 1124 | 163 | 14.50% | 0 | 0 | 163 | 953 | 8 |
| `built-ins/String` | 768 | 134 | 17.45% | 82 | 0 | 134 | 552 | 0 |
| `built-ins/Object` | 1664 | 130 | 7.81% | 538 | 0 | 130 | 996 | 0 |
| `built-ins/JSON` | 330 | 92 | 27.88% | 0 | 0 | 92 | 238 | 0 |
| `built-ins/Math` | 654 | 46 | 7.03% | 0 | 0 | 46 | 608 | 0 |
| `built-ins/Symbol` | 184 | 18 | 9.78% | 42 | 0 | 18 | 124 | 0 |
| `language/directive-prologue` | 62 | 6 | 9.68% | 0 | 0 | 6 | 56 | 0 |

(Total buckets: 18; buckets with 0 mismatches: 6)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 2027 (75.72%)
- VmError::Unimplemented: 646 (24.13%)
- termination: 4 (0.15%)

### Top 20

| # | Kind | Count | Reason |
| ---: | --- | ---: | --- |
| 1 | exception/other | 528 | `async generator functions` |
| 2 | VmError::Unimplemented | 447 | `unimplemented: class inheritance` |
| 3 | exception/other | 396 | `negative expectation mismatch: expected parse SyntaxError, got runtime <unknown error type>` |
| 4 | exception/other | 158 | `Cannot convert undefined or null to object` |
| 5 | exception/other | 66 | `Expected SameValue(«"xCls2"», «"xCls2"») to be false` |
| 6 | exception/other | 66 | `Expected a Test262Error to be thrown but no exception was thrown at all` |
| 7 | exception/other | 60 | `Expected SameValue(«"xCover"», «"xCover"») to be false` |
| 8 | VmError::Unimplemented | 58 | `unimplemented: unary operator` |
| 9 | exception/other | 55 | `Expected a TypeError to be thrown but no exception was thrown at all` |
| 10 | exception/other | 52 | `value is not callable` |
| 11 | VmError::Unimplemented | 48 | `unimplemented: expression type` |
| 12 | exception/other | 45 | `negative expectation mismatch: expected parse SyntaxError, got runtime SyntaxError` |
| 13 | exception/other | 42 | `GetIterator: value is not iterable` |
| 14 | exception/other | 39 | `Expected a ReferenceError to be thrown but no exception was thrown at all` |
| 15 | VmError::Unimplemented | 34 | `unimplemented: binary operator` |
| 16 | exception/other | 24 | `invalid handle (vm-js/src/heap.rs:1911:16)` |
| 17 | exception/other | 23 | `f is not defined` |
| 18 | exception/other | 16 | `should not be called` |
| 19 | VmError::Unimplemented | 16 | `unimplemented: BigInt literal out of range` |
| 20 | exception/other | 14 | `Expected true but got false` |

## Timed-out tests

- `built-ins/Array/prototype/concat/Array.prototype.concat_large-typed-array.js#non_strict`
- `built-ins/Array/prototype/concat/Array.prototype.concat_large-typed-array.js#strict`
