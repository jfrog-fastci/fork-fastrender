# typecheck-ts

`typecheck-ts` is a lightweight, deterministic TypeScript type checker facade for the
`ecma-rs` toolchain.

The public API is intentionally small:

- [`Program`](src/program/api.rs): the main entry point for parsing/binding/type checking and queries.
- [`Host`](src/program/api.rs): your environment adapter (file text + module resolution + options).
- Stable identifiers (`FileId`, `DefId`, `BodyId`, `ExprId`, `TypeId`) that are cheap to copy and
  safe to cache *within the lifetime of a `Program`*.

This crate does **not** hardcode filesystem access or Node/TypeScript module resolution.
Instead, module resolution is always routed through your [`Host::resolve`](src/program/api.rs).
If you *do* want Node/TS-style resolution, enable the optional `resolve` feature and reuse the
helper resolver implementation.

## Design principles (why the API looks like this)

- **Determinism by default.** Same inputs → same outputs regardless of scheduling.
  (Stable ordering for roots, exports, diagnostics, and profiling output.)
- **Host-driven environment.** The checker does not assume paths, `node_modules`, or OS I/O.
- **Two-level checking model.**
  - *Global* work (parse → lower → bind → module graph) happens once per file.
  - *Local* work (expression typing, inference, control-flow narrowing) is computed per body via
    [`Program::check_body`](src/program/api/typing.rs), producing side tables keyed by `ExprId`/`PatId`.
- **Parallel-friendly.** `Program` is `Send + Sync`, and body checking does not require holding a
  global lock for the duration of the check.

## Quick start (public API)

The built-in [`MemoryHost`](src/lib.rs) is a convenient starting point for tests and tools that
already have an in-memory file graph.

```rust
use typecheck_ts::{FileKey, MemoryHost, Program};

let entry = FileKey::new("index.ts");
let dep = FileKey::new("math.ts");

let mut host = MemoryHost::new();
host.insert(entry.clone(), r#"import { add } from "./math"; export const total = add(1, 2);"#);
host.insert(dep.clone(), "export function add(a: number, b: number) { return a + b; }");
host.link(entry.clone(), "./math", dep.clone());

let program = Program::new(host, vec![entry.clone()]);
let diagnostics = program.check();
assert!(diagnostics.is_empty());

let entry_id = program.file_id(&entry).unwrap();
let exports = program.exports_of(entry_id);
let total_def = exports.get("total").unwrap().def.unwrap();
let total_ty = program.type_of_def(total_def);
assert_eq!(program.display_type(total_ty).to_string(), "number");
```

For runnable, larger examples (including diagnostics rendering and JSON snapshots), see:

- [`examples/memory_host_basic.rs`](examples/memory_host_basic.rs)
- [`examples/json_snapshot.rs`](examples/json_snapshot.rs) (requires the `serde` feature)

```bash
# From vendor/ecma-rs/
bash scripts/cargo_agent.sh run -p typecheck-ts --example memory_host_basic
bash scripts/cargo_agent.sh run -p typecheck-ts --features serde --example json_snapshot
```

## Stable IDs and side tables

`typecheck-ts` is designed so downstream callers never need to depend on internal arenas or AST
shapes.

### IDs you will see

- `FileKey`: your stable file identifier (usually a normalized virtual path).
- `FileId`: program-internal numeric id for a loaded file.
- `DefId` / `BodyId`: stable-ish, content-addressed ids produced by `hir-js` lowering.
- `ExprId` / `PatId`: **indices local to a single body** (only meaningful with a `BodyId`).
- `TypeId`: an interned id from `types-ts-interned`.

### Body side tables

Expression and pattern types are produced by [`Program::check_body`](src/program/api/typing.rs),
which returns a [`BodyCheckResult`](src/program/api.rs). The result owns:

- `expr_types: Vec<TypeId>` indexed by `ExprId`
- `pat_types: Vec<TypeId>` indexed by `PatId`
- `expr_spans` / `pat_spans` (UTF-8 byte ranges) for mapping ids back to source text

This is the core “coarse-grained query” model: instead of a global query for “type of expression
X”, the checker type-checks one body and returns a side table.

## Implementing a `Host`

