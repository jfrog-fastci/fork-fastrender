# Import maps (WHATWG HTML mapping + FastRender API)

FastRender’s import map module is the spec-mapped home for **WHATWG HTML import maps**.

In the HTML platform, import maps influence **module specifier → URL** resolution for:

* `<script type="module">` imports
* `import()` (dynamic import)

This module is deliberately scoped to **import map parsing + normalization**, plus the spec-mapped
utilities needed to wire import maps into HTML/script and module loading:

* `create_import_map_parse_result(...)` (HTML “import map parse result”)
* `resolve_imports_match(...)` (HTML “resolve an imports match” helper)

Module script fetching/execution is separate, but module loading must call into the import map APIs
described here.

---

## Status in this repository (reality check)

Code lives in:

* `src/js/import_maps/`
  * `mod.rs`: module entry point + re-exports
  * `parse.rs`: parsing + normalization implementation
  * `resolve.rs`: `resolve an imports match` helper (prefix/exact matching)
  * `types.rs`: `ImportMap` data model, warnings/errors
  * `parse_tests.rs`: focused unit tests

What exists today:

* **Implemented:** parsing + normalization (`parse_import_map_string`) and the normalized data
  structures (`ImportMap`, `ModuleSpecifierMap`, `ScopesMap`, `ModuleIntegrityMap`).
* **Implemented:** import map parse results (`create_import_map_parse_result`).
* **Implemented:** the core matching helper (`resolve_imports_match`) for "resolve an imports match"
  (prefix/exact matching only; not full module specifier resolution).
* **Not implemented yet:** registration (`register an import map`), merging (`merge existing and new
  import maps`), and full module specifier resolution (`resolve a module specifier`).

Those “not implemented yet” items are still documented below, because they are the intended
integration surface for `<script type="importmap">` and module loading.

### How to run tests

Import map parsing/normalization is covered by small, deterministic unit tests in
`src/js/import_maps/parse_tests.rs`.

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
  * `rg -n 'parse an import map string' specs/whatwg-html/source`
* `sort and normalize a module specifier map`:
  * `rg -n 'sort and normalize a module specifier map' specs/whatwg-html/source`
* `normalize a specifier key`:
  * `rg -n 'normalize a specifier key' specs/whatwg-html/source`
* `sort and normalize scopes`:
  * `rg -n 'sort and normalize scopes' specs/whatwg-html/source`
* `normalize a module integrity map`:
  * `rg -n 'normalize a module integrity map' specs/whatwg-html/source`

### Script integration (parse result implemented; registration not implemented yet)

* `<script type="importmap">` preparation (creates parse result):
  * `rg -n 'creating an import map parse result' specs/whatwg-html/source`
* `<script type="importmap">` execution (registers import map):
  * `rg -n 'Register an import map' specs/whatwg-html/source`
* “Import map parse results” section:
  * `rg -n 'Import map parse results' specs/whatwg-html/source`
* `create an import map parse result`:
  * `rg -n 'create an import map parse result' specs/whatwg-html/source`
* `register an import map`:
  * `rg -n 'register an import map' specs/whatwg-html/source`

### Merging (not implemented yet)

* `merge existing and new import maps`:
  * `rg -n 'merge existing and new import maps' specs/whatwg-html/source`
* `merge module specifier maps`:
  * `rg -n 'merge module specifier maps' specs/whatwg-html/source`

### Resolution (imports-match helper implemented; full resolution not implemented yet)

* `resolve a module specifier`:
  * `rg -n '<dfn>resolve a module specifier' specs/whatwg-html/source`
* `resolve an imports match`:
  * `rg -n 'resolve an imports match' specs/whatwg-html/source`
* `resolve a URL-like module specifier`:
  * `rg -n 'resolve a URL-like module specifier' specs/whatwg-html/source`
* `add module to resolved module set`:
  * `rg -n 'add module to resolved module set' specs/whatwg-html/source`

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

### `ModuleIntegrityMap` (implemented)

Rust type: `ModuleIntegrityMap { entries: Vec<(String, String)> }`

* Unlike `imports`/`scopes`, HTML does **not** require sorting this map; FastRender keeps entries in
  insertion order.
* Duplicate keys are treated as “last one wins” (implemented by overwriting the previous entry in
  the vector).

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

### `ImportMapParseResult` (implemented)

Rust type: `ImportMapParseResult`:

* `import_map: Option<ImportMap>`
* `error_to_rethrow: Option<ImportMapError>`
* `warnings: Vec<ImportMapWarning>`

This is the spec-mapped "import map parse result" struct that HTML stores in the script element’s
`result` slot during `<script type="importmap">` preparation.

### `ImportMapState` + resolved module set (spec concept; not implemented yet)

For merging and resolution, HTML defines mutable per-global state:

* a current merged import map (on the `Window` global object), and
* a **resolved module set** (specifier resolution records), which prevents later import maps from
  changing the meaning of already-resolved specifiers.

FastRender does not yet have a concrete `ImportMapState` type in `src/js/import_maps/`, but future
work should introduce it there (or in a closely-related module) so both the HTML `<script>` pipeline
and the module loader share the same state and algorithms.

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
* Sorting is done in **descending UTF-16 code unit order**, so resolution can be implemented with
  “first match wins” iteration later.
* JSON object key order and duplicate keys:
  * Input JSON is parsed into an order-preserving representation (matching the spec’s use of
    “ordered maps”).
  * Repeated top-level keys (e.g. multiple `"imports"` properties) are handled as “last one wins”
    (`parse_import_map_string` consults the last occurrence).
  * Repeated keys inside `"imports"`/`"scopes"` are resolved after normalization; the last occurrence
    wins.

