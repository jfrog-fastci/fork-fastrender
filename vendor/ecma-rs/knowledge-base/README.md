# knowledge-base

Semantic knowledge base for `ecma-rs` analysis passes, expressed as YAML/TOML.

This crate provides:

- A small schema for describing API semantics (effects, purity, metadata).
- A loader (`ApiDatabase::load_default` / `KnowledgeBase::load_default`) that bundles the on-disk
  database at build time (`build.rs`).
- A validator (`ApiDatabase::validate`) to keep the database coherent.

For development tooling that wants to load directly from the repository checkout (without relying
on the embedded bundle), use `ApiDatabase::load_from_dir(root)` where `root` is the
`knowledge-base/` crate directory.

## Bundled file layout

The default (bundled) knowledge base is built from files under:

- `core/`
- `node/`
- `web/`
- `ecosystem/`

The `web/` directory may optionally contain platform-specific subdirectories:

- `web/chrome/`
- `web/firefox/`
- `web/safari/`

When using `ApiDatabase::api_for_target` with `TargetEnv::Web { platform: ... }`, platform-specific
entries are preferred over `web/`'s generic entries.

Supported formats:

- YAML (`.yaml`, `.yml`)
- TOML (`.toml`)

The loader selects a parser based on the file extension.

## Canonical API naming

API names are strings. They should be stable and canonical, because downstream analysis treats them
as identifiers.

General guidelines:

- Use dot-separated paths.
- Prototype methods use `.prototype.` (`String.prototype.split`, not `String.split`).
- Prefer the most common spelling and add alternative spellings under `aliases`.

### Core JS

Use standard JS spellings:

- `Array.prototype.map`
- `Promise.all`
- `JSON.parse`

### Node.js builtins

Canonical names use `node:<module>.<export>`:

- `node:fs.readFile`
- `node:path.join`
- `node:crypto.randomBytes`

Common alias spellings (e.g. `fs.readFile`, `path.join`) can be listed under `aliases`.

When loading the bundled knowledge base, lookups are alias-aware via:

- `ApiDatabase::get(name_or_alias)`
- `ApiDatabase::canonical_name(name_or_alias)`
- `ApiDatabase::id_of(name_or_alias)`

Additionally, Node.js canonical names automatically treat the `node:` prefixless form as an alias
(e.g. `fs.readFile` resolves to `node:fs.readFile`).

### Web platform globals

Use the global name as it appears in JS:

- Global functions: `fetch`
- Constructors: `URL`, `URLSearchParams`
- Prototype members: `URL.prototype.pathname`

### Ecosystem / npm packages

Namespace with the package name:

- `lodash.map`
- `rxjs.Observable`

## Schema versioning & stability

Schema v1 is intended to be forward-compatible:

- Unknown fields are ignored by the parser.
- `properties` is an open-ended map for experimental or niche metadata.

On disk, schema v1 entries may appear in a few equivalent shapes (the loader accepts all of them):

- A YAML list of API entries (`- name: ...`), implicitly schema v1.
- A single YAML document with `schema = 1` and `apis = [...]` (or the aliases
  `schema_version`/`symbols`).
- A shorthand YAML mapping of `ApiName: { ... }` entries, implicitly schema v1.

## Entry schema (v1)

Each API entry supports (at minimum):

- `name`: canonical API name.
- `aliases`: optional list of alternate spellings.
- `kind`: `function|constructor|getter|setter|value` (defaults to `function`).
- `effects`: effect-model `EffectTemplate` (or a structured object, see below).
- `effect_summary`: optional effect-model `EffectSummary` overriding the computed summary (may also be written as an `EffectSet` expression like `IO | MAY_THROW`).
- `purity`: effect-model `PurityTemplate` (or a structured object, see below).
- `semantics`: optional short tag like `Map`, `Fetch`, `JsonParse`.
- `signature`: optional documentation-only signature hint.
- `since` / `until`: optional version constraints used by `ApiDatabase::api_for_target`.
  - Parsed as lenient semver bounds (e.g. `v20.0.0`, `>=18`, `<22`).
  - `since: "baseline"` is treated as “no constraint” (common in web modules).
  - Unparseable values only match under `TargetEnv::Unknown` (conservative fallback).
- `properties`: `map<string, any>` (stored as `serde_json::Value`).

### Effects & purity forms

The loader accepts two representations:

1. **Direct `effect-model` templates**, e.g.

   - `effects: Pure`
   - `effects: Io`
   - `effects: { custom: { flags: IO | NETWORK, throws: Maybe } }`
   - `effects: { depends_on_args: { base: ALLOCATES, args: [0] } }`

2. **Structured objects** (normalized by the loader), e.g.

```yaml
effects:
  template: depends_on_callback
  allocates: true
  io: false
  network: false
  nondeterministic: false
  reads_global: false
  writes_global: false
  may_throw: true
purity:
  template: depends_on_callback
```

Some modules additionally include `effects.base: [...]` as a shorthand list of base effect tokens
(e.g. `[alloc, io, may_throw]`). The loader treats these as defaults for the boolean fields and
ignores unknown tokens (e.g. `async`).

`template: depends_on_callback` is treated as “depends on argument 0” for now.

## Property reads (`kind: property_get`)

Most entries describe callable APIs (`kind: function`, `kind: constructor`, etc).

For getter-like property reads such as `obj.prop`, use:

- `kind: property_get`

This lets analysis passes (e.g. `effect-js`) attach semantics to `ExprKind::Member` nodes, not just
call expressions.

### TOML module example (schema v1)

```toml
schema = 1

[[apis]]
name = "Math.ceil"
aliases = []
effects = "Pure"
purity = "Pure"
```

### YAML module example (schema v1)

```yaml
schema_version: 1
symbols:
  - name: Array.prototype.map
    kind: function
    effects:
      depends_on_args:
        base: ALLOCATES
        args: [0]
    purity:
      depends_on_args:
        base: Allocating
        args: [0]
```

## Legacy YAML layouts

For migration convenience, the loader also accepts older YAML shapes:

1. **Bare list of entries**:

```yaml
- name: node:fs.readFile
  aliases: [fs.readFile]
  effects: Io
  purity: ReadOnly
  async: true
```

2. **Mapping format**:

```yaml
console.log:
  aliases: [console.log]
  purity: { template: impure }
  effects: { template: io, io: true, may_throw: true }
```

## `properties` conventions

`properties` is intentionally flexible so optimization passes can consume new
metadata without requiring frequent schema migrations.

Consumers must be tolerant of missing or malformed values.

### String encoding properties

`effect-js` uses `properties` on API entries to understand string encodings.

`properties` values are JSON (strings/booleans/numbers/arrays/objects). Encoding keys use string
values.

Standardized keys:

- `encoding.output`: `ascii|latin1|utf8|unknown|same_as_input`
- `encoding.preserves_input_if`: `ascii|latin1|utf8` (optional)
- `encoding.length_preserving_if`: `ascii|latin1|utf8` (optional)

Interpretation:

- `encoding.output = same_as_input`: the API returns a string with the same encoding as its input.
- `encoding.preserves_input_if`: the encoding is only considered preserved when the input encoding
  matches; otherwise the result is treated as `unknown`.

Example:

```yaml
- name: String.prototype.toLowerCase
  properties:
    encoding.output: same_as_input
    encoding.preserves_input_if: ascii
    encoding.length_preserving_if: ascii
```

### Arbitrary properties (typed)

`properties` preserves structured values (via `serde_json::Value`) so downstream analyses can use
typed metadata without string parsing. For example:

```yaml
- name: lodash.debounce
  properties:
    timer_based: true
    mutates_argument: 0
    tags: ["timing", "callback"]
    meta:
      category: timing
      notes: "may schedule work"
```

### Array fusion + parallelization properties

These properties are used to drive pipeline fusion (e.g. `map`/`filter`/`reduce`)
and parallelization decisions.

- `properties.fusion.fusable_with: [<canonical api name>, ...]`
- `properties.output.length_relation: same_as_input | le_input | unknown`
- `properties.parallel.requires_callback_pure: bool`
- `properties.parallel.forbid_uses_index: bool`
- `properties.parallel.forbid_uses_array: bool`
- `properties.reduce.associative_if_callback_associative: bool` (optional; placeholder)
## Contributing

- Pick a directory (`core/`, `node/`, `web/`, `ecosystem/`).
- Add a `*.yaml` or `*.toml` file.
- Run:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p knowledge-base --lib
```

- Prefer conservative semantics (when in doubt, mark `unknown` / `may_throw: true`).
- Keep entries small and focused: one API per entry.
- Prefer adding new information under `properties` before changing the Rust schema.