The checker expects a [`Host`](src/program/api.rs) implementation that is:

- `Send + Sync + 'static`
- deterministic for the duration of a `Program` (same `(from, specifier)` → same resolution)

### Required: `file_text`

```rust
fn file_text(&self, file: &FileKey) -> Result<Arc<str>, HostError>;
```

- Must return the full UTF-8 source text.
- All spans/offsets in this crate are **UTF-8 byte offsets** (not UTF-16).
- If you have raw bytes (filesystem I/O), validate/convert once and store as `Arc<str>`.

### Required: `resolve`

```rust
fn resolve(&self, from: &FileKey, specifier: &str) -> Option<FileKey>;
```

This is used for:

- `import` / `export from` edges
- `import()` in type positions (`import("./x").T`)
- `/// <reference path="..." />`
- `/// <reference types="..." />`
- `CompilerOptions.types` (tsconfig-style `types` packages)

If `resolve` returns `None`, the checker will usually emit `TS2307` (unresolved module) unless the
import is satisfied by an ambient module declaration (`declare module "..." { ... }`).

The checker records the observed module graph in its internal database; you can query the
file-backed resolution results later via:

- [`Program::resolve_module`](src/program/api/files.rs)
- [`Program::resolved_module_deps`](src/program/api/files.rs)

### Recommended: `file_kind`

```rust
fn file_kind(&self, file: &FileKey) -> FileKind;
```

Return `FileKind::Dts` for `.d.ts` files (libs, `@types/*`, ambient module declarations).
Return `Tsx`/`Jsx` when parsing JSX.

### Recommended: `compiler_options`

```rust
fn compiler_options(&self) -> CompilerOptions;
```

Key options that affect integration:

- `target`: influences the default `lib.*.d.ts` set.
- `libs`: **overrides** the default lib set entirely (like `tsc --lib ...`).
- `no_default_lib`: disables default libs (see “Libs” below).
- `types`: additional `@types` packages to include (see “Types packages” below).

### Optional: `lib_files`

```rust
fn lib_files(&self) -> Vec<LibFile>;
```

Use this to provide:

- custom global declaration files (`.d.ts`)
- libs when disabling the default `bundled-libs` feature
- environment-specific shims (e.g. custom `dom` stubs, test-only globals)

Non-`.d.ts` “libs” are ignored with a warning diagnostic.

### Host implementations

#### In-memory host

Use [`typecheck_ts::MemoryHost`](src/lib.rs) (also used by examples and tests).

#### On-disk host

For a complete on-disk integration (tsconfig loading, filesystem I/O, and Node-style resolution),
see [`typecheck-ts-cli`](../typecheck-ts-cli/README.md).

If you only need module resolution helpers, enable the `resolve` feature and build your `Host`
around the provided resolver:

```toml
[dependencies]
typecheck-ts = { path = "...", features = ["resolve"] }
```

The `resolve` feature keeps the core checker lightweight while letting disk-based tools opt into
deterministic Node/TS-style resolution (`node_modules`, `index.*`, extension probing, etc.).

## Libs (`lib.*.d.ts`) and `types` packages

### Default libs and `--lib`

With the default `bundled-libs` feature enabled, `typecheck-ts` embeds the official TypeScript
`lib.*.d.ts` files (pinned to the workspace TypeScript version) and loads them automatically:

- if `CompilerOptions.libs` is empty and `no_default_lib` is false:
  - the baseline ES lib is derived from `target` (`es5`, `es2015`, …)
  - `dom` is included by default
- if `CompilerOptions.libs` is non-empty:
  - it replaces the default lib set entirely (matching TypeScript)

### `noLib` / `no_default_lib`

`CompilerOptions.no_default_lib` disables loading the default bundled libs (TypeScript’s
`--noLib`/`--noDefaultLib` behaviour).

Additionally, if no explicit `libs` are set, `typecheck-ts` scans root files for
`/// <reference no-default-lib="true" />` (or `noLib="true"`) and treats it as `no_default_lib`.

Host-provided `lib_files()` are still included even when `no_default_lib` is set.

### `types` packages (`CompilerOptions.types`)

`CompilerOptions.types` is treated like `tsconfig.json`’s `compilerOptions.types`:

- Each entry is resolved through `Host::resolve`.
- If resolving `name` fails, the checker tries a TypeScript-style fallback:
  - `name` → `@types/name`
  - `@scope/pkg` → `@types/scope__pkg`
- Unresolved type packages produce an `unresolved module` diagnostic.

Triple-slash `/// <reference types="..." />` directives behave similarly, but are resolved
relative to the referencing file.

## Running the checker and querying results

### `check()`: program-wide diagnostics

[`Program::check`](src/program/api/diagnostics.rs) parses, binds, and type-checks all reachable
source files, returning the accumulated diagnostics.

If you need to handle host failures (`HostError`) or cancellation explicitly, use
[`Program::check_fallible`](src/program/api/diagnostics.rs).

### `check_body()`: per-body side tables

Use [`Program::check_body`](src/program/api/typing.rs) when you want the per-body side tables
(`ExprId`/`PatId` → `TypeId`) via [`BodyCheckResult`](src/program/api.rs). `check()` internally
checks bodies as well, so `check_body()` is also the way to retrieve the cached result.

### Offset-based queries (`type_at`, `symbol_at`)

When you have a `(file, offset)` location (e.g. from an editor), prefer:

- [`Program::type_at`](src/program/api/type_at.rs) → inferred `TypeId`
- [`Program::symbol_at`](src/program/api/symbols.rs) + [`Program::symbol_info`](src/program/api/symbols.rs)

Offsets are **UTF-8 byte offsets**.

```rust
use typecheck_ts::{FileKey, MemoryHost, Program};

let mut host = MemoryHost::new();
let file = FileKey::new("index.ts");
host.insert(file.clone(), "export const total = 1 + 2;");

let program = Program::new(host, vec![file.clone()]);
assert!(program.check().is_empty());

let file_id = program.file_id(&file).unwrap();
let plus_offset = "export const total = 1 ".len() as u32; // points at `+`
let ty = program.type_at(file_id, plus_offset).unwrap();
assert_eq!(program.display_type(ty).to_string(), "number");
```

## Determinism and parallelism notes

- `Program::new` sorts/deduplicates the provided `roots` list.
- Many query results are returned in deterministic order (`BTreeMap`/sorted vectors), but some
  APIs intentionally preserve “natural” checker ordering. If you need stable output for snapshots,
  sort diagnostics with `diagnostics::sort_diagnostics` (see examples).
- `Program` is `Send + Sync`. You can call `check_body` (or `type_at`, which triggers body checks)
  from multiple threads; results are cached and shared via `Arc`.
- For best determinism, normalize your `FileKey`s (e.g. `a/b.ts` with `/` separators) and ensure
  `Host::resolve` is deterministic.

## Cancellation

Cancellation is cooperative and is designed for harnesses/IDEs:

- Call [`Program::cancel`](src/program/api/cancellation.rs) from another thread to request
  cancellation.
- Fallible APIs (e.g. `check_fallible`) return `Err(FatalError::Cancelled)`.
- Convenience APIs (e.g. `check`) return a single cancellation diagnostic (`CANCELLED`).

Call [`Program::clear_cancellation`](src/program/api/cancellation.rs) before re-running work.

## Profiling: `QueryStats` + tracing

### Query-level stats

`Program` records per-query timings and cache statistics and exposes them via
[`Program::query_stats`](src/program/api/diagnostics.rs):

```rust
use typecheck_ts::{FileKey, MemoryHost, Program};

let mut host = MemoryHost::new();
let entry = FileKey::new("index.ts");
host.insert(entry.clone(), "export const x = 1;");

let program = Program::new(host, vec![entry]);
let _ = program.check();

let stats = program.query_stats();
// `stats` can be serialized when the `serde` feature is enabled.
let _ = stats;
```

### Structured tracing

The checker emits coarse `tracing` spans around key query boundaries (parse/lower/bind/check).
Library code does **not** install a global subscriber; binaries should configure `tracing` as
desired.

For end-to-end examples of `--trace`/`--profile` style integration, see:

- [`typecheck-ts-harness`](../typecheck-ts-harness/README.md)
- [`typecheck-ts-cli`](../typecheck-ts-cli/README.md)
