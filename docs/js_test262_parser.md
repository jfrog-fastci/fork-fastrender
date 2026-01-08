# JS test262 parser harness

FastRender tracks JavaScript **parser** conformance via the ecma-rs
[`test262` parser harness](../engines/ecma-rs/test262/README.md) (which drives
`tc39/test262-parser-tests`).

This suite is heavier than the curated semantics runner and is primarily useful when working on
parsing in `engines/ecma-rs`.

## 1) Initialize required submodules

From the FastRender repo root:

```bash
# JS engine (submodule)
git submodule update --init engines/ecma-rs

# test262 parser corpus (nested submodule inside ecma-rs)
git -C engines/ecma-rs submodule update --init test262/data
```

## 2) Run the harness

From the FastRender repo root:

```bash
cargo xtask js test262-parser
```

By default, the runner writes a JSON report to:

```
target/js/test262-parser.json
```

Run `cargo xtask js test262-parser --help` for the full, authoritative CLI (this doc only calls out
the flags we use the most).

## Key flags

- `--manifest <PATH>`
  - Override the expectations manifest (skip/xfail/flaky) used to classify known gaps.
  - When omitted, xtask defaults to: `tests/js/test262_parser_expectations.toml`.
- `--shard <index>/<total>`
  - Run a deterministic shard of the corpus (0-based index).
  - Example: `--shard 3/8` to run the 4th shard out of 8.
- `--fail-on <all|new|none>`
  - Controls which mismatches produce a non-zero exit code:
    - `new` (default): fail only on **unexpected** mismatches (not covered by the manifest).
    - `all`: fail on any mismatch (including expected/xfail/flaky).
    - `none`: always exit 0 (useful for generating reports while iterating).

