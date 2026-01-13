# RegExp Unicode property escapes via ICU4X (`icu_properties`) — spike notes

This directory contains a small prototype adapter at `src/regexp_unicode_icu.rs` exploring whether
`vm-js` can implement the **code-point** subset of ECMA-262 RegExp Unicode property escapes
(`\p{..}` / `\P{..}`) using ICU4X data instead of vendoring/parsing UCD text files.

The adapter is **not wired into the RegExp engine yet**; it only covers:

- Binary properties (ECMA-262 `table-binary-unicode-properties`)
- `General_Category` (`gc`)
- `Script` (`sc`)
- `Script_Extensions` (`scx`)

String/sequence properties (notably emoji sequences) are out of scope.

## Unicode version / data provenance

The adapter uses `icu_properties` with the `compiled_data` feature (pulling in
`icu_properties_data`).

The `icu_properties_data` crate documents its data provenance as:

> “This data was generated with CLDR version 48.0.0, ICU version release-78.1rc …”

(see the crate docs in rustdoc output).

ICU 78 corresponds to **Unicode 17.0.0** property data, which is the Unicode version expected by
current test262 RegExp Unicode property escape tests. This makes ICU4X’s compiled data a plausible
drop-in backing store for ECMA-262 code-point properties.

As a lightweight sanity check, `src/regexp_unicode_icu.rs` includes a unit test asserting that
U+16D40 (KIRAT RAI VOWEL SIGN AA, a Unicode 17.0.0 character) has `sc=Kirat_Rai` in the ICU4X
compiled data.

## Property coverage

### Binary properties

Most binary properties in ECMA-262’s `table-binary-unicode-properties` map directly to ICU4X binary
property markers under `icu_properties::props` and are queryable as:

```rust
CodePointSetData::new::<icu_properties::props::Emoji>().contains32(cp)
```

Three properties are handled as special cases because ICU4X does not expose them as standalone
binary properties:

- `Any`: always true (for `0x0000..=0x10FFFF`)
- `ASCII`: `cp <= 0x7F`
- `Assigned`: `General_Category != Unassigned`

In addition, `White_Space` has a **name alias mismatch**: ECMA-262 uses the UCD alias `space`,
whereas ICU4X’s built-in short name for this property is `WSpace`. The spike therefore
special-cases `White_Space` name resolution to accept `space` and reject `WSpace` to stay aligned
with ECMA-262’s accepted spellings.

### `General_Category` (`gc`)

ICU4X provides:

- a map from code point to atomic `GeneralCategory` via `CodePointMapData<GeneralCategory>::get32`
- a parser for `GeneralCategoryGroup` (supports both atomic categories and grouped values like
  `Letter`, `Cased_Letter`, etc.)

Membership is checked by mapping to an atomic category and testing group membership.

### `Script` (`sc`)

ICU4X provides `CodePointMapData<Script>::get32(cp)` for code point script values, and
`PropertyParser<Script>::get_strict()` for parsing script names (e.g. `Latin` and `Latn`).

### `Script_Extensions` (`scx`)

ICU4X provides `icu_properties::script::ScriptWithExtensions::has_script32(cp, Script)` to test
membership in the `Script_Extensions` set for a code point.

## Surrogate code points

ECMAScript RegExp property escapes operate on *code points* and must handle surrogate code points
(`0xD800..=0xDFFF`) when they occur as isolated UTF-16 code units.

The adapter intentionally uses ICU4X’s `*32` APIs (`contains32` / `get32` / `has_script32`) so that
surrogates can be queried without converting to Rust `char`.

Observed behavior matches test262 expectations:

- `gc=Surrogate` matches surrogate code points
- `sc=Unknown` and `scx=Unknown` for surrogates
- `Assigned` includes surrogates because their general category is `Surrogate` (not `Unassigned`)

## Name/value matching policy

This spike currently does **strict**, case-sensitive matching and does **not** implement
ECMA-262-style “loose matching” (underscore/hyphen/space folding).

ICU4X supports both strict and loose parsing (`PropertyParser::get_strict()` / `get_loose()`), but a
fully compliant implementation would still need ECMA-specific tests/constraints around which names
and aliases are accepted.

## Next steps

If this approach is kept:

1. Wire `ResolvedProperty` into the RegExp parser/compiler as the backing set/map provider for
   `\p{..}` / `\P{..}`.
2. Decide on the precise ECMA-262 matching rules (strict vs loose; which aliases are accepted) and
   add targeted test262 coverage.
3. Consider caching the ICU4X `CodePointSetData`/`CodePointMapData` instances if construction cost
   becomes noticeable (the spike currently constructs them on demand).
