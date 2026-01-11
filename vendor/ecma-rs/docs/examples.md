# Runnable examples

The workspace ships a small set of compiled examples intended for copy/paste
setups in downstream tools. They avoid filesystem I/O by using in-memory hosts
and produce deterministic output (stable ordering of diagnostics and queries).
These examples are compiled by CI via the `justfile`'s scoped `check-*` recipes.

Note: this workspace intentionally does not commit `Cargo.lock`. If you want to
run the examples with `--locked` (matching CI), generate it first:

```bash
bash scripts/cargo_agent.sh generate-lockfile
```

## Nested-workspace note (when vendored in FastRender)

When `ecma-rs` is vendored under `vendor/ecma-rs/` in the FastRender mono-repo,
you can run these examples from the FastRender repo root via:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh run -p <crate> --example <example>
```

Or, if you're already inside `vendor/ecma-rs/`:

```bash
bash scripts/cargo_agent.sh run -p <crate> --example <example>
```

## `diagnostics`

```bash
bash scripts/cargo_agent.sh run -p diagnostics --example diagnostics_basic
```

This example builds a `Diagnostic` and renders it with caret highlighting using
`SimpleFiles`.

## `parse-js`

```bash
bash scripts/cargo_agent.sh run -p parse-js --example parse_js_basic
```

This example parses a small TypeScript module with explicit `ParseOptions` and
prints a couple of basic stats.

## `typecheck-ts`

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts --example memory_host_basic
bash scripts/cargo_agent.sh run -p typecheck-ts --example json_snapshot
```

- `memory_host_basic` demonstrates `MemoryHost`, deterministic module resolution,
  rendering diagnostics, and common queries (`exports_of`, `type_at`, `symbol_at`,
  `display_type`).
- `json_snapshot` prints a minimal JSON payload (with a `schema_version`) that
  can be redirected to a file for snapshot tests:

  ```bash
  bash scripts/cargo_agent.sh run -p typecheck-ts --example json_snapshot > snapshot.json
  ```

## `types-ts-interned`

```bash
bash scripts/cargo_agent.sh run -p types-ts-interned --example types_ts_interned_basic
```

This example builds a few interned types (`{ x: number }`, union types, callable
signatures), prints their display forms, and demonstrates assignability queries
via `RelateCtx`.

## `hir-js`

```bash
bash scripts/cargo_agent.sh run -p hir-js --example basic_lowering
```

This example parses+lowers a small TypeScript snippet and shows how to use
`SpanMap` for byte-offset lookups.

## `semantic-js` (JS mode)

```bash
bash scripts/cargo_agent.sh run -p semantic-js --example js_mode_basic
```

This example binds+resolves a small JavaScript snippet in-memory and prints the
top-level symbols plus identifier resolutions.

## `emit-js`

```bash
bash scripts/cargo_agent.sh run -p emit-js --example emit_js_basic
```

This example parses a small TypeScript snippet with `parse-js` and prints the
minified emitted output.

## `ts-erase`

```bash
bash scripts/cargo_agent.sh run -p ts-erase --example ts_erase_basic
```

This example parses a small TypeScript module, erases TypeScript-only syntax via
`ts-erase`, and prints the minified JavaScript output using `emit-js`.

## `native-js` (LLVM)

```bash
# From the ecma-rs workspace root:
bash scripts/cargo_llvm.sh run -p native-js --example compile_and_run

# Or, from the FastRender repo root (when ecma-rs is vendored under vendor/ecma-rs/):
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js --example compile_and_run
```

This example compiles an in-memory TypeScript snippet to LLVM IR using
`native-js`'s minimal emitter, invokes `clang` to produce a temporary native
executable, runs it, and prints stdout.

It requires an LLVM 18 toolchain (including `clang-18`) on `PATH`. See:

- [`native-js/README.md`](../native-js/README.md)
- [`scripts/check_system.sh`](../scripts/check_system.sh)

## `optimize-js`

```bash
bash scripts/cargo_agent.sh run -p optimize-js --example optimize_js_basic
```

This example compiles a small JS snippet to SSA, runs the optimizer, and
decompiles the result back to emitted JavaScript.

## `minify-js`

```bash
bash scripts/cargo_agent.sh run -p minify-js --example minify_js_basic
```

This example minifies a small TypeScript snippet and prints the emitted
JavaScript to stdout.