### 2) Create/register parse result (partially implemented)

HTML stores an **import map parse result** in the `<script>` element’s `result` slot during
preparation, then registers it during execution:

Rust API:

* `fastrender::js::import_maps::create_import_map_parse_result(input: &str, base_url: &url::Url)
  -> ImportMapParseResult`

Spec mapping:

* “create an import map parse result” (**implemented as** `create_import_map_parse_result`)
* “register an import map”

FastRender does not yet implement registration/merge, but the expected flow is:

1. At `</script>` boundary for `<script type="importmap">`:
   * run parsing (by calling `create_import_map_parse_result(...)`) which captures any thrown error
     into `error_to_rethrow` instead of failing immediately.
2. When the script element executes (HTML “execute the script element”):
   * if `error_to_rethrow` exists, report it and do not mutate import map state
   * otherwise, merge the parsed import map into the global import map state

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

### Supporting helper: `resolve_imports_match` (implemented)

Rust API:

* `fastrender::js::import_maps::resolve_imports_match(normalized_specifier, as_url, specifier_map)
  -> Option<Option<url::Url>>`

Spec mapping: “resolve an imports match”.

This is a low-level helper used by the full “resolve a module specifier” algorithm. It implements:

* exact-key matches and trailing-slash prefix matches (most-specific-first due to map sorting),
* the “special URL” gate for allowing prefix matches, and
* backtracking protection for prefix mappings.

Return values:

* `None`: no matching entry was found in the given `ModuleSpecifierMap` (caller should fall back).
* `Some(Some(url))`: a URL mapping was found (success).
* `Some(None)`: a match was found, but resolution is blocked/invalid (e.g. null entry, invalid
  join/backtracking). In the full spec this should translate into a thrown exception and **must not**
  fall back to other candidates.

Example:

```rust
use fastrender::js::import_maps::{parse_import_map_string, resolve_imports_match};
use url::Url;

let base_url = Url::parse("https://example.com/base/page.html").unwrap();
let (map, _warnings) = parse_import_map_string(r#"{ "imports": { "pkg/": "/static/pkg/" } }"#, &base_url)
    .unwrap();

let as_url = Url::parse("https://example.com/app.js").ok();
let normalized_specifier = "pkg/util.js";
let resolved = resolve_imports_match(normalized_specifier, as_url.as_ref(), &map.imports);

assert!(
    matches!(resolved, Some(Some(url)) if url.as_str() == "https://example.com/static/pkg/util.js")
);
```

### 3) `merge` (spec concept; not implemented yet)

Spec mapping: “merge existing and new import maps”.

This is required for multiple `<script type="importmap">` elements in one document and must consult
the resolved module set to drop rules that would affect already-resolved specifiers.

### 4) `resolve_module_specifier` (spec concept; not implemented yet)

Spec mapping:

* “resolve a module specifier”
* “resolve an imports match”
* “add module to resolved module set”

This is the API module graph code should call to turn a specifier string into a URL, using the
current import map state.

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

Many warnings result in a `null` mapping entry in the normalized map (which later resolution must
treat as “blocked”).

---

## URL handling notes (important for callers)

* Base URLs and parsed URLs in this module use `url::Url` directly (not `js::Url` / `WebUrl`).
* Specifier keys are normalized using “resolve a URL-like module specifier”:
  * if the key starts with `/`, `./`, or `../`, it is URL-parsed against `base_url`
  * otherwise, it is URL-parsed as an absolute URL; if that fails, it stays a bare specifier string
* Address values in `"imports"` and `"scopes"` are currently resolved using `base_url.join(address)`
  (i.e. URL parsing with a base URL). This matches the HTML spec’s normalization example (relative
  URLs like `"node_modules/helper/index.mjs"` become absolute under the document base URL).

### Resource-safety notes (current limitations)

Import maps are attacker-controlled input, but this module currently has **no explicit size limits**:

* JSON parsing uses `serde_json` without an input-length cap.
* URL parsing uses `url::Url` (unbounded) instead of FastRender’s bounded URL wrapper
  (`crate::resource::web_url::WebUrl` / `js::Url`).

As import maps become integrated into real page execution, consider enforcing size limits at the
call-site (e.g. script size limits for `<script type="importmap">`) and/or migrating this module to
use bounded URL parsing to match the rest of the JS-facing surface.

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
(`Some(None)`), and should never hit its prefix-invariant debug assertion for maps produced by
`parse_import_map_string(...)`.

---

## Special URL handling + backtracking protection

These matter when implementing “resolve an imports match”, and are enforced by
`resolve_imports_match(...)` today:

* Prefix mappings (keys ending in `/`) only apply when the referrer is bare (`as_url == None`) or
  when `as_url` has a **special** scheme (`http`, `https`, `file`, `ftp`, `ws`, `wss`).
* Backtracking protection: after resolving `afterPrefix` relative to the mapped URL, the resulting
  URL must still have the mapped base URL serialization as a prefix.

Note: `resolve_imports_match` currently reports “blocked/invalid” cases as `Some(None)` (so the caller
can surface a TypeError-style exception message). It does not yet construct spec-accurate error
messages.

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
4. During “execute the script element”, register/merge into global import map state (not yet wired;
   will use `result.error_to_rethrow` and/or `result.import_map`).

### Module loader (module scripts integration is separate)

Module script graph loading is separate from import maps, but must:

* use the global import map state, and
* resolve every module specifier through the import map resolution algorithm (once implemented here)

In other words: module graph code should not “roll its own” import map parsing/normalization.
