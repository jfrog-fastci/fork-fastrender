# Import maps (WHATWG HTML mapping + FastRender API)

FastRender’s import map module implements the **HTML Standard import maps algorithms** in a form
usable by:

1. the HTML `<script type="importmap">` pipeline (parse + register), and
2. the module loader / module graph code (resolve module specifiers).

This module is deliberately **pure “specifier → URL mapping” logic**. Integration with module
scripts (fetching module graphs, caching, evaluation order, etc.) is separate, but should **always**
use the APIs described here for resolution and import map state.

---

## What it implements (scope)

The import map module covers:

* **Parsing** a JSON import map string into a fully-normalized `ImportMap`.
* Creating an **`import map parse result`** (`ImportMapParseResult`) that carries either:
  * a parsed import map, or
  * an error that must be “rethrown” later when registering the map.
* Maintaining per-global/per-document **`ImportMapState`**:
  * the current merged `ImportMap`, and
  * the **resolved module set** (used to prevent later import maps from changing already-resolved
    specifiers).
* **Merging** new import maps into existing state with the same “ignore conflicting/impactful rules”
  behavior as the HTML spec.
* **Resolving module specifiers** using the current import map, including:
  * scope matching (most- to least-specific),
  * prefix matching (most- to least-specific),
  * special-URL constraints, and
  * backtracking protection.

Not in scope here:

* Module script fetching / module graph construction.
* The `<script>` scheduling model (`async`/`defer`/parser-blocking) — see
  [`docs/html_script_processing.md`](html_script_processing.md).
* CSP/SRI/CORS integration (although the import map `integrity` map is parsed and merged; how it is
  applied to fetch requests is part of module loader integration).

---

## Spec anchors (local WHATWG HTML copy)

All spec references below are to the local submodule file:

`specs/whatwg-html/source`

Use these `rg -n` commands to jump to the normative algorithms:

### Script processing integration

* `<script type="importmap">` preparation (creates parse result):
  * `rg -n 'creating an import map parse result' specs/whatwg-html/source`
* `<script type="importmap">` execution (registers import map):
  * `rg -n 'Register an import map' specs/whatwg-html/source`

### Parse result + registration

* “Import map parse results”:
  * `rg -n 'Import map parse results' specs/whatwg-html/source`
* `create an import map parse result`:
  * `rg -n 'create an import map parse result' specs/whatwg-html/source`
* `register an import map`:
  * `rg -n 'register an import map' specs/whatwg-html/source`

### Parsing + normalization

* `parse an import map string`:
  * `rg -n 'parse an import map string' specs/whatwg-html/source`
* `sort and normalize a module specifier map`:
  * `rg -n 'sort and normalize a module specifier map' specs/whatwg-html/source`
* `sort and normalize scopes`:
  * `rg -n 'sort and normalize scopes' specs/whatwg-html/source`
* `normalize a module integrity map`:
  * `rg -n 'normalize a module integrity map' specs/whatwg-html/source`
* `normalize a specifier key`:
  * `rg -n 'normalize a specifier key' specs/whatwg-html/source`

### Resolution + merge

* `resolve a module specifier`:
  * `rg -n '<dfn>resolve a module specifier' specs/whatwg-html/source`
* `resolve an imports match` (prefix mapping + backtracking checks live here):
  * `rg -n 'resolve an imports match' specs/whatwg-html/source`
* `resolve a URL-like module specifier`:
  * `rg -n 'resolve a URL-like module specifier' specs/whatwg-html/source`
* `add module to resolved module set`:
  * `rg -n 'add module to resolved module set' specs/whatwg-html/source`
* `merge existing and new import maps`:
  * `rg -n 'merge existing and new import maps' specs/whatwg-html/source`

---

## Data structures

### `ImportMap`

In spec terms, this is the “import map” struct with three items:

* `imports`: a **module specifier map** (`specifier → URL-or-null`)
* `scopes`: `scope_prefix → module specifier map`
* `integrity`: `resolved_url → integrity_metadata_string`

