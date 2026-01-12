# `types-ts-interned`

Interned, deterministic TypeScript type representation + evaluator/relation engine.

## Features

- `strict-determinism` (enabled by default): panics on any stable-hash ID
  collision. This makes collision handling schedule-independent: you either get
  deterministic output, or a deterministic fail-fast error.
- `serde`: enables `serde` support for the core interned data structures (IDs,
  `TypeKind`, snapshots, etc.).
- `serde-json`: enables JSON helpers such as `TypeStore::debug_json`. Implies
  `serde`.

To opt out of fail-fast collision handling, disable default features:

```toml
types-ts-interned = { workspace = true, default-features = false }
```

## Runnable example

```bash
bash scripts/cargo_agent.sh run -p types-ts-interned --example types_ts_interned_basic
```

This example shows how to create a [`TypeStore`], intern structural types, and
run assignability checks via [`RelateCtx`].

## Name interning

`TypeStore` interns string names used by `PropKey::{String,Symbol}` and
`TypeKind::StringLiteral`.

- Prefer `TypeStore::intern_name_ref(&str)` when you already have a borrowed
  `&str` / `&String` (common in parsers/typecheckers). This avoids allocating on
  hits and uses a read-fast lookup path.
- Use `TypeStore::intern_name(String)` when you already own the `String` and can
  move it into the store (avoids allocating again on insert).

## Native layout model (experimental)

`types-ts-interned` also contains an LLVM-agnostic native layout engine used by
the strict-native AOT pipeline.

- Compute a native runtime layout for a type via `TypeStore::layout_of`.
- Inspect a layout via `TypeStore::layout(LayoutId) -> Layout`.
- For types that contain `TypeKind::Ref` nodes, callers can expand refs via
  `TypeStore::layout_of_evaluated` (using a `TypeExpander`) before computing a
  layout.

Function/closure representation:

- `TypeKind::Callable` lowers to a GC-managed pointer to a canonical closure
  object payload layout:
  - `fn_ptr` (opaque code pointer)
  - `env` (`PtrKind::GcAny`, a GC-managed pointer with unknown pointee layout)
- Object shapes with `call_signatures` / `construct_signatures` use the same
  `fn_ptr` + `env` header as a prefix before regular object properties, making
  callables-with-properties representable.
- Callable-like intersections (e.g. `((x: T) => U) & { foo: string }`) are also
  representable: when an intersection contains a callable (or callable object)
  member and all members are object-like, it lowers to a `GcObject` payload that
  starts with the canonical `fn_ptr` + `env` header followed by deterministically
  merged properties.

GC tracing helpers:

- `TypeStore::gc_trace(LayoutId)` produces a trace plan that includes tagged
  unions.
- `TypeStore::gc_ptr_offsets(LayoutId)` extracts unconditional GC pointer
  offsets (pointers that are present regardless of union discriminants).

String representation (native AOT):

- `TypeKind::String` / string literals currently lower to an **interned id**
  (`AbiScalar::U32`) rather than a GC-managed pointer. This keeps object/tuple
  shapes GC-traceable with simple flat pointer maps and matches the
  `runtime-native` string interner ABI (`InternedId`).

## Fuzzing

This crate exposes a fuzz entry point behind the `fuzzing` and `serde-json`
features:
`types_ts_interned::fuzz_type_graph(&[u8])`.

The `fuzz/type_graph` harness (wired up via `cargo-fuzz`) feeds arbitrary bytes
into `fuzz_type_graph`, which:

- Deserializes a (potentially cyclic) `TypeKind` graph from JSON.
- Builds interned types in a fresh `TypeStore`.
- Runs type evaluation/normalization and assignability checks with explicit
  depth/step limits.

Invariant: for any input, evaluation/relate must terminate and must not panic.

### Run

From the repo root:

```bash
# one-time
bash scripts/cargo_agent.sh install cargo-fuzz

# Note: `cargo-fuzz` defaults to AddressSanitizer, which reserves a very large virtual address space
# for shadow memory. `scripts/cargo_agent.sh` automatically bumps RLIMIT_AS for `fuzz` subcommands.

# run the fuzzer
bash scripts/cargo_agent.sh fuzz run type_graph
```

This repo pins a stable toolchain, but the `cargo-fuzz` subcommand requires
nightly-only compiler flags for sanitizer coverage. The workspace config opts
into `RUSTC_BOOTSTRAP=1` so `bash scripts/cargo_agent.sh fuzz …` works out of
the box.

If you prefer to use nightly explicitly, run:

```bash
bash scripts/cargo_agent.sh +nightly fuzz run type_graph
```
