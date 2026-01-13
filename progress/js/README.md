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

Note: `pageset_js_failures.json` intentionally keeps a stable shape (explicit zero/empty fields) so
it stays easy to consume programmatically; the tool’s `target/pageset/js_report.json` may omit
zero-valued fields for compactness.

## Note on missing telemetry

The pageset scoreboard (`progress/pages/*.json`) only contains JS failure telemetry when it was
generated with JavaScript execution enabled (via `pageset_progress run --js`).

If `pages_with_js` is `0`, the committed progress artifacts were generated without `--js` (the
default) and therefore omit `diagnostics.stats.js`. Rerun with `--js` to populate the telemetry, then
regenerate the report.

```bash
# From repo root, after `fetches/html/` caches exist:
bash scripts/cargo_agent.sh run --release --bin pageset_progress -- \
  run --js --js-max-frames 10
```

Note: enabling JavaScript may require raising `--timeout`/`--soft-timeout-ms` because it drives an
event loop and can execute additional scripts/tasks before rendering the final frame.