FastRender stores a parsed `ImportMap` in a fully-normalized form:

* Specifier keys are normalized per **“normalize a specifier key”**.
* URL-like keys/values are URL-parsed against the `baseURL` and stored in their **serialized**
  (canonical) form.
* Specifier maps and scope maps are stored in the **descending code-unit order** required by the
  spec, so resolution can iterate in-order and “first match wins”.
* Mapping values can be `null`. A `null` entry means resolution for that specifier key (or prefix)
  is **blocked** and must throw during resolution (see “resolve an imports match”).

### `ImportMapState`

This is the per-global/per-document mutable state needed by the HTML algorithms:

* the current merged `ImportMap` (initially the **empty import map**), and
* the **resolved module set**.

This state should be owned by the “Window/global” embedding layer (not by the parser), since both
HTML `<script>` processing and module loading consult it.

### Resolved module set

HTML defines a `resolved module set` on the `Window` global object, containing **specifier
resolution records**.

The import map module uses this set for one purpose: when merging a new import map, it must **drop
any new rules that would affect already-resolved modules** (and warn), so that a page cannot change
the meaning of past `import`s by inserting a later `<script type="importmap">`.

Each record conceptually stores:

* `serialized_base_url`: the referrer base URL used for scope matching
* `normalized_specifier`: either the original bare specifier, or the serialized URL for URL-like
  specifiers
* `specifier_as_url`: `null` or the parsed URL (spec note: implementations can store a boolean for
  “bare or special URL-like” instead of the full URL)

---

## API overview

The module exposes four “entry point” operations that mirror the HTML Standard.

### 1) `parse(...)`

**Spec mapping:** “parse an import map string”.

**Use when:** you have the raw JSON text and a base URL and want a normalized `ImportMap`.

Expected behavior:

* Throws/returns an error for structural problems the spec treats as fatal (non-object top-level,
  invalid `imports`/`scopes`/`integrity` types, etc.).
* Returns warnings (non-fatal) for things like:
  * unknown top-level keys,
  * non-string addresses,
  * unparseable scope prefixes,
  * invalid address URLs,
  * trailing-slash key/value mismatches.

### 2) `create_parse_result(...)` + `register_parse_result(...)`

**Spec mapping:**

* “create an import map parse result”
* “register an import map”

**Use when:** implementing the HTML `<script type="importmap">` lifecycle.

Flow:

1. `create_parse_result(input, base_url)`:
   * Calls `parse(...)` internally.
   * Captures any thrown error as `error_to_rethrow` instead of immediately surfacing it.
2. Later (when the script element “executes”), call `register_parse_result(state, result)`:
   * If `error_to_rethrow` is present: report the exception (HTML: “report an exception”) and do not
     mutate state.
   * Otherwise: merge `result.import_map` into `state` (see below).

This split matters because HTML stores the parse result in the `<script>` element’s `result` slot
while it is “ready”, then performs registration during the “execute the script element” step.

### 3) `merge(...)`

**Spec mapping:** “merge existing and new import maps” (plus “merge module specifier maps”).

**Use when:** registering a new import map into the existing `ImportMapState`.

Key semantics:

* Conflicts do **not** overwrite:
  * if a specifier key already exists in the old map, the new rule is ignored (warn).
* The resolved module set is consulted to drop any new rules that would affect already-resolved
  modules (warn).
* Scopes are merged per-scope-prefix, using the same conflict rules as top-level imports.
* Integrity entries are merged similarly (old wins; warn on ignored).

The spec warns that the resolved module set can reach **thousands** of entries and encourages an
efficient matching strategy; avoid quadratic “scan everything” implementations.

### 4) `resolve_module_specifier(...)`

**Spec mapping:**

* “resolve a module specifier”
* “resolve an imports match”
* “add module to resolved module set”
* “resolve a URL-like module specifier”

