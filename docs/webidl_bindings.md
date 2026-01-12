# WebIDL bindings pipeline

FastRender’s long-term plan is to expose DOM + Web APIs to JavaScript via **WebIDL-shaped bindings**.
This repo already has the first piece of that pipeline: a **deterministic, queryable snapshot** of
upstream WHATWG WebIDL that downstream binding/codegen work can build on.

This doc is contributor-facing: it explains how the snapshot is produced, why it is committed, and
how to update it.

For an overview of the consolidated WebIDL crate layout (and where new code belongs), see
[`docs/webidl_stack.md`](webidl_stack.md).

## What’s in the repo today

- **Runtime WebIDL “world” representation**: `src/webidl/mod.rs`
  - This is *metadata only* (names, members, extended attributes, inheritance).
  - It is intentionally **not** a full WebIDL semantic model.
- **Authoritative WebIDL IR + algorithms**: `vendor/ecma-rs/webidl`
  - This is the shared implementation of WebIDL IR (exported as `webidl::ir`) and WebIDL algorithms
    (conversion helpers like `DOMString`, overload resolution, etc.).
  - It also defines the `JsRuntime` trait boundary those helpers are defined against.
  - FastRender re-exports this API surface as `fastrender::js::webidl` so generated bindings can
    depend on a single path and we do not fork/duplicate WebIDL algorithms between repos.
  - FastRender’s canonical `vm-js` embedding adapter lives in `vendor/ecma-rs/webidl-vm-js` as
    `webidl_vm_js::VmJsWebIdlCx`.
- **Bindings runtime (canonical)**: `src/js/webidl/runtime_vmjs.rs`
  - This is FastRender’s in-tree “bindings runtime” layer that installs WebIDL-generated APIs onto a
    **real `vm-js` realm** (`vm_js::{Vm, Heap, Realm, Scope}`) and performs conversions using the
    canonical `webidl` crate.
  - It is re-exported under `fastrender::js::webidl` as:
    - `VmJsWebIdlBindingsCx`
    - `VmJsWebIdlBindingsState`
    - `WebIdlBindingsRuntime`
- **Binding installation / host scaffolding (legacy heap-only runtime)**: `vendor/ecma-rs/webidl-runtime`
  - This provides a heap-only `vm-js` value/object model (`VmJsRuntime`) used by early scaffolding
    code. It cannot execute author scripts and should not be used for new bindings work.
  - Cargo package name: `webidl-js-runtime` (library crate name: `webidl_runtime`, imported as
    `webidl_js_runtime` in FastRender).
  - It remains available under `fastrender::js::webidl::legacy` while migration is in progress.
  - Run it via the vendored ecma-rs workspace wrapper:

    ```bash
    bash vendor/ecma-rs/scripts/cargo_agent.sh test -p webidl-js-runtime
    bash vendor/ecma-rs/scripts/cargo_agent.sh build -p webidl-js-runtime
    ```
  - Note: FastRender’s canonical author-script execution is realm-based (`vm-js` + `WindowRealm`).
    The heap-only runtime exists mainly for migration and targeted unit tests.
- **Committed generated snapshot**: `src/webidl/generated/mod.rs`
  - Contains `pub const WORLD: WebIdlWorld = ...`.
  - Marked `@generated` and must not be edited by hand.
- **Extractor/parser/resolver implementation**: `xtask/src/webidl/*`
  - Extraction: pulls `<pre class=idl>` blocks from spec sources.
  - Parsing: a small, forgiving subset parser.
  - Resolution: merges `partial interface`/`partial dictionary` and applies `includes`, with stable
    ordering rules.
- **Codegen driver**: `xtask/src/webidl_codegen.rs`
  - Wired up as `bash scripts/cargo_agent.sh xtask webidl` (alias for `bash scripts/cargo_agent.sh xtask web-idl-codegen`).

## The pipeline (extract → resolve → generate)

The `bash scripts/cargo_agent.sh xtask webidl` command runs the full pipeline end-to-end:

1. **Load + extract** IDL blocks from vendored sources:
   - Prelude/overrides:
      - `tools/webidl/prelude.idl`
      - `tools/webidl/overrides/*.idl` (lexicographic)
    - Specs:
      - DOM: `specs/whatwg-dom/dom.bs` (Bikeshed source)
      - HTML: `specs/whatwg-html/source` (WHATWG HTML source format)
      - URL: `specs/whatwg-url/url.bs` (Bikeshed source)
      - Fetch: `specs/whatwg-fetch/fetch.bs` (Bikeshed source)
2. **Parse + resolve** into a consolidated world:
    - Merge partial definitions.
    - Apply `includes` statements.
