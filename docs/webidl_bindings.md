# WebIDL bindings pipeline

FastRender’s long-term plan is to expose DOM + Web APIs to JavaScript via **WebIDL-shaped bindings**.
This repo already has the first piece of that pipeline: a **deterministic, queryable snapshot** of
upstream WHATWG WebIDL that downstream binding/codegen work can build on.

This doc is contributor-facing: it explains how the snapshot is produced, why it is committed, and
how to update it.

## What’s in the repo today

- **Runtime WebIDL “world” representation**: `src/webidl/mod.rs`
  - This is *metadata only* (names, members, extended attributes, inheritance).
  - It is intentionally **not** a full WebIDL semantic model.
- **Authoritative WebIDL conversion/runtime layer**: `engines/ecma-rs/webidl`
  - This is the shared implementation of WebIDL conversion helpers (e.g. `DOMString`) and the
    `JsRuntime` trait boundary those helpers are defined against.
  - FastRender re-exports this API surface as `fastrender::js::webidl` so generated bindings can
    depend on a single path and we do not fork/duplicate WebIDL algorithms between repos.
  - The `vm-js` embedding adapter lives in `engines/ecma-rs/webidl-vm-js`.
- **Binding installation / host scaffolding (temporary)**: `crates/webidl-js-runtime`
  - This provides a minimal `vm-js`-backed value/object model (`VmJsRuntime`) and a host-facing
    trait (`WebIdlBindingsRuntime`) used by early generated bindings to install functions onto a
    global object.
  - It intentionally reuses `engines/ecma-rs/webidl`’s core types (`InterfaceId`, `WebIdlHooks`,
    `WebIdlLimits`) so FastRender only has one set of WebIDL “core” identifiers/limits/hook traits.
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

## WebIDL-driven JS bindings codegen (`cargo xtask webidl-bindings`)

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

- Unlike `cargo xtask webidl`, **`webidl-bindings` does not require the vendored `specs/` submodules**
  to be present. It consumes the committed snapshot world (`src/webidl/generated/mod.rs`) instead.
- The generator is intentionally incremental and only supports a small subset of WebIDL features
  needed by FastRender today. Expand it as new APIs are wired up.

### Outputs

`cargo xtask webidl-bindings` writes two committed Rust modules:

- **Window-facing bindings glue**: `src/js/bindings/generated/mod.rs`
  - Generated wrappers perform WebIDL-ish argument conversions then dispatch into the host
    integration via `fastrender::js::bindings::WebHostBindings`.
- **DOM scaffold bindings (temporary)**: `src/js/bindings/dom_generated.rs`
  - A minimal `vm-js`-backed DOM surface used for early integration and unit tests.
  - Controlled by an explicit allowlist: `tools/webidl/bindings_allowlist.toml`.

To add new scaffold bindings, edit `tools/webidl/bindings_allowlist.toml` (interfaces, attributes,
operations) and rerun `cargo xtask webidl-bindings`. The generator fails fast if allowlisted members
do not exist in the snapshot world (typo guard).

## Debugging unsupported/odd IDL

The WebIDL support in `xtask/src/webidl` is intentionally a **small subset** aimed at WHATWG
sources:

- Unknown top-level definitions are preserved as `ParsedDefinition::Other { raw }` (and ignored by
  the resolver today).
- Interface members are kept as raw strings in the snapshot so downstream codegen can decide what
  to support first.

When extraction/parsing breaks, add a focused regression test under `xtask/tests/webidl_*.rs`
instead of patching around it in downstream codegen.