**Use when:** the module loader needs to turn an `import` specifier string into a URL.

Behavior summary:

1. Compute `as_url` (URL-or-null) using “resolve a URL-like module specifier”.
2. Compute `normalized_specifier`:
   * if `as_url` exists: use its serialization,
   * otherwise: use the original specifier (bare specifier).
3. Consult scoped maps in descending specificity order; then fall back to top-level `imports`.
4. If no map matches:
   * if `as_url` exists: return it,
   * otherwise: throw (bare specifier not mapped).
5. On success: record the resolution in the resolved module set.

See below for the two easy-to-get-wrong corner cases: special URL handling and backtracking
protection.

---

## Warnings vs errors (how to think about it)

Import maps intentionally have a “robust parsing” posture: many invalid inputs become warnings,
because treating them as fatal would make future spec extensions impossible and would make typos too
dangerous.

### Errors (fatal; end up as `error_to_rethrow` in parse result)

Common fatal cases:

* Top-level JSON value is not an object.
* `"imports"` exists but is not an object.
* `"scopes"` exists but is not an object.
* A scope’s value is not an object.
* `"integrity"` exists but is not an object.

Resolution-time errors (TypeError-style):

* Bare specifier not mapped by the import map.
* A matching entry’s value is `null` (explicitly blocks resolution).
* Prefix mapping produces an invalid URL, or triggers the backtracking guard.

### Warnings (non-fatal; should be surfaced to console/logging)

Examples:

* Unknown top-level keys in the JSON.
* Empty specifier keys, invalid address URLs, or mismatched trailing slashes.
* Unparseable scope prefixes.
* Merge conflicts / ignored rules when merging import maps.

The key rule: **warnings do not prevent registration**, but they can lead to `null` map entries, which
can later cause resolution failures.

---

## Special URL handling + backtracking protection

These are the two main “security/compat” pitfalls in import map resolution.

### “Special” URL handling

The spec allows prefix mappings (keys ending in `/`) only when the referrer specifier is either:

* a bare specifier (`as_url == null`), or
* a URL-like specifier whose parsed URL **is special** (`http:`, `https:`, `file:`, `ws:`, etc.).

This means prefix mappings **must not** apply to non-special URL schemes (e.g. `data:`). Ensure the
implementation uses the URL Standard’s “is special” concept when deciding whether prefix matches are
eligible.

### Backtracking protection for prefix mappings

For a prefix rule like `"pkg/": "/static/pkg/"`, resolving `"pkg/../secret.js"` must not be allowed
to escape the mapped prefix.

HTML enforces this by:

1. resolving the `afterPrefix` segment relative to the mapped URL, then
2. checking that the resulting URL serialization still has the mapped URL serialization as a prefix.

If not, resolution must throw (and **must not** fall back to other candidates).

---

## Integration notes (who should call what)

### HTML parser / `<script type="importmap">`

When the streaming HTML parser finishes an inline import map script (`</script>` boundary):

1. Determine the **script base URL at that point in parsing** (same base URL timing rules as classic
   scripts).
2. Call `create_parse_result(source_text, base_url)`.
3. Store the parse result in the script element’s “result” slot (HTML does this; FastRender’s DOM
   representation can store it alongside other script metadata).
4. When the script element later “executes”, call `register_parse_result(import_map_state, result)`.

Note: import map scripts do not execute JS code; they only mutate import map state.

### Module loader (module scripts integration is separate)

Any subsystem that loads modules must:

* resolve every `import`/`export ... from`/dynamic `import()` specifier by calling
  `resolve_module_specifier(...)` against the `ImportMapState`, and
* rely on `ImportMapState`’s resolved module set to keep later import maps from altering previously
  resolved specifiers.

In other words: **module graph code should never parse/merge import maps on its own**; it should
consume the already-registered `ImportMapState` and use `resolve_module_specifier(...)` as the single
source of truth for specifier → URL resolution.
