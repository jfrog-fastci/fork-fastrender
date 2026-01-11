# knowledge-base

Semantic knowledge base for `ecma-rs` analysis passes, expressed as YAML/TOML.

The default (bundled) knowledge base is built from files under `core/`, `node/`, `web/`, and
`ecosystem/`. Files are bundled into the crate at build time (`build.rs`), and loaded with
`ApiDatabase::load_default()` / `KnowledgeBase::load_default()`.

## Naming conventions

Canonical names should be stable and globally unique.

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

## Entry schema (YAML)

Each YAML file under `core/`, `node/`, `web/`, and `ecosystem/` is typically a list of API entries.
Newer files may use the schema-v1 wrapper (`schema_version: 1`, `symbols: [...]`).

The loader normalizes entries to `effect-model`'s `EffectTemplate` / `PurityTemplate`, and also
stores a non-template summary `effect_summary` (`EffectSet`) so downstream analyses can preserve
base flags even when a template is callback-dependent.

Common fields:

- `name`: stable API name (see naming conventions above)
- `aliases`: list of alternate spellings
- `kind`: `function|constructor|getter|setter|value`
- `semantics`: short semantics identifier (e.g. `Map`, `Filter`, `Reduce`)
- `signature`: optional signature hint
- `since` / `until`: version / availability metadata
- `effects`: either an `EffectTemplate` enum value (`Pure`, `Io`, `Unknown`, ...), or a detail map:
  - `template`: `pure|io|depends_on_callback|unknown`
  - `allocates`, `io`, `network`, `nondeterministic`: booleans
  - `may_throw`: boolean (legacy)
- `throws`: `never|maybe|always|unknown` (overrides `effects.may_throw` when both are present)
- `purity`: either a `PurityTemplate` value (`Pure`, `ReadOnly`, `Allocating`, `Impure`, ...), or
  `{ template: ... }`
- metadata flags: `async`, `idempotent`, `deterministic`, `parallelizable`
- `properties`: arbitrary structured metadata (`serde_json::Value`)

## Bundled file formats

The default (bundled) knowledge base is built from files under:

- `core/`
- `node/`
- `web/`
- `ecosystem/`

Supported formats:

- YAML (`.yaml`, `.yml`)
- TOML (`.toml`)

The loader selects a parser based on the file extension.

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
    aliases: []
    purity:
      template: depends_on_callback
    effects:
      template: depends_on_callback
      may_throw: true
      allocates: true
      io: false
      network: false
      nondeterministic: false
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
- `encoding.preserves_input_if`: `ascii|latin1|utf8`
- `encoding.length_preserving_if`: `ascii|latin1|utf8` (optional)

Interpretation:

- `encoding.output = same_as_input`: the API returns a string with the same encoding as its input.
- `encoding.preserves_input_if`: the encoding is only considered preserved when the input encoding
  matches; otherwise the result is treated as `unknown`.

In YAML, `properties` is typically a map of string keys; encoding keys use a dotted namespace like
`encoding.output`:

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

## Adding a module

1. Pick a directory (`core/`, `node/`, `web/`, `ecosystem/`).
2. Add a `*.yaml` or `*.toml` file.
3. Run:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p knowledge-base --lib
```
