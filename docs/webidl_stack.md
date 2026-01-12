# WebIDL stack (post-consolidation)

FastRender’s WebIDL implementation is split into:

- **Generic JS/WebIDL infrastructure** in `vendor/ecma-rs/` (owned by FastRender; modify it freely).
- **FastRender-specific bindings and embedding glue** in `src/js/`.

This document is the contributor-facing “where does this live?” reference for the consolidated
stack (and the boundary between `vendor/ecma-rs/` vs `src/`; see also:
[`instructions/ecma_rs_ownership.md`](../instructions/ecma_rs_ownership.md)).

## Crate / code layout

### `vendor/ecma-rs/webidl` (crate: `webidl`)

Runtime-independent WebIDL implementation:

- WebIDL IR / IDL model and parsing utilities (`webidl::ir`)
- WebIDL conversions (JS ↔ IDL) and helpers (`DOMString`/`USVString`, numeric conversions,
  sequences/records/unions, etc.)
- Overload resolution (the WebIDL overload selection algorithm)
- Runtime boundary traits that conversions/overload resolution are defined against:
  - `webidl::JsRuntime` / `webidl::WebIdlJsRuntime`
  - `webidl::WebIdlHooks` (platform object checks)
  - `webidl::WebIdlLimits` (resource limits)

If you need a new WebIDL spec algorithm, or want to improve correctness/perf of an existing one, it
belongs here.

### `vendor/ecma-rs/webidl-vm-js` (crate: `webidl-vm-js`)

`vm-js` adapter for `webidl`:

- Implements the `webidl` runtime traits on top of `vm-js` values/objects/symbols.
- Owns the rooting/lifetime strategy needed to safely run conversions while `vm-js` GC can happen
  during allocations (e.g. `webidl_vm_js::VmJsWebIdlCx`).
- Contains **generic** glue used by generated bindings when targeting a real `vm-js` realm (host
  dispatch helpers, common conversion helpers, binding installer/runtime primitives).

If you need a new feature in the `webidl` ↔ `vm-js` adapter (rooting, iterator helpers, host dispatch
plumbing), it belongs here.

### Legacy heap-only runtime (compat): `vendor/ecma-rs/webidl-runtime`

The legacy heap-only runtime adapter is used by early scaffolding and some unit tests. It cannot
execute author scripts and should not be used for new bindings work.

- Cargo package name: `webidl-js-runtime`
- Rust crate name: `webidl_js_runtime`

This layer exists for migration/testing where older heap-only bindings/runtime code is still
referenced. Prefer the realm-based `webidl-vm-js` path for new bindings work.

FastRender continues to expose this layer under `fastrender::js::webidl::legacy` while migration is
in progress.

### `src/js/webidl/*` (FastRender-specific)

FastRender’s in-tree bindings and integration layer:

- Generated binding installers (from `xtask webidl-bindings`):
  - `src/js/webidl/bindings/generated/`
- Hand-written FastRender glue around generated code (installation into realms, host dispatch wiring,
  and FastRender-specific helpers).
- Re-exports so generated code can depend on a stable path (`fastrender::js::webidl`).

## Boundary rules (where new code goes)

- Need a new **WebIDL spec algorithm** (conversion, overload rule, IR/parsing feature)?
  - Put it in `vendor/ecma-rs/webidl`.
- Need a new **`vm-js` adapter capability** (rooting, iterator helpers, host dispatch glue)?
  - Put it in `vendor/ecma-rs/webidl-vm-js`.
- Need a new **FastRender binding or embedding integration** (DOM/Web API behavior, realm wiring)?
  - Put it in `src/js/**` (and the concrete behavior typically lives under `src/web/**`).

Do **not** create new WebIDL infrastructure crates outside `vendor/ecma-rs/`. If something is generic
JS/WebIDL infrastructure, it belongs in the vendored ecma-rs workspace.

## How to regenerate generated outputs

### Regenerate the committed WebIDL snapshot (`xtask webidl`)

This refreshes `src/webidl/generated/mod.rs` from the vendored spec sources.

```bash
# One-time (or when specs change): ensure the relevant spec submodules exist.
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

This refreshes generated Rust glue from the committed snapshot world.

```bash
# Regenerate generated bindings glue.
bash scripts/cargo_agent.sh xtask webidl-bindings

# CI-style check (do not write; fail if output differs).
bash scripts/cargo_agent.sh xtask webidl-bindings --check
```

## Scoped test commands

Run the tests that correspond to what you changed:

```bash
# FastRender compiles (bindings glue + integration).
bash scripts/cargo_agent.sh check -p fastrender --quiet

# webidl crate tests (vendored ecma-rs workspace).
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p webidl

# webidl-vm-js adapter tests.
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p webidl-vm-js

# Binding generator goldens / snapshot checks.
bash scripts/cargo_agent.sh test -p xtask --test webidl_bindings_snapshots_up_to_date

# FastRender integration tests that exercise WebIDL-driven bindings.
bash scripts/cargo_agent.sh test -p fastrender --test misc_tests -- js_webidl
```

## Related docs

- WebIDL bindings/codegen pipeline: [`docs/webidl_bindings.md`](webidl_bindings.md)
- Consolidation rationale/target layout: [`instructions/webidl_consolidation.md`](../instructions/webidl_consolidation.md)
