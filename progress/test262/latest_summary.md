# test262 curated snapshot

- Date (UTC): `2026-01-12T09:55:58+00:00`
- Git HEAD: `52344be01daabaed53c7d08a46ecb174e8130d12` (dirty)
- Command: `timeout -k 10 600 bash scripts/cargo_agent.sh xtask js test262 --suite curated --fail-on none --report target/js/test262.json --summary target/js/test262_summary.md`

## Totals

| Metric | Count |
| --- | ---: |
| Total | 15209 |
| Passed | 10069 (66.20%) |
| Failed | 5138 |
| Timed out | 2 |
| Skipped | 0 |

## Mismatches (manifest-aware)

| Kind | Count |
| --- | ---: |
| Unexpected | 0 |
| Expected | 4034 |
| Flaky | 0 |

## Top failing areas (first two path components, top 10)

| Area prefix | Count |
| --- | ---: |
| `language/statements` | 3159 |
| `built-ins/Object` | 570 |
| `built-ins/Array` | 345 |
| `language/expressions` | 265 |
| `built-ins/String` | 254 |
| `language/block-scope` | 209 |
| `built-ins/JSON` | 106 |
| `built-ins/Number` | 82 |
| `built-ins/Math` | 48 |
| `built-ins/Symbol` | 48 |

## Top failure reasons (first line of `error`, top 10)

| Reason | Count |
| --- | ---: |
| `value is not callable` | 539 |
| `unimplemented: async generator functions` | 528 |
| `unimplemented: class inheritance` | 451 |
| `negative expectation mismatch: expected parse SyntaxError, got runtime <unknown error type>` | 441 |
| `Cannot convert undefined or null to object` | 252 |
| `unimplemented: GeneratorResumeAbrupt` | 200 |
| `error[PS0013]: expected token BracketClose` | 192 |
| `error[PS0002]: expected statement (not a declaration)` | 133 |
| `error[PS0002]: expected identifier` | 119 |
| `unimplemented: expression type` | 118 |

## Timed-out tests

- `built-ins/Array/prototype/concat/arg-length-exceeding-integer-limit.js#non_strict`
- `built-ins/Array/prototype/concat/arg-length-exceeding-integer-limit.js#strict`
