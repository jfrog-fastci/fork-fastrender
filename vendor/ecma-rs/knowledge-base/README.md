# knowledge-base

Semantic knowledge base for `ecma-rs` analysis passes.

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

