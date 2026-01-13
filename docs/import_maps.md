# Import maps (WHATWG HTML mapping + FastRender API)

FastRender’s import map module is the spec-mapped home for **WHATWG HTML import maps**.

In the HTML platform, import maps influence **module specifier → URL** resolution for:

* `<script type="module">` imports
* `import()` (dynamic import)

This module implements the WHATWG HTML import maps algorithms: parsing/normalization, host-side
state/merging, and module specifier resolution.

Key host-facing entry points:

* `ImportMapState` (host-side global state: merged import map + resolved module set)
* `create_import_map_parse_result(...)` (HTML “import map parse result”)
* `register_import_map(...)` / `merge_existing_and_new_import_maps(...)` (HTML “register/merge import maps”)
* `resolve_module_specifier(...)` (HTML “resolve a module specifier” entry point)
* `resolve_imports_match(...)` (HTML “resolve an imports match” helper; returns `Result` and throws `ImportMapError` for blocked cases)

Module script fetching/execution is separate, but module loading must call into the import map APIs
described here.

---

## Status in this repository (reality check)

Code lives in:

* `src/js/import_maps/`
  * `mod.rs`: module entry point + re-exports
  * `merge.rs`: merge + registration implementation (`merge_module_specifier_maps`, `merge_existing_and_new_import_maps`, `register_import_map`)
  * `parse.rs`: parsing + normalization implementation
  * `resolve.rs`: module specifier resolution (`resolve_module_specifier`) + helpers (`resolve_imports_match`, `add_module_to_resolved_module_set`)
  * `types.rs`: data model (`ImportMap`, `ImportMapState`), warnings/errors, resolved module set types
  * `parse_tests.rs`: focused unit tests
  * `merge_tests.rs`: focused unit tests
  * `tests.rs`: merge/register/resolve unit tests

What exists today:

* **Implemented:** parsing + normalization (`parse_import_map_string`) and the normalized data
  structures (`ImportMap`, `ModuleSpecifierMap`, `ScopesMap`, `ModuleIntegrityMap`).
* **Implemented:** import map parse results (`create_import_map_parse_result`).
* **Implemented:** host-side global state (`ImportMapState`) including the **resolved module set**
  record types (`SpecifierResolutionRecord`, `SpecifierAsUrlKind`).
* **Implemented:** registration + merging:
  * `merge_module_specifier_maps`
  * `merge_existing_and_new_import_maps`
  * `register_import_map`
* **Implemented:** full module specifier resolution (`resolve_module_specifier`) and resolved-module-set
  updates (`add_module_to_resolved_module_set`).
* **Implemented:** the core matching helper (`resolve_imports_match`) for "resolve an imports match"
  (returns `Err(ImportMapError::TypeError(...))` for blocked cases like null entries/backtracking).

Import maps are integrated end-to-end for:

* `BrowserTab` (production `vm-js` executor):
  * Inline `<script type="importmap">` scripts are executed by the HTML script scheduler and
    registered via `BrowserTabJsExecutor::execute_import_map_script` (implemented by
    `VmJsBrowserTabExecutor` in `src/api/browser_tab_vm_js_executor.rs`).
  * The active import map state is stored **per document/realm**: `WindowRealm` owns a per-realm
    `realm_module_loader::ModuleLoader` (`src/js/realm_module_loader.rs`), which in turn owns the
    `ImportMapState`.
  * That same `ImportMapState` is consulted for **all** module specifier resolution in the realm:
    * `<script type="module">` static imports,
    * dynamic `import()` from both classic and module scripts, and
    * `import.meta.resolve(...)` (implemented via the realm module loader).
  * See the integration test:
    `tests/js/js_html_integration.rs::p2_dynamic_import_works_from_classic_and_module_scripts_and_honors_import_maps`.
* `vm-js` realm module loader (`src/js/realm_module_loader.rs`):
  * Resolves every `ModuleRequest` by calling
    `import_maps::resolve_module_specifier(&mut state, specifier, base_url)` (where `state` is the
    per-realm `ImportMapState`).
  * Tracks classic-script base URLs (via `ScriptId`) so dynamic `import()` originating from classic
    scripts resolves relative to the script URL and still honors import maps.
