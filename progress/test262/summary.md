# test262 semantic baseline

- Report: `progress/test262/baseline.json`

## Summary

| Metric | Count |
| --- | ---: |
| Total cases | 16960 |
| Matched upstream expected | 14362 |
| Mismatched upstream expected | 2598 |
| Timeouts | 0 |

### Manifest expectations (kind)

| Kind | Count |
| --- | ---: |
| pass | 4942 |
| xfail | 11990 |
| skip | 28 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS (pass+matched) | 4792 |
| XFAIL (xfail+mismatched) | 2448 |
| SKIP | 28 |
| Unexpected failures (pass+mismatched) | 150 |
| XPASS (xfail+matched) | 9542 |

### Mismatch classification (for `--fail-on`)

| Kind | Count |
| --- | ---: |
| expected | 2448 |
| unexpected | 150 |
| flaky | 0 |

## Breakdown by area

| Area | Total | PASS | XFAIL | SKIP | Unexpected | Timeouts | ΔPASS | ΔXFAIL | ΔTimeout |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `built-ins/Array` | 1503 | 341 | 129 | 28 | 6 | 0 |  |  |  |
| `built-ins/Boolean` | 101 | 101 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Error` | 2 | 2 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Function` | 96 | 96 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/JSON` | 330 | 0 | 82 | 0 | 0 | 0 |  |  |  |
| `built-ins/Map` | 405 | 403 | 2 | 0 | 0 | 0 |  |  |  |
| `built-ins/Math` | 654 | 0 | 46 | 0 | 0 | 0 |  |  |  |
| `built-ins/Number` | 302 | 302 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Object` | 1664 | 1182 | 126 | 0 | 0 | 0 |  |  |  |
| `built-ins/Promise` | 64 | 64 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Set` | 764 | 390 | 304 | 0 | 0 | 0 |  |  |  |
| `built-ins/String` | 770 | 236 | 44 | 0 | 0 | 0 |  |  |  |
| `built-ins/Symbol` | 184 | 42 | 10 | 0 | 0 | 0 |  |  |  |
| `built-ins/WeakMap` | 8 | 8 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/WeakSet` | 6 | 6 | 0 | 0 | 0 | 0 |  |  |  |
| `language/block-scope` | 287 | 0 | 89 | 0 | 0 | 0 |  |  |  |
| `language/directive-prologue` | 62 | 0 | 6 | 0 | 0 | 0 |  |  |  |
| `language/expressions` | 2329 | 1032 | 193 | 0 | 0 | 0 |  |  |  |
| `language/function-code` | 281 | 0 | 2 | 0 | 0 | 0 |  |  |  |
| `language/statements` | 7147 | 586 | 1415 | 0 | 144 | 0 |  |  |  |
| `staging/sm` | 1 | 1 | 0 | 0 | 0 | 0 |  |  |  |
