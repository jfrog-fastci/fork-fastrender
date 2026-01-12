# WebIDL stack (consolidated)

FastRender’s WebIDL implementation is split into:

- **Generic JS/WebIDL infrastructure** in `vendor/ecma-rs/` (owned by FastRender; modify it freely).
- **FastRender-specific bindings and embedding glue** in `src/js/`.

This document is the **single source of truth** for where WebIDL-related code belongs after the
WebIDL consolidation work (see also: [`instructions/ecma_rs_ownership.md`](../instructions/ecma_rs_ownership.md)).

## Crate / code layout

### `vendor/ecma-rs/webidl` (crate: `webidl`)

Runtime-independent WebIDL implementation:

- **WebIDL IR + parsing utilities** (post-consolidation, this lives under `webidl::ir`)
- **WebIDL conversions** (JS ↔ IDL) and helpers (`DOMString`/`USVString`, numeric conversions,
  sequences/records/unions, etc.)
- **Overload resolution** (the WebIDL overload selection algorithm)
- **Runtime traits** that conversions/overload-resolution are defined against (the “what the JS VM
  must provide” boundary)

If you need a *new WebIDL algorithm* or want to improve correctness/perf of an existing one, it
belongs here.

### `vendor/ecma-rs/webidl-vm-js` (crate: `webidl-vm-js`)

`vm-js` adapter for `webidl`:

- Implements the `webidl` runtime traits on top of `vm-js` values/objects/symbols.
- Owns the **rooting/lifetime strategy** needed to safely run WebIDL conversions while `vm-js` GC can
  happen during allocations.
- Contains **generic** glue used by generated bindings when targeting a real `vm-js` realm (host
  dispatch helpers, common conversion helpers, binding installer/runtime primitives).

If you need a new feature in the `webidl` ↔ `vm-js` adapter (rooting, iterator helpers, host dispatch
plumbing), it belongs here.

### `vendor/ecma-rs/webidl-runtime` (Cargo package: `webidl-js-runtime`; Rust crate: `webidl_js_runtime`)

Legacy / compatibility runtime pieces:

- A **heap-only** WebIDL runtime adapter and/or binding installation helpers that predate the
  realm-based `vm-js` bindings work.
- Retained for migration/testing where it’s still referenced.

If you are implementing new WebIDL-driven bindings for FastRender, prefer the `webidl-vm-js` realm
path. Only touch `webidl-runtime` when maintaining legacy code paths.

### `src/js/webidl/*` (FastRender-specific)

FastRender’s in-tree bindings and integration layer:

- Generated binding installers (from `xtask webidl-bindings`):
  - `src/js/webidl/bindings/generated/`
- Hand-written FastRender glue around generated code (installation into realms, host dispatch wiring,
  and FastRender-specific helpers).

If you are implementing an actual Web Platform API (DOM/Web APIs) *as exposed by FastRender*, the
code belongs under `src/js/` (often `src/js/webidl/`, sometimes adjacent JS subsystems depending on
the feature).

## Rules of thumb (where new code goes)

- Need a new **WebIDL spec algorithm** (conversion, overload rule, IR parsing feature)?
  - Put it in `vendor/ecma-rs/webidl` (or `vendor/ecma-rs/webidl-vm-js` if it’s specifically about
    running the algorithm on `vm-js`).
- Need a new **vm-js adapter capability** (rooting, property definition helpers, host dispatch glue)?
  - Put it in `vendor/ecma-rs/webidl-vm-js`.
- Need a new **FastRender binding** (implementing a DOM/Web API, calling into FastRender internals)?
  - Put it in `src/js/**`.

Do **not** create new WebIDL infrastructure crates under `crates/`. If something is generic JS/WebIDL
infrastructure, it belongs in `vendor/ecma-rs/` (see
[`instructions/ecma_rs_ownership.md`](../instructions/ecma_rs_ownership.md)).

## How to run

### Regenerate the committed WebIDL snapshot (`xtask webidl`)

This refreshes `src/webidl/generated/mod.rs` from the vendored spec sources.

```bash
# One-time (or when specs change): ensure the WebIDL spec submodules exist.
git submodule update --init \
  specs/whatwg-dom \
  specs/whatwg-html \
  specs/whatwg-url \
  specs/whatwg-fetch

# Regenerate the snapshot.
bash scripts/cargo_agent.sh xtask webidl

# CI-style check (do not write; fail if output differs).
bash scripts/cargo_agent.sh xtask webidl --check
```

### Regenerate bindings (`xtask webidl-bindings`)

This refreshes generated Rust glue (installers/dispatch stubs) from the committed snapshot world.

```bash
# Regenerate generated bindings glue.
bash scripts/cargo_agent.sh xtask webidl-bindings

# CI-style check (do not write; fail if output differs).
bash scripts/cargo_agent.sh xtask webidl-bindings --check
```

### Run scoped tests

Run the tests that correspond to what you changed:

```bash
# webidl crate tests (vendored ecma-rs workspace; `cargo_agent.sh` will auto-scope it).
bash scripts/cargo_agent.sh test -p webidl

# webidl-vm-js adapter tests.
bash scripts/cargo_agent.sh test -p webidl-vm-js

# Binding generator goldens / snapshot checks.
bash scripts/cargo_agent.sh test -p xtask --test webidl_bindings_snapshots_up_to_date

# FastRender integration tests that exercise WebIDL-driven bindings.
bash scripts/cargo_agent.sh test -p fastrender --test misc -- js_webidl
```

## Related docs

- WebIDL bindings/codegen pipeline: [`docs/webidl_bindings.md`](webidl_bindings.md)
- Consolidation plan / rationale: [`instructions/webidl_consolidation.md`](../instructions/webidl_consolidation.md)