* Tooling loader (`src/js/vmjs/module_loader.rs`, `VmJsModuleLoader`):
  * Module evaluation can be run with import maps by passing an `&mut ImportMapState` to
    `evaluate_*_with_import_maps(...)`. This applies to both static and dynamic imports.
  * The loader also mirrors that state into the realm module loader so `import.meta.resolve(...)`
    stays consistent.

> Spec note: External import maps are **not supported** by the HTML Standard today. A
> `<script type="importmap" src="...">` must not be fetched/processed; browsers fire the `<script>`
> element `error` event. FastRender matches this behavior (see `HtmlScriptScheduler`).

Remaining gaps / limitations:

* The tooling loader (`VmJsModuleLoader`) is not an HTML-like execution environment: it does not run
  a full task queue / networking pipeline like `BrowserTab` and only waits for promises by draining
  a bounded number of microtask checkpoints. This is sufficient for many module graphs, but can
  reject modules whose loading/evaluation remains pending beyond microtasks (e.g. top-level await
  waiting on timers or network).

### How to run tests

Import map parsing/normalization + merge/registration/resolution is covered by small, deterministic
unit tests in:

* `src/js/import_maps/parse_tests.rs`
* `src/js/import_maps/merge_tests.rs`
* `src/js/import_maps/tests.rs`

Run them (scoped) with:

```bash
# Runs the fastrender crate's lib test binary, filtered to import_maps tests.
bash scripts/cargo_agent.sh test -p fastrender --lib import_maps
```

---

## Spec anchors (local WHATWG HTML copy)

All spec references below are to:

`specs/whatwg-html/source`

Use these `rg -n` commands to jump to the normative algorithms.

### Parsing + normalization (implemented)

* `parse an import map string`:
  * `rg -n '<dfn>parse an import map string' specs/whatwg-html/source`
* `sort and normalize a module specifier map`:
  * `rg -n 'sorting and normalizing a module specifier map' specs/whatwg-html/source`
* `normalize a specifier key`:
  * `rg -n 'normalize a specifier key' specs/whatwg-html/source`
* `sort and normalize scopes`:
  * `rg -n 'sort and normalize scopes' specs/whatwg-html/source`
* `normalize a module integrity map`:
  * `rg -n 'normalize a module integrity map' specs/whatwg-html/source`

### Script integration (parse result + registration implemented)

* `<script type="importmap">` preparation (creates parse result):
  * `rg -n 'creating an import map parse result' specs/whatwg-html/source`
* `<script type="importmap">` execution (registers import map):
  * `rg -n 'Register an import map' specs/whatwg-html/source`
* “Import map parse results” section:
  * `rg -n 'Import map parse results' specs/whatwg-html/source`
* `create an import map parse result`:
  * `rg -n 'create an import map parse result' specs/whatwg-html/source`
* `register an import map` (**implemented as** `register_import_map`):
  * `rg -n 'register an import map' specs/whatwg-html/source`

### Merging (implemented)

* `merge existing and new import maps` (**implemented as** `merge_existing_and_new_import_maps`):
  * `rg -n '<dfn data-x="merge existing and new import maps"' specs/whatwg-html/source`
* `merge module specifier maps` (**implemented as** `merge_module_specifier_maps`):
  * `rg -n 'merge module specifier maps' specs/whatwg-html/source`

### Resolution (implemented)

* `resolve a module specifier` (**implemented as** `resolve_module_specifier`):
  * `rg -n '<dfn>resolve a module specifier' specs/whatwg-html/source`
* `resolve an imports match` (**implemented as** `resolve_imports_match`):
  * `rg -n 'resolve an imports match' specs/whatwg-html/source`
* `resolve a URL-like module specifier`:
  * `rg -n 'data-x="resolving a URL-like module specifier"' specs/whatwg-html/source`
* `add module to resolved module set` (**implemented as** `add_module_to_resolved_module_set`):
  * `rg -n 'add module to resolved module set' specs/whatwg-html/source`

### Fetch option helpers (implemented)

* `resolve a module integrity metadata` (**implemented as** `resolve_module_integrity_metadata`):
  * `rg -n 'resolve a module integrity metadata' specs/whatwg-html/source`

