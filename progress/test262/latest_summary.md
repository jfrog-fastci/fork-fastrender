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

## Overall

| Metric | Count |
| --- | ---: |
| Total cases | 17318 |
| Matched upstream expected | 15665 (90.46%) |
| Mismatched upstream expected | 1653 (9.54%) |
| Timeouts | 0 |
| Skipped | 40 |
| Unexpected mismatches | 672 |

### Outcomes (runner)

| Outcome | Count |
| --- | ---: |
| passed | 15625 |
| failed | 1653 |
| timed_out | 0 |
| skipped | 40 |

### Expectations (manifest)

| Kind | Count |
| --- | ---: |
| pass | 8696 |
| xfail | 8582 |
| skip | 40 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS | 8024 |
| FAIL (unexpected) | 672 |
| XFAIL | 981 |
| XPASS | 7601 |
| SKIP | 40 |

## Breakdown by major area

| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| language | 10128 | 8907 | 1221 | 12.06% | 1786 | 666 | 555 | 7121 | 0 |
| built-ins | 7185 | 6753 | 432 | 6.01% | 6233 | 6 | 426 | 480 | 40 |
| staging | 5 | 5 | 0 | 0.00% | 5 | 0 | 0 | 0 | 0 |

## Top failing buckets (by mismatched cases)

| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `language/statements` | 7161 | 1110 | 15.50% | 754 | 666 | 444 | 5297 | 0 |
| `built-ins/Set` | 764 | 306 | 40.05% | 388 | 2 | 304 | 70 | 0 |
| `built-ins/Object` | 1692 | 110 | 6.50% | 1330 | 2 | 108 | 252 | 0 |
| `language/expressions` | 2337 | 103 | 4.41% | 1032 | 0 | 103 | 1202 | 0 |
| `built-ins/Array` | 1503 | 6 | 0.40% | 1453 | 0 | 6 | 4 | 40 |
| `language/directive-prologue` | 62 | 6 | 9.68% | 0 | 0 | 6 | 56 | 0 |
| `built-ins/Map` | 405 | 4 | 0.99% | 401 | 2 | 2 | 0 | 0 |
| `built-ins/String` | 820 | 4 | 0.49% | 814 | 0 | 4 | 2 | 0 |
| `built-ins/Symbol` | 184 | 2 | 1.09% | 44 | 0 | 2 | 138 | 0 |
| `language/block-scope` | 287 | 2 | 0.70% | 0 | 0 | 2 | 285 | 0 |

(Total buckets: 22; buckets with 0 mismatches: 12)

## Top mismatch reasons (first line of `error`)

Mismatched cases by high-level bucket:
- exception/other: 1017 (61.52%)
- VmError::Unimplemented: 613 (37.08%)
- termination: 23 (1.39%)

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
| 14 | VmError::Unimplemented | 20 | `unimplemented: binary operator` |
| 15 | exception/other | 16 | `Expected true but got false` |
| 16 | exception/other | 16 | `desc.writable Expected SameValue(«true», «false») to be true` |
| 17 | exception/other | 14 | `Built-in objects must be extensible. Expected SameValue(«false», «true») to be true` |
| 18 | exception/other | 14 | `GetSetRecord coerces size Expected SameValue(«0», «1») to be true` |
| 19 | exception/other | 14 | `isConstructor invoked with a non-function value` |
| 20 | VmError::Unimplemented | 11 | `unimplemented: yield in expression type` |

## Timed-out tests

_None._
