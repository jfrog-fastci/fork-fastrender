# test262 semantic baseline

- Report: `progress/test262/baseline.json`

## Summary

| Metric | Count |
| --- | ---: |
| Total cases | 15209 |
| Matched upstream expected | 11177 |
| Mismatched upstream expected | 4032 |
| Timeouts | 0 |

### Manifest expectations (kind)

| Kind | Count |
| --- | ---: |
| pass | 1662 |
| xfail | 13545 |
| skip | 2 |
| flaky | 0 |

### Results vs expectations

| Status | Count |
| --- | ---: |
| PASS (pass+matched) | 1662 |
| XFAIL (xfail+mismatched) | 4032 |
| SKIP | 2 |
| XPASS (xfail+matched) | 9513 |

### Mismatch classification (for `--fail-on`)

| Kind | Count |
| --- | ---: |
| expected | 4032 |
| unexpected | 0 |
| flaky | 0 |

## Breakdown by area

| Area | Total | PASS | XFAIL | SKIP | Unexpected | Timeouts | ΔPASS | ΔXFAIL | ΔTimeout |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `built-ins/Array` | 1120 | 0 | 343 | 2 | 0 | 0 |  |  |  |
| `built-ins/Boolean` | 101 | 0 | 21 | 0 | 0 | 0 |  |  |  |
| `built-ins/JSON` | 330 | 0 | 106 | 0 | 0 | 0 |  |  |  |
| `built-ins/Math` | 654 | 0 | 48 | 0 | 0 | 0 |  |  |  |
| `built-ins/Number` | 302 | 0 | 82 | 0 | 0 | 0 |  |  |  |
| `built-ins/Object` | 1664 | 538 | 570 | 0 | 0 | 0 |  |  |  |
| `built-ins/String` | 768 | 82 | 254 | 0 | 0 | 0 |  |  |  |
| `built-ins/Symbol` | 184 | 42 | 48 | 0 | 0 | 0 |  |  |  |
| `language/block-scope` | 287 | 0 | 191 | 0 | 0 | 0 |  |  |  |
| `language/directive-prologue` | 62 | 0 | 6 | 0 | 0 | 0 |  |  |  |
| `language/expressions` | 2325 | 1000 | 265 | 0 | 0 | 0 |  |  |  |
| `language/function-code` | 281 | 0 | 16 | 0 | 0 | 0 |  |  |  |
| `language/statements` | 7131 | 0 | 2082 | 0 | 0 | 0 |  |  |  |