---

## Data structures

### `ImportMap` (implemented)

Rust type: `fastrender::js::import_maps::ImportMap` (`src/js/import_maps/types.rs`)

This is the normalized import map struct with three items:

* `imports: ModuleSpecifierMap`
* `scopes: ScopesMap`
* `integrity: ModuleIntegrityMap`

### `ModuleSpecifierMap` (implemented)

Rust type: `ModuleSpecifierMap { entries: Vec<(String, Option<url::Url>)> }`

Key points:

* Keys are sorted in **descending UTF-16 code unit order** (see `code_unit_cmp`), matching the spec’s
  “code unit less than” sorting requirement.
* Values are `Option<Url>`:
  * `Some(url)` = a valid address URL
  * `None` = a `null` entry (resolution is blocked for that key/prefix per the spec)

### `ScopesMap` (implemented)

Rust type: `ScopesMap { entries: Vec<(String, ModuleSpecifierMap)> }`

* Scope prefixes are normalized to serialized URLs and sorted in **descending UTF-16 code unit
  order**.
* Type alias: `ScopeMap` is an alias for `ScopesMap` (matching the HTML Standard’s terminology).

### `ModuleIntegrityMap` (implemented)

Rust type: `ModuleIntegrityMap { entries: Vec<(String, String)> }`

* Unlike `imports`/`scopes`, HTML does **not** require sorting this map; FastRender keeps entries in
  insertion order.
* Duplicate keys **within a single import map** are treated as “last one wins” during normalization
  (implemented by overwriting the previous entry in the vector).
* When merging multiple import maps, integrity conflicts are resolved in favor of the existing state
  (old wins), matching the HTML “merge existing and new import maps” behavior.

### `ImportMapWarning` / `ImportMapWarningKind` (implemented)

Rust types:

* `ImportMapWarning { kind: ImportMapWarningKind }`
* `ImportMapWarningKind` enumerates spec “report a warning to the console” cases (unknown top-level
  keys, invalid addresses, etc.)

### `ImportMapError` (implemented)

Rust type: `ImportMapError`:

* `ImportMapError::Json` — input is not valid JSON syntax.
* `ImportMapError::TypeError(String)` — input violates fatal type constraints from the spec (e.g.
  `"imports"` exists but is not a JSON object).
* `ImportMapError::LimitExceeded(String)` — deterministic resource-safety limits were exceeded while
  parsing or merging (see `ImportMapLimits`).

Type alias: `ModuleResolutionError` is currently an alias for `ImportMapError` (it exists so callers
can name resolution-specific errors distinctly, even though the underlying cases are the same today).

### `ImportMapParseResult` (implemented)

Rust type: `ImportMapParseResult`:

* `import_map: Option<ImportMap>`
* `error_to_rethrow: Option<ImportMapError>`
* `warnings: Vec<ImportMapWarning>`

This is the spec-mapped "import map parse result" struct that HTML stores in the script element’s
`result` slot during `<script type="importmap">` preparation.

### `ImportMapState` + resolved module set (implemented)

Rust type: `fastrender::js::import_maps::ImportMapState` (`src/js/import_maps/types.rs`)

Rust types:

* `ImportMapState { import_map, resolved_module_set }`
* `SpecifierResolutionRecord`
* `SpecifierAsUrlKind`

HTML defines mutable per-global state that must be shared between `<script type="importmap">`
registration and module loading:

* a current **merged import map**, and
* a **resolved module set** (specifier resolution records), which prevents later import maps from
  changing the meaning of already-resolved specifiers.

In FastRender, specifiers enter the resolved module set whenever the host resolves through
`resolve_module_specifier(...)` (directly or indirectly), including:

* module graph fetch/evaluation for `<script type="module">`,
* dynamic `import()` (from both module and classic scripts), and
* `import.meta.resolve(...)` (implemented via the same module loader resolution path).

FastRender models this directly with `ImportMapState`:

* `import_map: ImportMap` — the current merged import map.
* `resolved_module_set: ResolvedModuleSet` — records created during
  `resolve_module_specifier(...)` (and consulted during `merge_existing_and_new_import_maps(...)`).

