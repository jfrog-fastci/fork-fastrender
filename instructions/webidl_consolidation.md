# WebIDL Stack Consolidation

FastRender’s WebIDL stack has been consolidated into the vendored ecma-rs workspace
(`vendor/ecma-rs/`).

**Status: COMPLETE** (generic WebIDL infrastructure lives in `vendor/ecma-rs/`; the legacy
heap-only runtime remains available for migration/testing.)

This document is a contributor-facing “where does this live?” reference so new work does not
re-introduce a parallel WebIDL stack under `crates/`.

See also: [`docs/webidl_stack.md`](../docs/webidl_stack.md).

---

## Repository shape

Current shape:

```
vendor/ecma-rs/
├── webidl/               ← WebIDL IR + parsing + spec algorithms (exports `webidl::ir`)
├── webidl-vm-js/         ← vm-js adapter + realm bindings helpers
└── webidl-runtime/       ← legacy heap-only runtime adapter (compat/migration; Cargo package `webidl-runtime`)

src/js/webidl/            ← FastRender-specific bindings integration (re-exports, host dispatch,
                            realm installation/runtime glue)

crates/
└── js-wpt-dom-runner/    ← FastRender-specific tooling (offline WPT runner)
```

Note: historical WebIDL infrastructure that previously lived under `crates/` has been migrated into
`vendor/ecma-rs/webidl` and should not be re-introduced.

Key point: **generic JS/WebIDL infrastructure belongs in `vendor/ecma-rs/`**. FastRender’s `src/`
contains the embedding integration and concrete DOM/Web API behavior.

---

## Where to add things (rules of thumb)

### WebIDL IR / parsing / algorithms

Put it in `vendor/ecma-rs/webidl/`:

- WebIDL parsing, IR data structures, and any helper algorithms shared by codegen.
- Conversion algorithms (e.g. `DOMString`, `sequence<T>`, overload resolution).
- The canonical “runtime boundary” traits used by conversions.

In code, the IR lives under `webidl::ir`.

### vm-js adapter and bindings helpers

Put it in `vendor/ecma-rs/webidl-vm-js/`:

- `vm-js`-backed implementations of the `webidl` runtime traits (e.g. `VmJsWebIdlCx`).
- Helpers used by generated bindings installers (property presets, numeric helpers, shared conversion
  helpers for sequences/records/enums/unions, etc.).

### Legacy heap-only runtime adapter

If you need the legacy heap-only runtime (used by early scaffolding and some unit tests), the
canonical implementation lives in `vendor/ecma-rs/webidl-runtime/`:

- Cargo package name: `webidl-runtime` (Rust crate name: `webidl_runtime`).
- FastRender exposes it via `fastrender::js::webidl::legacy` while migration is in progress.

### FastRender-specific DOM bindings integration

Put it in `src/js/webidl/`:

- glue/re-exports so generated bindings can depend on a stable in-crate path (`fastrender::js::webidl`)
- host dispatch integration and embedding-specific state wiring
- realm installation/runtime entry points for FastRender’s DOM and event loop

Concrete DOM/Web API behavior should generally live under `src/web/` (and be invoked by bindings).

---

## Non-negotiables

- Do not create new “mirror” crates under `crates/` for WebIDL/JS infrastructure.
- If an ecma-rs crate is missing something FastRender needs, **modify ecma-rs directly**.
