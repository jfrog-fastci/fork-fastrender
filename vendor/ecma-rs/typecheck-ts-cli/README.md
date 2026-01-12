# typecheck-ts-cli

Standalone CLI for running the `typecheck-ts` engine against on-disk TypeScript
sources.

## Usage

```bash
# From the FastRender repo root (vendored workspace):
bash vendor/ecma-rs/scripts/cargo_agent.sh run -p typecheck-ts-cli -- typecheck fixtures/basic.ts

# Or, from within vendor/ecma-rs/:
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- typecheck fixtures/basic.ts

# Enforce the repo's strict-native TypeScript subset (see EXEC.plan):
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- typecheck --native-strict fixtures/basic.ts

# Load a real project via tsconfig.json (entries are optional in project mode):
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- typecheck --project path/to/tsconfig.json

# Request extra output:
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- typecheck fixtures/basic.ts \
  --type-at fixtures/basic.ts:42 \
  --symbol-at fixtures/basic.ts:17 \
  --exports fixtures/basic.ts

# Emit structured JSON (diagnostics + query results)
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- typecheck fixtures/basic.ts --json
```

### Options

- `--json`: emit structured JSON output (see below) with deterministic ordering.
- `--type-at <file:offset>`: inferred type at a byte offset within the file.
- `--symbol-at <file:offset>`: resolved symbol information at an offset.
- `--exports <file>`: export map for the file with symbol/type information.
- `--lib <name>`: explicit lib set (e.g. `es2020`, `dom`); overrides defaults.
- `--no-default-lib`: disable bundled libs.
- `--target`: select target lib set (`es5`, `es2015`, …).
- `--project` / `-p`: load `tsconfig.json` (compiler options, file discovery, and `baseUrl`/`paths` resolution).
- `--node-resolve`: enable Node/TS-style resolution (including `node_modules`).
- `--native-strict`: enforce the AOT-friendly TypeScript subset described in
  `EXEC.plan`.
- `--strict-native`: legacy alias for `--native-strict` (also available via
  `compilerOptions.nativeStrict` or `compilerOptions.strictNative` in
  `tsconfig.json`).
- `--trace` / `--profile`: emit tracing spans in JSON (compatible with the
  harness profiling format).

### Encoding

Source files are read as UTF-8. Offsets passed to `--type-at`/`--symbol-at` are
byte offsets in that UTF-8 text; invalid encodings cause the CLI to exit with an
error before rendering diagnostics.

### Module resolution

By default, imports are resolved relative to the importing file, checking
`<spec>.ts`, `<spec>.d.ts`, `<spec>.tsx`, `<spec>.js`, and `<spec>.jsx` plus
`index.*` variants. `--node-resolve` additionally walks up the directory tree
looking in `node_modules/`.

### `typeRoots` / `types` packages

The CLI follows the `typecheck-ts` core policy for ambient type packages:

- Both `compilerOptions.types` and `/// <reference types="..." />` are resolved
  by `typecheck-ts` via the host's `Host::resolve` hook, with a fallback that
  maps `foo` to `@types/foo` and `@scope/pkg` to `@types/scope__pkg` (matching
  `tsc`'s `@types` naming).
- In project mode, the CLI host uses `compilerOptions.typeRoots` (or the default
  `node_modules/@types` ancestors) to resolve `@types/*` specifiers to their
  `.d.ts` entrypoints.
- When `compilerOptions.types` is omitted in `tsconfig.json`, the CLI matches
  `tsc` by including all discoverable packages under `typeRoots` (in stable,
  sorted order) by expanding them into `CompilerOptions.types` before invoking
  the checker.

### Diagnostics

Human output uses `diagnostics::render` with file context. JSON output uses
stable ordering for diagnostics and query results to ease consumption by other
tools.

## JSON output schema

When `--json` is passed, the CLI emits a single JSON object:

```jsonc
{
  "schema_version": 1,
  "files": ["..."],
  "diagnostics": ["..."],
  "queries": {
    "type_at": { "file": "...", "offset": 0, "type": "..." },
    "symbol_at": { "file": "...", "offset": 0, "symbol": 0 },
    "explain_assignability": { "src": { /* TypeAtResult */ }, "dst": { /* TypeAtResult */ }, "assignable": false },
    "exports": { "<file>": { "<name>": { "symbol": 0, "def": 0, "type": "..." } } }
  }
}
```

- `schema_version` is a monotonically increasing integer; consumers should gate
  parsing logic on this value.
- All file paths in JSON output (`files`, `queries.*.file`, export-map keys,
  etc.) are normalized to TypeScript-style virtual paths via
  `diagnostics::paths::normalize_ts_path`:
  - separators are `/`
  - Windows drive letters are lowercased (`C:\foo` → `c:/foo`)
  - `.`/`..` segments are collapsed
