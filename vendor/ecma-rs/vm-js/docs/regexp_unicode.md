# RegExp `/v` Unicode string-property tables

ECMAScript RegExp with the `/v` (“Unicode sets”) flag adds **Unicode properties of strings**
such as `\p{RGI_Emoji}`.

Unlike classic Unicode property escapes (which match a single Unicode code point), **string
properties match whole strings**, e.g. multi-code-point emoji sequences (ZWJ sequences, emoji tag
sequences, skin-tone variations, etc).

To keep matching fast and deterministic, `vm-js` uses a **generated UTF-16 trie** for these
properties (see `vendor/ecma-rs/vm-js/src/regexp_unicode_property_strings.rs`).

## Source of truth

The upstream source is the **test262 generated lists**, derived from
https://github.com/mathiasbynens/unicode-property-escapes-tests (Unicode v17.0.0) and mirrored into
tc39/test262 under:

- `test/built-ins/RegExp/property-escapes/generated/strings/*.js`

FastRender vendors a lightweight snapshot of those inputs under:

- `tools/unicode/regexp_unicode_string_props/*.js`

This keeps CI deterministic without requiring the heavyweight `vendor/ecma-rs/test262-semantic/data`
submodule.

## Regenerating the tables

From the repository root:

```bash
# Regenerate (writes vendor/ecma-rs/vm-js/src/regexp_unicode_property_strings.rs):
timeout -k 10 600 bash scripts/cargo_agent.sh xtask generate-regexp-unicode-property-strings

# Check-only mode (does not modify files; exits non-zero if stale):
timeout -k 10 600 bash scripts/cargo_agent.sh xtask generate-regexp-unicode-property-strings --check
```

CI enforces this via:

```bash
cargo xtask generate-regexp-unicode-property-strings --check
```

For deeper implementation notes (including code point properties + case folding), see
`vendor/ecma-rs/vm-js/docs/regexp_unicode_properties.md`.