Type alias: `ResolvedModuleSet = Vec<SpecifierResolutionRecord>`.

`SpecifierResolutionRecord` is the stored “specifier resolution record”:

* `serialized_base_url: Option<String>` — the base URL serialization used for resolution. This is
  used to decide which scoped rules would have applied.
* `specifier: String` — the **normalized specifier** (URL serialization if URL-like, otherwise the
  original bare specifier string).
* `as_url_kind: SpecifierAsUrlKind` — whether `asURL` was null / special / non-special. This is used
  by merge filtering to match the spec rule that prefix matches only apply when `asURL` is null OR a
  special URL (non-special URL-like specifiers such as `blob:` should not be affected by new prefix
  rules).

Host integration should keep one `ImportMapState` per **global object** / JS realm (e.g. a `Window`)
and pass it to `register_import_map(...)` and `resolve_module_specifier(...)`.

In FastRender’s production `vm-js` embedding, this is implemented by storing the `ImportMapState`
inside the per-realm module loader (`src/js/realm_module_loader.rs::ModuleLoader`), owned by
`WindowRealm` (`src/js/vmjs/window_realm.rs`). `BrowserTab` creates a fresh `WindowRealm` per
navigation, so import map state is naturally scoped to the current document.

Example: if `"foo"` is resolved while the active import map maps it to `/a.js`, and a later
`<script type="importmap">` tries to map `"foo"` to `/b.js`, the later mapping is ignored during
merge and subsequent resolutions of `"foo"` continue to produce `/a.js`.

---

## API overview

### 1) `parse_import_map_string` (implemented)

Rust API:

* `fastrender::js::import_maps::parse_import_map_string(input: &str, base_url: &url::Url)
  -> Result<(ImportMap, Vec<ImportMapWarning>), ImportMapError>`

Example:

```rust
use fastrender::js::import_maps::parse_import_map_string;
use url::Url;

let base_url = Url::parse("https://example.com/base/page.html").unwrap();
let (import_map, warnings) = parse_import_map_string(
    r#"{ "imports": { "lodash": "/node_modules/lodash-es/lodash.js" } }"#,
    &base_url,
)
.unwrap();

// Non-fatal issues (typos/invalid addresses/etc.) are surfaced here.
for warning in warnings {
    eprintln!("import map warning: {:?}", warning.kind);
}

assert_eq!(import_map.imports.entries.len(), 1);
```

Spec mapping:

* “parse an import map string”
* “sort and normalize a module specifier map”
* “normalize a specifier key”
* “sort and normalize scopes”
* “normalize a module integrity map”

Behavior summary:

* Fatal type errors become `ImportMapError::TypeError(...)` (matching spec “throw a TypeError”).
* Non-fatal issues become `ImportMapWarning`s and typically produce `null` entries in the normalized
  map (i.e. `Option<Url> = None`).
* Sorting is done in **descending UTF-16 code unit order**, so module specifier resolution can
  implement the spec’s “first match wins” iteration.
* JSON object key order and duplicate keys:
  * Input JSON is parsed into an order-preserving representation (matching the spec’s use of
    “ordered maps”).
  * Repeated top-level keys (e.g. multiple `"imports"` properties) are handled as “last one wins”
    (`parse_import_map_string` consults the last occurrence).
* Repeated keys inside `"imports"`/`"scopes"` are resolved after normalization; the last occurrence
  wins.

### 2) `create_import_map_parse_result` (implemented)

HTML stores an **import map parse result** in the `<script>` element’s `result` slot during
preparation, then registers it during execution:

Rust API:

* `fastrender::js::import_maps::create_import_map_parse_result(input: &str, base_url: &url::Url)
  -> ImportMapParseResult`

Spec mapping:

* “create an import map parse result” (**implemented as** `create_import_map_parse_result`)
* “register an import map” (implemented as `register_import_map`, see below)

Typical HTML-shaped flow:

1. At `</script>` boundary for `<script type="importmap">`: call
   `create_import_map_parse_result(...)` and store the `ImportMapParseResult` on the script element.
2. When the script element executes: call `register_import_map(...)` with the stored parse result and
   the per-global `ImportMapState`.

Example:

```rust
use fastrender::js::import_maps::create_import_map_parse_result;
use url::Url;

let base_url = Url::parse("https://example.com/base/page.html").unwrap();
let result = create_import_map_parse_result(r#"{ "imports": { "x": "/x.js" } }"#, &base_url);

for warning in &result.warnings {
    eprintln!("import map warning: {:?}", warning.kind);
}

assert!(result.error_to_rethrow.is_none());
assert!(result.import_map.is_some());
```

Example (parse failure captured as `error_to_rethrow`):

```rust
use fastrender::js::import_maps::create_import_map_parse_result;
use url::Url;

let base_url = Url::parse("https://example.com/base/page.html").unwrap();
let result = create_import_map_parse_result(r#"{ "imports": [] }"#, &base_url);

assert!(result.import_map.is_none());
assert!(result.error_to_rethrow.is_some());
assert!(
    result.warnings.is_empty(),
    "warnings are only produced when parsing/normalization succeeds"
);
```

### 3) `register_import_map` (implemented)

Rust API:

* `fastrender::js::import_maps::register_import_map(state: &mut ImportMapState, result: ImportMapParseResult)
  -> Result<(), ImportMapError>`

Spec mapping: “register an import map”.

Behavior summary:

* If `result.error_to_rethrow` is present, `register_import_map` returns `Err(...)` and **does not**
  mutate state.
* Otherwise, if `result.import_map` is present, it merges it into `state` using
  `merge_existing_and_new_import_maps(...)`.
* Warnings are produced during parse (`create_import_map_parse_result`) and should be surfaced by
  the caller as console warnings; registration does not look at or re-emit them.

Example:

```rust
use fastrender::js::import_maps::{create_import_map_parse_result, register_import_map, ImportMapState};
use url::Url;

let base_url = Url::parse("https://example.com/base/page.html").unwrap();
let mut state = ImportMapState::default();

let result = create_import_map_parse_result(r#"{ "imports": { "x": "/x.js" } }"#, &base_url);
register_import_map(&mut state, result).unwrap();
```

### Supporting helper: `resolve_imports_match` (implemented)

Rust API:

* `fastrender::js::import_maps::resolve_imports_match(normalized_specifier, as_url, specifier_map)
  -> Result<Option<url::Url>, ImportMapError>`

Spec mapping: “resolve an imports match”.

This is a low-level helper used by the full “resolve a module specifier” algorithm.

Most callers should prefer `resolve_module_specifier(...)`, which applies scope fallback rules and
updates the resolved module set. `resolve_imports_match(...)` is exposed primarily for implementing
or testing parts of the resolution algorithm.

It implements:

* exact-key matches and trailing-slash prefix matches (most-specific-first due to map sorting),
* the “special URL” gate for allowing prefix matches, and
* backtracking protection for prefix mappings.

Return values:

* `Ok(None)`: no matching entry was found in the given `ModuleSpecifierMap` (caller should fall back).
* `Ok(Some(url))`: a URL mapping was found (success).
* `Err(ImportMapError::TypeError(...))`: a match was found, but resolution is blocked/invalid (e.g.
  null entry, invalid join/backtracking). In the full spec this should translate into a thrown
  exception and **must not** fall back to other candidates.

Example:

```rust
use fastrender::js::import_maps::{parse_import_map_string, resolve_imports_match};
use url::Url;

let base_url = Url::parse("https://example.com/base/page.html").unwrap();
let (map, _warnings) = parse_import_map_string(r#"{ "imports": { "pkg/": "/static/pkg/" } }"#, &base_url)
    .unwrap();

// `as_url` is the spec’s "specifier as a URL" (computed from the specifier itself).
// For a bare specifier like "pkg/util.js", it is null.
let as_url: Option<Url> = None;
let normalized_specifier = "pkg/util.js";
let resolved = resolve_imports_match(normalized_specifier, as_url.as_ref(), &map.imports);

assert!(
    matches!(resolved, Ok(Some(url)) if url.as_str() == "https://example.com/static/pkg/util.js")
);
```

### 4) `merge_existing_and_new_import_maps` (implemented)

Rust API:

