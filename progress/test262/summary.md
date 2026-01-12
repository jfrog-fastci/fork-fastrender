# test262 semantic baseline

- Report: `progress/test262/baseline.json`

## Summary

| Metric | Count |
| --- | ---: |
| Total cases | 15378 |
| Matched upstream expected | 12707 |
| Mismatched upstream expected | 2671 |
| Timeouts | 0 |

### Manifest expectations (kind)

| Kind | Count |
| --- | ---: |
| pass | 2291 |
| xfail | 13077 |
| skip | 10 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS (pass+matched) | 2291 |
| XFAIL (xfail+mismatched) | 2671 |
| SKIP | 10 |
| XPASS (xfail+matched) | 10406 |

### Mismatch classification (for `--fail-on`)

| Kind | Count |
| --- | ---: |
| expected | 2671 |
| unexpected | 0 |
| flaky | 0 |

## Breakdown by area

| Area | Total | PASS | XFAIL | SKIP | Unexpected | Timeouts | ΔPASS | ΔXFAIL | ΔTimeout |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `built-ins/Array` | 1124 | 0 | 161 | 10 | 0 | 0 |  |  |  |
| `built-ins/Boolean` | 101 | 100 | 1 | 0 | 0 | 0 |  |  |  |
| `built-ins/Function` | 86 | 86 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/JSON` | 330 | 0 | 90 | 0 | 0 | 0 |  |  |  |
| `built-ins/Math` | 654 | 0 | 46 | 0 | 0 | 0 |  |  |  |
| `built-ins/Number` | 302 | 302 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/Object` | 1664 | 570 | 130 | 0 | 0 | 0 |  |  |  |
| `built-ins/Promise` | 64 | 64 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/String` | 768 | 82 | 134 | 0 | 0 | 0 |  |  |  |
| `built-ins/Symbol` | 184 | 42 | 18 | 0 | 0 | 0 |  |  |  |
| `built-ins/WeakMap` | 8 | 8 | 0 | 0 | 0 | 0 |  |  |  |
| `built-ins/WeakSet` | 6 | 6 | 0 | 0 | 0 | 0 |  |  |  |
| `language/block-scope` | 287 | 0 | 191 | 0 | 0 | 0 |  |  |  |
| `language/directive-prologue` | 62 | 0 | 6 | 0 | 0 | 0 |  |  |  |
| `language/expressions` | 2325 | 1030 | 207 | 0 | 0 | 0 |  |  |  |
| `language/function-code` | 281 | 0 | 2 | 0 | 0 | 0 |  |  |  |
| `language/statements` | 7131 | 0 | 1685 | 0 | 0 | 0 |  |  |  |
| `staging/sm` | 1 | 1 | 0 | 0 | 0 | 0 |  |  |  |
