# megatest-harness

Regression/stress harness for the self-contained JavaScript programs under `vendor/ecma-rs/megatest/`.

It is intended as an early-warning system for:
- parser / lowering drift
- non-deterministic ID allocation
- optimizer panics
- unexpected behavior changes across the pipeline

## What it checks

For each `*.js` file under `vendor/ecma-rs/megatest/` (discovered + sorted deterministically), the harness records:

- **parse-js** (strict `Dialect::Ecma`, `SourceType::Module`)
  - top-level statement count
  - SHA256 of the serialized AST (`ast_sha256`)
- **hir-js**
  - cheap arena/collection counts (`defs`, `bodies`, `exprs`, `stmts`, …)
  - SHA256 of HIR ID allocation/mappings (`ids_sha256`)
- **optimize-js**
  - either a deterministic error summary (sorted diagnostics), or for successful compiles:
    - function count, instruction count, dominance calculation count
    - SHA256 of the decompiled/emitted JS (`decompiled_js_sha256`)

All expected values live in `baselines/baseline.json` and are compared in tests/CLI.

## Running

From the repository root:

```bash
# Fast check (parse + lower for all megatests):
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p megatest-harness

# Full check (also validates optimize-js results; slower):
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p megatest-harness -- --ignored
```

### Filtering

To run a subset of fixtures, set `MEGATEST_FILTER` to a substring of the relative path:

```bash
MEGATEST_FILTER=optimizable_0 \
  bash vendor/ecma-rs/scripts/cargo_agent.sh test -p megatest-harness
```

### Updating baselines

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh run -p megatest-harness -- --update-baselines
```

Note: `--update-baselines` always writes a baseline that covers the **full** corpus.
If `MEGATEST_FILTER` (or `--filter`) is set, only the matching entries are recomputed; the rest are
carried over from the existing baseline.
