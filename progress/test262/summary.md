# test262 semantic baseline

- Report: `progress/test262/baseline.json`

## Summary

| Metric | Count |
| --- | ---: |
| Total cases | 17008 |
| Matched upstream expected | 14871 |
| Mismatched upstream expected | 2137 |
| Timeouts | 0 |

### Manifest expectations (kind)

| Kind | Count |
| --- | ---: |
| pass | 5736 |
| xfail | 11244 |
| skip | 28 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS (pass+matched) | 5054 |
| XFAIL (xfail+mismatched) | 1455 |
| SKIP | 28 |
| Unexpected failures (pass+mismatched) | 682 |
| XPASS (xfail+matched) | 9789 |

### Mismatch classification (for `--fail-on`)

| Kind | Count |
| --- | ---: |
| expected | 1455 |
| unexpected | 682 |
| flaky | 0 |

## Breakdown by area

| Area | Total | PASS | XFAIL | SKIP | Unexpected | Timeouts | ΔPASS | ΔXFAIL | ΔTimeout |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `built-ins/Array` | 1503 | 401 | 122 | 28 | 6 | 0 |  |  |  |
| `built-ins/Boolean` | 101 | 99 | 0 | 0 | 2 | 0 |  |  |  |
| `built-ins/Error` | 2 | 2 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Function` | 96 | 92 | 0 | 0 | 4 | 0 |  |  |  |
| `built-ins/JSON` | 330 | 6 | 2 | 0 | 0 | 0 |  |  |  |
| `built-ins/Map` | 405 | 401 | 2 | 0 | 2 | 0 |  |  |  |
| `built-ins/Math` | 654 | 0 | 14 | 0 | 0 | 0 |  |  |  |
| `built-ins/Number` | 302 | 302 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Object` | 1664 | 1200 | 114 | 0 | 2 | 0 |  |  |  |
| `built-ins/Promise` | 64 | 64 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/RegExp` | 32 | 32 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Set` | 764 | 388 | 304 | 0 | 2 | 0 |  |  |  |
| `built-ins/String` | 770 | 270 | 14 | 0 | 0 | 0 |  |  |  |
| `built-ins/Symbol` | 184 | 44 | 6 | 0 | 0 | 0 |  |  |  |
| `built-ins/WeakMap` | 8 | 0 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/WeakSet` | 6 | 0 | 0 | 0 | 0 | 0 |  |  |  |
| `language/block-scope` | 287 | 0 | 2 | 0 | 0 | 0 |  |  |  |
| `language/directive-prologue` | 62 | 0 | 6 | 0 | 0 | 0 |  |  |  |
| `language/expressions` | 2333 | 1032 | 197 | 0 | 0 | 0 |  |  |  |
| `language/function-code` | 281 | 0 | 0 | 0 | 0 | 0 |  |  |  |
| `language/statements` | 7159 | 720 | 672 | 0 | 664 | 0 |  |  |  |
| `staging/sm` | 1 | 1 | 0 | 0 | 0 | 0 |  |  |  |