* `fastrender::js::import_maps::merge_existing_and_new_import_maps(state: &mut ImportMapState, new_import_map: &ImportMap)
  -> Result<(), ImportMapError>`

Spec mapping: “merge existing and new import maps”.

This is required for multiple `<script type="importmap">` elements in one document and must consult
the resolved module set to drop rules that would affect already-resolved specifiers.

This uses `merge_module_specifier_maps(...)` (HTML “merge module specifier maps”) to merge component
specifier maps (conflicts are resolved in favor of the existing state).

In practice, most callers should use `register_import_map(...)` instead; this lower-level API is
useful if the host wants to parse import maps separately or merge pre-parsed import maps.

### Supporting helper: `merge_module_specifier_maps` (implemented)

Rust API:

* `fastrender::js::import_maps::merge_module_specifier_maps(new_map: &ModuleSpecifierMap, old_map: &ModuleSpecifierMap)
  -> ModuleSpecifierMap`

Spec mapping: “merge module specifier maps”.

This is a low-level helper used by `merge_existing_and_new_import_maps(...)`. Conflicts are resolved
in favor of the existing map (old wins): if a key exists in `old_map`, the entry from `new_map` is
ignored.

### 5) `resolve_module_specifier` (implemented)

Spec mapping:

* “resolve a module specifier”
* “resolve an imports match”
* “add module to resolved module set”

This is the API module graph code should call to turn a specifier string into a URL, using the
current import map state. It also appends to the resolved module set, so later import maps cannot
retroactively change the meaning of already-resolved specifiers.

Rust API:

* `fastrender::js::import_maps::resolve_module_specifier(state: &mut ImportMapState, specifier: &str, base_url: &url::Url)
  -> Result<url::Url, ImportMapError>`

Important notes for callers:

* This API is **stateful**: on successful resolution, it updates `state.resolved_module_set` so that
  later import maps cannot change already-resolved meanings.
* Errors are surfaced as `ImportMapError::TypeError(...)` (e.g. blocked by a null entry, prefix
  backtracking, or bare specifier not mapped by the import map).

### 6) `add_module_to_resolved_module_set` (implemented)

Rust API:

* `fastrender::js::import_maps::add_module_to_resolved_module_set(
    state: &mut ImportMapState,
    serialized_base_url: String,
    normalized_specifier: String,
    as_url: Option<&url::Url>,
  )`

Spec mapping: “add module to resolved module set”.

Most callers should never need this directly: `resolve_module_specifier(...)` calls it automatically.
It is exposed for hosts that perform module resolution in multiple steps but still need to update
the resolved module set for correct future import map merging.

---

## Warnings vs errors (current behavior)

Import maps are designed to be tolerant: many issues are warnings, not fatal errors.

### Errors (`ImportMapError`)

Fatal cases currently surfaced as `ImportMapError::TypeError(...)` include:

* top-level JSON is not an object
* `"imports"` exists but is not an object
* `"scopes"` exists but is not an object
* a scope’s value is not an object
* `"integrity"` exists but is not an object

### Warnings (`ImportMapWarningKind`)

Non-fatal examples (see `ImportMapWarningKind`):

* `UnknownTopLevelKey { key }`
* `EmptySpecifierKey`
* `AddressNotString { specifier_key }`
* `AddressInvalid { specifier_key, address }`
* `TrailingSlashMismatch { specifier_key, address }`
* `ScopePrefixNotParseable { prefix }`
* `IntegrityKeyFailedToResolve { key }`
* `IntegrityValueNotString { key }`

Many warnings result in a `null` mapping entry in the normalized map (which module specifier
resolution must treat as “blocked”).

### BrowserTab: surfacing import map failures

FastRender follows the HTML Standard’s split between **warnings** (“report a warning to the console”)
and **errors** (exceptions reported to the global object):

* Warnings are recorded as console warnings (`console.warn`-level diagnostics) with an `importmap:`
  prefix.
* Errors are recorded as:
  * a console error (`console.error`-level diagnostics) with an `importmap:` prefix, and
  * an uncaught JS exception diagnostic (for parity with other runtime errors), and
  * a `window` `"error"` event (so `window.addEventListener('error', ...)` and `window.onerror` can
    observe import map failures).

