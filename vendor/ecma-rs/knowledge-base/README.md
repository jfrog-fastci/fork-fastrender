# knowledge-base

Semantic knowledge base for `ecma-rs` analysis passes.

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
name = "Math.sqrt"
aliases = []
effects = "Pure"
purity = "Pure"
```

## String encoding properties

`effect-js` uses `properties` on API entries to understand string encodings.

Standardized keys:

- `encoding.output`: one of
  - `ascii`
  - `latin1`
  - `utf8`
  - `unknown`
  - `same_as_input`
- `encoding.preserves_input_if` (optional): one of
  - `ascii`
  - `latin1`
  - `utf8`

Interpretation:

- `encoding.output = same_as_input`: the API returns a string with the same encoding as its input.
- When `encoding.preserves_input_if` is present, the encoding is only considered preserved when the
  input encoding matches; otherwise the result is treated as `unknown`.

