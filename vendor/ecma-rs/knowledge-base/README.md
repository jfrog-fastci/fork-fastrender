# knowledge-base

Semantic knowledge base for `ecma-rs` analysis passes, expressed as YAML.

## Naming conventions

### Node.js builtins

Canonical names use `node:<module>.<export>`:

- `node:fs.readFile`
- `node:path.join`
- `node:crypto.randomBytes`

Common alias spellings (e.g. `fs.readFile`, `path.join`) can be listed under `aliases`.

### Web platform globals

Use the global name as it appears in JS:

- Global functions: `fetch`
- Constructors: `URL`, `URLSearchParams`
- Prototype members: `URL.prototype.pathname`

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

## String encoding properties

`effect-js` uses `properties` on API entries to understand string encodings.

Standardized keys:

- `properties.encoding.output`: `ascii|latin1|utf8|unknown|same_as_input`
- `properties.encoding.preserves_input_if`: `ascii|latin1|utf8`
- `properties.encoding.length_preserving_if`: `ascii|latin1|utf8` (optional)

Interpretation:

- `properties.encoding.output = same_as_input`: the API returns a string with the same encoding as
  its input.
- `properties.encoding.preserves_input_if`: the encoding is only considered preserved when the
  input encoding matches; otherwise the result is treated as `unknown`.

In YAML, `properties` is typically a map of string keys; encoding keys use a dotted namespace like
`encoding.output`:

```yaml
- name: String.prototype.toLowerCase
  properties:
    encoding.output: same_as_input
    encoding.preserves_input_if: ascii
    encoding.length_preserving_if: ascii
```