3. **Generate** deterministic Rust data into:
   - `src/webidl/generated/mod.rs`

### Running codegen (update the committed snapshot)

`specs/` are optional git submodules. To regenerate the snapshot you must have the relevant spec
submodules checked out:

```bash
git submodule update --init \
  specs/whatwg-dom \
  specs/whatwg-html \
  specs/whatwg-url \
  specs/whatwg-fetch
```

Then run:

```bash
bash scripts/cargo_agent.sh xtask webidl
```

This overwrites `src/webidl/generated/mod.rs`. Commit the result.

### `--check` mode (don’t write; fail if out of date)

To verify the generated snapshot is up to date without writing anything:

```bash
bash scripts/cargo_agent.sh xtask webidl --check
```

Notes:

- This is useful for local “did I forget to regenerate?” checks.
- CI does **not** initialize `specs/` submodules by default (they’re large and only needed for
  contributors doing spec-driven work), so `--check` currently requires a full local checkout.

### Inputs/outputs can be overridden

The command supports explicit paths (mostly useful for debugging):

```bash
bash scripts/cargo_agent.sh xtask webidl \
  --dom-source specs/whatwg-dom/dom.bs \
  --html-source specs/whatwg-html/source \
  --url-source specs/whatwg-url/url.bs \
  --fetch-source specs/whatwg-fetch/fetch.bs \
  --out src/webidl/generated/mod.rs
```

## Determinism (why we commit the snapshot)

We commit `src/webidl/generated/mod.rs` because:

- `specs/` are optional submodules (and CI doesn’t init them),
- WebIDL extraction/parsing is *tooling*, not runtime behavior,
- keeping generated output committed makes builds/tests independent of network + submodule state.

Determinism rules are part of the contract:

- Definitions are stored in deterministic maps (`BTreeMap`), so iteration order is stable.
- The resolution pass has explicit ordering rules for members (base definition first, then partials
  appended in appearance order, then mixin members appended in `includes` order). See
  `xtask/src/webidl/resolve.rs`.

If your diff shows large reorderings, treat it as a red flag—either the upstream spec changed
significantly or we accidentally introduced nondeterminism.

## Adding new IDL sources / interfaces

The current generator snapshots IDL from:

- DOM (`specs/whatwg-dom/dom.bs`)
- HTML (`specs/whatwg-html/source`)
- URL (`specs/whatwg-url/url.bs`)
- Fetch (`specs/whatwg-fetch/fetch.bs`)

To pull in additional WebIDL sources (WebSockets/etc.), you will need to:

1. Add/init the appropriate spec submodule under `specs/` (see `specs/README.md`).
2. Extend `xtask/src/webidl_codegen.rs` to include the source in the call to
   `xtask::webidl::load::load_combined_webidl` (and update the header comment in
   `xtask/src/webidl/generate.rs`).
3. Re-run `bash scripts/cargo_agent.sh xtask webidl` and commit the updated `src/webidl/generated/mod.rs`.

Downstream binding generation (Rust glue / JS-visible APIs) should treat the snapshot as the source
of truth for *shape* (members, overload sets, extended attributes) and implement behavior in Rust.

## WebIDL-driven JS bindings codegen (`bash scripts/cargo_agent.sh xtask webidl-bindings`)

The committed WebIDL snapshot (`src/webidl/generated/mod.rs`) is also used as the *shape source* for
generating Rust glue that exposes DOM/web APIs to a JavaScript runtime.

FastRender includes a second deterministic codegen step:

```bash
# Regenerate the committed Rust glue.
bash scripts/cargo_agent.sh xtask webidl-bindings

# CI-style check mode (do not write; fail if output differs).
bash scripts/cargo_agent.sh xtask webidl-bindings --check
```

Notes:

- Unlike `bash scripts/cargo_agent.sh xtask webidl`, **`webidl-bindings` does not require the vendored `specs/` submodules**
  to be present. It consumes the committed snapshot world (`src/webidl/generated/mod.rs`) instead.
- The generator is intentionally incremental and only supports a small subset of WebIDL features
  needed by FastRender today. Expand it as new APIs are wired up.

### Outputs

`bash scripts/cargo_agent.sh xtask webidl-bindings` emits committed Rust glue from the snapshot
world:

- **vm-js realm bindings** (default backend): `src/js/webidl/bindings/generated/mod.rs`
  - Generated installers install constructors/prototypes directly into a `vm_js::Realm` and dispatch
    into the embedder via `webidl_vm_js::WebIdlBindingsHost` (retrieved from
    `webidl_vm_js::host_from_hooks`, backed by a `webidl_vm_js::WebIdlBindingsHostSlot` exposed
    through `VmHostHooks::as_any_mut`).
  - Controlled by an explicit allowlist: `tools/webidl/window_bindings_allowlist.toml` (typo-guarded
    against the committed snapshot world).
