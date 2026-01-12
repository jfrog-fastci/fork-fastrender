# `progress/js/`

Committed summaries derived from the pageset scoreboard (`progress/pages/*.json`).

## Regenerating the report

The `pageset_progress` binary can aggregate JavaScript failure telemetry that is stored (per page) in
`diagnostics.stats.js`:

```bash
# From repo root:
bash scripts/cargo_agent.sh run --release --bin pageset_progress -- \
  report --progress-dir progress/pages --top-js-errors 32
```

This writes:

- `target/pageset/js_report.md`
- `target/pageset/js_report.json`

Copy those outputs into:

- `progress/js/pageset_js_failures.md`
- `progress/js/pageset_js_failures.json`

## Note on missing telemetry

If `pages_with_js` is `0`, the committed progress artifacts do not currently contain JS telemetry.
In that case, the report will be empty/zeroed until the pageset harness records `diagnostics.stats.js`.
