# test262 semantic baseline

- Report: `progress/test262/baseline.json`

## Summary

| Metric | Count |
| --- | ---: |
| Total cases | 17318 |
| Matched upstream expected | 15695 |
| Mismatched upstream expected | 1623 |
| Timeouts | 0 |

### Manifest expectations (kind)

| Kind | Count |
| --- | ---: |
| pass | 8700 |
| xfail | 8578 |
| skip | 40 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS (pass+matched) | 8028 |
| XFAIL (xfail+mismatched) | 951 |
| SKIP | 40 |
| Unexpected failures (pass+mismatched) | 672 |
| XPASS (xfail+matched) | 7627 |

### Mismatch classification (for `--fail-on`)

| Kind | Count |
| --- | ---: |
| expected | 951 |
| unexpected | 672 |
| flaky | 0 |

## Breakdown by area

| Area | Total | PASS | XFAIL | SKIP | Unexpected | Timeouts | ΔPASS | ΔXFAIL | ΔTimeout |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `built-ins/Array` | 1503 | 1457 | 6 | 40 | 0 | 0 |  |  |  |
| `built-ins/Boolean` | 101 | 101 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Error` | 2 | 2 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Function` | 96 | 96 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/JSON` | 330 | 330 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Map` | 405 | 401 | 2 | 0 | 2 | 0 |  |  |  |
| `built-ins/Math` | 654 | 654 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Number` | 302 | 302 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Object` | 1692 | 1330 | 108 | 0 | 2 | 0 |  |  |  |
| `built-ins/Promise` | 64 | 64 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/RegExp` | 254 | 254 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Set` | 764 | 388 | 304 | 0 | 2 | 0 |  |  |  |
| `built-ins/String` | 820 | 814 | 4 | 0 | 0 | 0 |  |  |  |
| `built-ins/Symbol` | 184 | 44 | 2 | 0 | 0 | 0 |  |  |  |
| `built-ins/WeakMap` | 8 | 0 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/WeakSet` | 6 | 0 | 0 | 0 | 0 | 0 |  |  |  |
| `language/block-scope` | 287 | 0 | 2 | 0 | 0 | 0 |  |  |  |
| `language/directive-prologue` | 62 | 0 | 6 | 0 | 0 | 0 |  |  |  |
| `language/expressions` | 2337 | 1032 | 103 | 0 | 0 | 0 |  |  |  |
| `language/function-code` | 281 | 0 | 0 | 0 | 0 | 0 |  |  |  |
| `language/statements` | 7161 | 754 | 414 | 0 | 666 | 0 |  |  |  |
| `staging/sm` | 5 | 5 | 0 | 0 | 0 | 0 |  |  |  |