Import maps are not JavaScript, so **inline** `<script type="importmap">` parse/registration
failures **do not** fire `<script>` element `"error"` events in FastRender. This matches the HTML
Standard’s `register an import map` algorithm, which reports the exception to the global object
instead of the script element.

Note: `<script type="importmap" src="...">` is not supported by the HTML Standard today; browsers
fire a `<script>` element `"error"` event for this case, and FastRender matches that behavior.

---

## How FastRender surfaces warnings/errors (BrowserTab + `vm-js`)

When processing inline `<script type="importmap">` via `api::BrowserTab` (using the production
`VmJsBrowserTabExecutor`):

### Warnings

* `ImportMapWarningKind` values are surfaced as **`console.warn(...)`**.
* In diagnostics, these appear under `RenderDiagnostics.console_messages` with
  `level = "warn"`.
* Messages are prefixed with `importmap:`, and are intentionally stable (tests rely on them).

### Errors

* `ImportMapError` values (invalid JSON, fatal type errors, deterministic limit exceedance) are
  surfaced as **`console.error(...)`**.
* In diagnostics, these appear under `RenderDiagnostics.console_messages` with
  `level = "error"`.
* Error strings are formatted as:
  * `SyntaxError: ...` for invalid JSON (`ImportMapError::Json`)
  * `TypeError: ...` for spec-mapped type errors (`ImportMapError::TypeError`)
  * `TypeError: import map limit exceeded: ...` for deterministic limits (`ImportMapError::LimitExceeded`)

#### Current deviation from HTML

FastRender also records import map registration failures into `RenderDiagnostics.js_exceptions` for
parity with other uncaught runtime errors. In browsers, import map failures are reported to the
developer console / global error reporting but are not JavaScript exceptions.

### After an error

FastRender matches HTML’s “don’t break the parser” semantics:

* Import map errors **do not abort HTML parsing** and **do not prevent later scripts from running**.
* If registration fails, the active import map state is **left unchanged** (no partial merge); any
  previously registered import maps remain in effect for subsequent module resolution.

---

## URL handling notes (important for callers)

* Base URLs and parsed URLs in this module use `url::Url` directly (not `js::Url` / `WebUrl`).
* Specifier keys are normalized using “resolve a URL-like module specifier”:
  * if the key starts with `/`, `./`, or `../`, it is URL-parsed against `base_url`
  * otherwise, it is URL-parsed as an absolute URL; if that fails, it stays a bare specifier string
* Address values in `"imports"` and `"scopes"` are resolved using the same “resolve a URL-like module
  specifier” algorithm:
  * valid forms are absolute URLs, or strings starting with `/`, `./`, or `../`
  * bare relative strings like `"node_modules/helper/index.mjs"` are **not** URL-like and will be
    rejected (they normalize to a `null` entry with an `AddressInvalid` warning)

### Resource-safety limits

Import maps are attacker-controlled JSON. FastRender enforces **deterministic size limits** during
import map parsing and merging via `ImportMapLimits`:

* `parse_import_map_string(...)` and `create_import_map_parse_result(...)` use
  `ImportMapLimits::default()`.
* For custom budgets, use:
  * `parse_import_map_string_with_limits(...)`
  * `create_import_map_parse_result_with_limits(...)`
  * `register_import_map_with_limits(...)` (prevents unbounded growth across many registered maps)

The default limits cap:

* input size (`max_bytes`) **before** JSON parsing
* entry counts for `imports`, `scopes`, per-scope maps, and `integrity`
* total entry count across the merged state (`max_total_entries`)
* per-string sizes (`max_key_bytes`, `max_value_bytes`)

Note: URL parsing still uses `url::Url` directly (unbounded); limits exist to keep import map inputs
and the merged in-memory state deterministic and bounded.

### Trailing slash normalization edge case (implicit `/` from URL serialization)

Import maps treat any **normalized** specifier key ending in `/` as a prefix match. Prefix matches
require that the mapped address URL’s serialization also ends in `/`.

Be careful: URL serialization can add an implicit trailing slash. For example, the URL string
`"https://example.com"` serializes as `"https://example.com/"`.