- **Legacy heap-only bindings** (`--backend legacy --out src/js/webidl/bindings/generated_legacy.rs`):
  `src/js/webidl/bindings/generated_legacy.rs`
  - Backed by `fastrender::js::webidl::legacy` (vendored at `vendor/ecma-rs/webidl-runtime`, Cargo
    package `webidl-js-runtime`).
  - Kept temporarily for migration and for unit tests that still exercise the older bindings/runtime
    surface.
  - Regenerate with:

    ```bash
    bash scripts/cargo_agent.sh xtask webidl-bindings \
      --backend legacy \
      --out src/js/webidl/bindings/generated_legacy.rs
    ```

- **Legacy `VmJsRuntime` DOM scaffold** (`--backend legacy`, default `--dom-out`): `src/js/legacy/dom_generated.rs`
  - Controlled by `tools/webidl/bindings_allowlist.toml`.

DOM bindings are currently implemented directly against `vm-js` realms in `src/js/legacy/vm_dom.rs`
and are installed with `fastrender::js::install_dom_bindings(vm, heap, realm, ...)`.

### Canonical binding/runtime helpers (avoid duplication)

Generated bindings should stay as small and drift-free as possible: any non-trivial, spec-shaped
algorithm should live in a shared runtime/helper layer (and be called from generated glue), rather
than being duplicated in every generated module.

In practice:

- **Spec algorithms** (WebIDL conversions and overload resolution) live in `vendor/ecma-rs/webidl`
  and are re-exported as `fastrender::js::webidl`.
- **`vm-js` realm bindings runtime helpers** (property definition presets, small numeric helpers used
  by generated glue) live in `vendor/ecma-rs/webidl-vm-js`:
  - `webidl_vm_js::bindings_runtime` contains installer/runtime helpers (`BindingsRuntime`,
    `DataPropertyAttributes`, `to_int32_f64` / `to_uint32_f64`, etc.).
  - `webidl_vm_js::conversions` contains shared `vm-js`-specific conversion helpers that generated
    bindings should call (sequence/record/enum conversion and union discrimination predicates),
    rather than emitting the conversion logic inline.
- **Host return-value conversion** from `BindingValue` back to a JS value is provided once as
  `fastrender::js::bindings::binding_value_to_js` (rather than emitting a copy per generated
  module).
- **Iterator acquisition** for list conversions (`sequence<T>` / `FrozenArray<T>`) should call a
  shared helper (rather than emitting the iterator loop inline):
  - `vm-js` realm backend: `webidl_vm_js::conversions::to_iterable_list` (wraps `vm_js::iterator`,
    including the Array fast-path).
  - Legacy backend: `WebIdlBindingsRuntime::get_iterator` / `iterator_step_value` (real `vm-js`
    realms delegate to `vm_js::iterator`).

Notes:

- `src/js/webidl/*` should stay as **thin re-exports/adapters**. In particular,
  `src/js/webidl/conversions.rs` exists to support the legacy heap-only backend (`webidl_js_runtime`)
  and should not be used by the `vm-js` realm backend.
- If you find yourself pasting a WebIDL algorithm (record/sequence conversion loops, union
  discrimination, etc.) into the generator output, it probably belongs in `vendor/ecma-rs/webidl` or
  `vendor/ecma-rs/webidl-vm-js` instead.

### Troubleshooting

- If `src/js/webidl/bindings/generated_legacy.rs` fails to compile (common symptoms are Rust
  `E0425` missing wrapper functions or `E0428` duplicate wrapper functions), regenerate the legacy
  bindings from the committed snapshot world:

  ```bash
  bash scripts/cargo_agent.sh xtask webidl-bindings \
    --backend legacy \
    --out src/js/webidl/bindings/generated_legacy.rs
  ```

- Then verify the build with:

  ```bash
  bash scripts/cargo_agent.sh check -p fastrender --quiet
  ```

## Debugging unsupported/odd IDL

The WebIDL support in `xtask/src/webidl` is intentionally a **small subset** aimed at WHATWG
sources:

- Unknown top-level definitions are preserved as `ParsedDefinition::Other { raw }` (and ignored by
  the resolver today).
- Interface members are kept as raw strings in the snapshot so downstream codegen can decide what
  to support first.

When extraction/parsing breaks, add a focused regression test under `xtask/tests/webidl_*.rs`
instead of patching around it in downstream codegen.