In the HTML spec’s “sort and normalize a module specifier map”, the trailing-slash mismatch check is
phrased in terms of the *original* JSON `specifierKey` (the raw key string). However, FastRender
enforces the invariant using the *normalized* key string (`normalizedSpecifierKey`) so URL
canonicalization cannot accidentally turn a non-prefix key into a prefix key.

This means a mapping like:

```json
{ "imports": { "https://example.com": "https://cdn.example.com/file.js" } }
```

normalizes the key to `"https://example.com/"` (a prefix key), generates a `TrailingSlashMismatch`
warning, and stores a `null` entry (`None`) for that normalized key.

When resolving, `resolve_imports_match(...)` will treat any match against that key as blocked
(returns `Err(ImportMapError::TypeError(...))`), and should never hit its prefix-invariant debug
assertion for maps produced by `parse_import_map_string(...)`.

---

## Special URL handling + backtracking protection

These matter when implementing “resolve an imports match”, and are enforced by
`resolve_imports_match(...)` today:

* Prefix mappings (keys ending in `/`) only apply when the specifier is bare (`as_url == None`) or
  when `as_url` has a **special** scheme (`http`, `https`, `file`, `ftp`, `ws`, `wss`).
* Backtracking protection: after resolving `afterPrefix` relative to the mapped URL, the resulting
  URL must still have the mapped base URL serialization as a prefix.

Note: `resolve_imports_match` returns `ImportMapError::TypeError(...)` for blocked cases. Error
strings are not yet guaranteed to be spec-accurate.

For host-facing resolution, use `resolve_module_specifier(...)`, which applies full scope/imports
fallback and updates the resolved module set.

#### Computing `normalized_specifier` / `as_url` (caller responsibility)

`resolve_imports_match` expects the caller to compute the same inputs as the HTML “resolve a module
specifier” algorithm:

* `as_url`: the result of “resolve a URL-like module specifier” (URL-or-null)
* `normalized_specifier`:
  * if `as_url` is non-null: its serialization
  * otherwise: the original (bare) specifier string

Example (equivalent to the HTML algorithm’s normalization step):

```rust
use url::Url;

fn compute_as_url_and_normalized(specifier: &str, base_url: &Url) -> (Option<Url>, String) {
    let as_url = if specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../") {
        base_url.join(specifier).ok()
    } else {
        Url::parse(specifier).ok()
    };
    let normalized = as_url
        .as_ref()
        .map(|u| u.to_string())
        .unwrap_or_else(|| specifier.to_string());
    (as_url, normalized)
}
```

---

## Integration notes (who should call what)

### HTML parser / `<script type="importmap">`

When the streaming HTML parser finishes parsing an inline import map script (`</script>` boundary),
it should:

1. Determine the base URL **at that point in parsing** (see `BaseUrlTracker` in
   `docs/html_script_processing.md`).
2. Call `create_import_map_parse_result(source_text, base_url)` and surface `result.warnings` as
   console warnings.
3. Store the `ImportMapParseResult` in a script-element result slot (HTML does this; FastRender will
   need an equivalent representation for import map scripts).
4. During “execute the script element”, register/merge into global import map state by calling
   `register_import_map(&mut state, result)` and reporting any returned error.

### Module loader (module scripts integration is separate)

Module script graph loading is separate from import maps, but must:

* use the global import map state, and
* resolve every module specifier through `resolve_module_specifier(&mut state, specifier, base_url)`

In other words: module graph code should not “roll its own” import map parsing/normalization.

FastRender previously had a host-side module *bundler* (`ModuleGraphLoader`) for tooling, but it was
removed once `vm-js` gained real ECMAScript module linking/evaluation.

Today, tooling module execution uses `VmJsModuleLoader` (`src/js/vmjs/module_loader.rs`). When an
active import map is available, callers should use its `*_with_import_maps(...)` APIs so module
specifier resolution goes through `resolve_module_specifier(&mut state, ...)`. When no import map is
provided, `VmJsModuleLoader` only supports relative/absolute URL specifiers and rejects bare
specifiers.

Ensure *every* module specifier is resolved via
`resolve_module_specifier(&mut state, specifier, base_url)` so that the import map rules (including
resolved-module-set updates) are applied consistently.
