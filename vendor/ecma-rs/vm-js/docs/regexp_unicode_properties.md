# RegExp Unicode property escapes: Unicode data + update procedure

This document is about **ECMAScript RegExp Unicode property escapes**:

* `\p{…}` / `\P{…}` (Unicode property escapes; `u` or `v` flag)
* Non-binary properties (`gc`, `sc`, `scx`)
* **`v`-mode only** binary *properties of strings* (Emoji-related)
* Case folding rules (`u`/`v` + `i`, and `v`-mode `MaybeSimpleCaseFolding`)

Unicode property escapes are extremely **data-driven**, and correctness depends on:

* Using the **right Unicode version** (must match test262’s generated tests)
* Supporting **only** the property names/values required by ECMA-262
* Enforcing **strict matching** (no “loose matching”)

## Implementation status (read this first)

`vm-js` is in the process of moving RegExp Unicode property escape handling to the
table-driven implementation described in this document.

At the time of writing:

* The **full Unicode v17.0.0 data** needed for spec-compliant property escapes lives in the
  generated tables (`src/regexp_unicode_tables.rs` and `src/regexp_unicode_property_strings.rs`)
  and the strict resolver (`src/regexp_unicode_resolver.rs`).
* The main RegExp parser/compiler in `src/regexp.rs` still contains a small, hand-rolled property
  escape implementation (currently only `\p{ASCII}` / `\P{ASCII}` and `\p{Script=Han}`), and does
  not yet support property escapes in all `/v` character-class contexts.
  * That hand-rolled implementation also uses ASCII case-insensitive comparisons for its supported
    spellings; this is **not** spec compliant (see “Strict matching rules” below) and should be
    replaced by the table-driven resolver.

This document describes the **intended ECMA-262 surface** and the **Unicode data update
procedure** that the table-driven implementation relies on.

## Spec and test references (bookmark these)

ECMA-262:

* [`UnicodeMatchProperty ( p )`](https://tc39.es/ecma262/#sec-static-semantics-unicodematchproperty-p)
* [`UnicodeMatchPropertyValue ( p, v )`](https://tc39.es/ecma262/#sec-static-semantics-unicodematchpropertyvalue-p-v)
* [`MaybeSimpleCaseFolding ( rer, A )`](https://tc39.es/ecma262/#sec-maybesimplecasefolding)
* [`Canonicalize ( ch )`](https://tc39.es/ecma262/#sec-canonicalize)
* Tables:
  * [Non-binary Unicode properties](https://tc39.es/ecma262/#table-nonbinary-unicode-properties)
  * [Binary Unicode properties](https://tc39.es/ecma262/#table-binary-unicode-properties)
  * [Binary Unicode properties of strings](https://tc39.es/ecma262/#table-binary-unicode-properties-of-strings)

test262 (vendored at `vendor/ecma-rs/test262-semantic/data/`):

* Generated code-point property tests (include the Unicode version in the header), e.g.:
  * `test/built-ins/RegExp/property-escapes/generated/General_Category_-_Surrogate.js` (`Unicode v17.0.0`)
* Generated `v`-mode Unicode-set tests (also include the Unicode version), e.g.:
  * `test/built-ins/RegExp/unicodeSets/generated/string-literal-difference-property-of-strings-escape.js` (`Unicode v17.0.0`)
* Negative tests that encode strict-matching expectations, e.g.:
  * `test/built-ins/RegExp/property-escapes/loose-matching-01.js` (whitespace is a `SyntaxError`)
  * `test/built-ins/RegExp/property-escapes/grammar-extension-In-prefix-Script.js` (`In…` prefix is a `SyntaxError`)
  * `test/built-ins/RegExp/property-escapes/grammar-extension-Is-prefix-Script.js` (`Is…` prefix is a `SyntaxError`)

## Supported properties (what we accept, and what we reject)

ECMA-262 intentionally restricts which Unicode properties are exposed through RegExp escapes to
ensure interoperability across engines. `vm-js` should follow those restrictions exactly.

### Non-binary properties (`name=value`)

Only the non-binary properties listed in ECMA-262 “Non-binary Unicode properties” are supported:

* `General_Category` (alias: `gc`)
* `Script` (alias: `sc`)
* `Script_Extensions` (alias: `scx`)

Notes:

* **No other non-binary properties** are accepted (e.g. `Block` is not supported).
* Only `General_Category` has the special “lone value” shorthand (see below).

### Binary code-point properties (53 properties; `\p{Property}`)

The only supported binary code-point properties are those listed in ECMA-262
“Binary Unicode properties” (53 canonical names + aliases).

Rather than maintain a hand-written list in Rust, the **source of truth is the spec table**
(which mirrors `PropertyAliases.txt` from the Unicode Character Database). Any table generator
should take the spellings *verbatim* from the spec/UCD.

Canonical property names (exact spellings used by the generator):

```
ASCII
ASCII_Hex_Digit
Alphabetic
Any
Assigned
Bidi_Control
Bidi_Mirrored
Case_Ignorable
Cased
Changes_When_Casefolded
Changes_When_Casemapped
Changes_When_Lowercased
Changes_When_NFKC_Casefolded
Changes_When_Titlecased
Changes_When_Uppercased
Dash
Default_Ignorable_Code_Point
Deprecated
Diacritic
Emoji
Emoji_Component
Emoji_Modifier
Emoji_Modifier_Base
Emoji_Presentation
Extended_Pictographic
Extender
Grapheme_Base
Grapheme_Extend
Hex_Digit
IDS_Binary_Operator
IDS_Trinary_Operator
ID_Continue
ID_Start
Ideographic
Join_Control
Logical_Order_Exception
Lowercase
Math
Noncharacter_Code_Point
Pattern_Syntax
Pattern_White_Space
Quotation_Mark
Radical
Regional_Indicator
Sentence_Terminal
Soft_Dotted
Terminal_Punctuation
Unified_Ideograph
Uppercase
Variation_Selector
White_Space
XID_Continue
XID_Start
```

Notes:

* `Any`, `ASCII`, and `Assigned` are **synthesized** by the generator (they are not read from a UCD
  data file directly):
  * `Any` = `0x000000..=0x10FFFF`
  * `ASCII` = `0x000000..=0x00007F`
  * `Assigned` = complement of `General_Category=Unassigned` (which includes surrogate code points,
    since they are `General_Category=Surrogate`).
* Property name aliases are accepted **exactly** as spelled in ECMA-262’s “Binary Unicode
  properties” table (no loose matching).

### Binary properties of strings (`v` flag only)

In Unicode-set mode (`v` flag), RegExp gains *properties of strings* (UTS #51 / Emoji).

Only the binary properties of strings listed in ECMA-262
“Binary Unicode properties of strings” are supported:

* `Basic_Emoji`
* `Emoji_Keycap_Sequence`
* `RGI_Emoji_Modifier_Sequence`
* `RGI_Emoji_Flag_Sequence`
* `RGI_Emoji_Tag_Sequence`
* `RGI_Emoji_ZWJ_Sequence`
* `RGI_Emoji`

These properties must **not** be accepted in `u` mode (they are `v`-mode only).

Note: per ECMA-262, Unicode string properties cannot be negated. That means:

* `\P{RGI_Emoji}` is a `SyntaxError`
* a negated `/v` character class that would require complementing a string-property set is also a
  `SyntaxError`

### “Lone” property names/values (`\p{Lu}`, `\p{Alphabetic}`)

The grammar permits `\p{…}` with a single identifier-like token (no `=`):

* If it matches a **General_Category** value/alias (from `PropertyValueAliases.txt`),
  it is treated as `General_Category=<value>`.
* Otherwise, if it matches a **binary property name/alias** (ECMA-262 table),
  it is treated as that binary property.
* Otherwise, if it matches a **binary property of strings** name (ECMA-262 table),
  it is treated as that string property (only in `v` mode).

Notably: Script names (e.g. `Adlam`) are **not** accepted as “lone” values; they must be spelled
as `Script=Adlam` or `sc=Adlam`.

## Strict matching rules (no loose matching)

`vm-js` must implement **strict matching** for both property names and values:

* **Case-sensitive** (no case folding for property names/values)
* **No whitespace stripping** (e.g. `\p{ General_Category=Lu }` is a `SyntaxError`)
* **No `Is…` or `In…` prefixes** (e.g. `\p{IsScript=Adlam}` / `\p{InAdlam}` are `SyntaxError`s)
* **No alternate separators** (e.g. `:` is invalid; only `=` is allowed)
* **No extra grammar extensions** (e.g. `\p{^…}` is invalid)

This is enforced by both:

* The ECMA-262 static semantics (`UnicodeMatchProperty*`)
* test262 negative tests under `test/built-ins/RegExp/property-escapes/`

## Unicode version policy (must match test262)

**Policy:** target **Unicode v17.0.0** for RegExp property escapes.

Reason: test262’s generated RegExp property-escape tests are explicitly generated for Unicode
v17.0.0 (see the `Unicode v17.0.0` header lines in the generated test files).

This is a pragmatic interoperability constraint: even if the spec references “latest” UCD, our
conformance oracle (test262) is pinned to a concrete Unicode version. If the engine’s Unicode data
drifts (ahead or behind), tests can fail in subtle ways.

Implementation note: keep the `UNICODE_VERSION` constant in
`xtask/src/generate_regexp_unicode_property_strings.rs` aligned with this policy and with test262’s
generated headers.

## Vendored Unicode input files (and other pinned sources)

### UCD snapshot (Unicode v17.0.0)

`vm-js` vendors a minimal Unicode Character Database (UCD) snapshot under:

* `tools/unicode/ucd-17.0.0/`

Files currently used for RegExp Unicode property escapes:

* `PropertyAliases.txt` (property name + alias spellings)
* `PropertyValueAliases.txt` (property value + alias spellings; used for `gc/sc/scx` values)
* `DerivedBinaryProperties.txt` (binary code point property sets)
* `DerivedGeneralCategory.txt` (General_Category sets)
* `Scripts.txt` (Script sets)
* `ScriptExtensions.txt` (Script_Extensions sets)
* `emoji-data.txt` (UCD emoji code point properties like `Emoji`, `Extended_Pictographic`, etc.)
* `CaseFolding.txt` (simple/common foldings; used by `Canonicalize`/`scf`)

Interoperability policy: while the UCD contains many properties/aliases, RegExp property escapes
must expose **only** the properties mandated by ECMA-262:

* Non-binary properties: `gc` / `sc` / `scx`
* Binary code point properties: the 53 properties in ECMA-262 “Binary Unicode properties”
* Binary properties of strings: the 7 emoji properties in ECMA-262 “Binary Unicode properties of strings”

The code point table generator enforces this by hard-coding the spec-required binary property list
and erroring if required properties are missing.

Note: `tools/unicode/emoji-data.txt` at the repo root is a separate input used by the **renderer**
emoji fallback code (currently Unicode 15.1.0). RegExp property escapes use the **Unicode 17.0.0**
copy under `tools/unicode/ucd-17.0.0/emoji-data.txt`.

### `v`-mode properties of strings (emoji sequences)

`vm-js` intentionally sources the *string property* sets from test262’s generated lists to stay
perfectly aligned with test262 (and thus Unicode v17.0.0).

Inputs (test262):

* `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/Basic_Emoji.js`
* `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/Emoji_Keycap_Sequence.js`
* `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/RGI_Emoji_Modifier_Sequence.js`
* `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/RGI_Emoji_Flag_Sequence.js`
* `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/RGI_Emoji_Tag_Sequence.js`
* `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/RGI_Emoji_ZWJ_Sequence.js`
* `vendor/ecma-rs/test262-semantic/data/test/built-ins/RegExp/property-escapes/generated/strings/RGI_Emoji.js` (union; validated by the generator)

Generated output (Rust):

* `vendor/ecma-rs/vm-js/src/regexp_unicode_property_strings.rs` (shared UTF-16 trie; `@generated`)

### Case folding (`CaseFolding.txt`)

RegExp property escapes interact with **case folding** via `Canonicalize` and
`MaybeSimpleCaseFolding`, both of which ultimately rely on the Unicode Character Database’s
`CaseFolding.txt` (status `C` + `S` mappings; no full/Turkic mappings).

Pinned inputs:

* `vendor/ecma-rs/vm-js/unicode/CaseFolding.txt` (Unicode 17.0.0; used by `vendor/ecma-rs/vm-js/build.rs`)
* `tools/unicode/ucd-17.0.0/CaseFolding.txt` (Unicode 17.0.0; used by `vendor/ecma-rs/vm-js/src/bin/gen_unicode_case_folding.rs`)

The two copies should remain identical (same Unicode version) to avoid silent drift between RegExp
case folding and any other consumers of `scf`.

### Code point properties (UCD)

Code point property membership (`gc/sc/scx` and binary code point properties) is generated from the
vendored UCD snapshot into a compact range-table module:

* Generated output: `vendor/ecma-rs/vm-js/src/regexp_unicode_tables.rs` (`@generated`)
* Generator: `vendor/ecma-rs/vm-js/src/bin/generate_regexp_unicode_tables.rs`

## Regenerating the generated tables

### RegExp code point property tables (`\p` / `\P`, `gc/sc/scx`)

The main code point property tables are generated from `tools/unicode/ucd-17.0.0/*` into:

* `vendor/ecma-rs/vm-js/src/regexp_unicode_tables.rs`

Regenerate (writes the file):

```bash
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin generate_regexp_unicode_tables
```

Check-only mode (does not modify files):

```bash
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin generate_regexp_unicode_tables -- --check
```

This generator is responsible for:

* The **exact** supported binary property names/aliases (driven by `PropertyAliases.txt` but
  filtered to the ECMA-262-required set)
* `gc/sc/scx` value alias resolution (`PropertyValueAliases.txt`)
* The code point membership range tables (including surrogate code points where applicable)

### RegExp `v` flag properties of strings table

The `v`-mode properties-of-strings trie is generated by an xtask that parses the test262-style input
files listed above.

```bash
# From the repo root (agent-safe):

# Regenerate (writes vendor/ecma-rs/vm-js/src/regexp_unicode_property_strings.rs):
timeout -k 10 600 bash scripts/cargo_agent.sh xtask generate-regexp-unicode-property-strings

# Check-only mode (does not modify files; exits non-zero if stale):
timeout -k 10 600 bash scripts/cargo_agent.sh xtask generate-regexp-unicode-property-strings --check
```

Inputs note:

- Source of truth is tc39/test262 (`test/built-ins/RegExp/property-escapes/generated/strings/*.js`,
  Unicode v17.0.0).
- FastRender vendors a lightweight snapshot of those inputs under
  `tools/unicode/regexp_unicode_string_props/` so CI can validate deterministically without checking
  out the full `vendor/ecma-rs/test262-semantic/data` submodule.

CI note: `.github/workflows/ci.yml` enforces this via:

```bash
cargo xtask generate-regexp-unicode-property-strings --check
```

After regenerating, run the relevant conformance suites (at minimum, RegExp-related test262
subsets).

### Case folding tables

RegExp case folding is derived from the pinned `CaseFolding.txt` snapshot:

* `vendor/ecma-rs/vm-js/build.rs` reads `vendor/ecma-rs/vm-js/unicode/CaseFolding.txt` and generates a
  compact `(from,to)` table into `OUT_DIR`, which is included by `vendor/ecma-rs/vm-js/src/regexp_case_folding.rs`.
  No manual regeneration is required beyond updating the input text file.
* `vendor/ecma-rs/vm-js/src/unicode_case_folding.rs` is a checked-in `scf` table generated by:

  ```bash
  # From the repo root (agent-safe):
  timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin gen_unicode_case_folding
  ```

  This reads `tools/unicode/ucd-17.0.0/CaseFolding.txt` and rewrites
  `vendor/ecma-rs/vm-js/src/unicode_case_folding.rs`.

## `vm-js` implementation pointers

If you’re updating RegExp Unicode property escape support, these are the main entry points:

* `vendor/ecma-rs/vm-js/src/regexp_unicode_tables.rs` — generated code point property tables:
  property-name/alias resolution, `gc/sc/scx` value resolution, and `contains_code_point(...)`
  membership checks.
* `vendor/ecma-rs/vm-js/src/bin/generate_regexp_unicode_tables.rs` — generator that reads
  `tools/unicode/ucd-17.0.0/*` and rewrites `src/regexp_unicode_tables.rs` (supports `--check`).
* `vendor/ecma-rs/vm-js/src/regexp_unicode_property_strings.rs` — generated trie + exact-name
  lookup for `v`-mode properties of strings (Emoji); generated from test262 `strings/*.js`.
* `xtask/src/generate_regexp_unicode_property_strings.rs` — generator (parses test262 input files,
  validates the `RGI_Emoji` union, supports `--check`).
* `vendor/ecma-rs/vm-js/src/regexp_unicode_resolver.rs` — strict (spec-aligned) resolver for
  `UnicodePropertyValueExpression` parsing (name/value vs lone). Used by unit tests and as a
  reference while wiring the table-driven property escape implementation into `src/regexp.rs`.
* Case folding:
  * `vendor/ecma-rs/vm-js/src/regexp.rs` — `canonicalize` implementation for `u`/`v` ignoreCase.
  * `vendor/ecma-rs/vm-js/src/unicode_case_folding.rs` — `scf` table used for `v`-mode
    `MaybeSimpleCaseFolding`.
  * `vendor/ecma-rs/vm-js/src/regexp_case_folding.rs` + `vendor/ecma-rs/vm-js/build.rs` — build-time
    generation of the compact RegExp folding table from `vm-js/unicode/CaseFolding.txt`.
* (Exploration) `vendor/ecma-rs/vm-js/src/regexp_unicode_icu.rs` and
  `vendor/ecma-rs/vm-js/docs/regexp_unicode_properties_icu4x.md` — ICU4X feasibility spike (not
  wired into the engine).

## Surrogates: property sets include them (tests rely on this)

Unicode property escapes operate over **Unicode code points**, and Unicode defines surrogate code
points (`U+D800..U+DFFF`) even though they are not scalar values.

ECMA-262 + test262 explicitly require surrogate code points to be included in property sets. For
example, `General_Category=Surrogate` must include both high and low surrogate ranges; see:

* `test/built-ins/RegExp/property-escapes/generated/General_Category_-_Surrogate.js`

Implementation note for JS/UTF-16:

* In `u`/`v` mode, the RegExp engine iterates by code points *but* isolated surrogate code units are
  still observed as code points equal to their value.
* Therefore, property escapes must match isolated surrogates correctly.

## Case folding (why `CaseFolding.txt` matters)

Case folding affects RegExp in two places:

1. **`Canonicalize` in `u`/`v` + `ignoreCase` (`i`)**
   * `Canonicalize ( ch )` uses simple/common case folding mappings from `CaseFolding.txt` to map a
     code point to its canonical case-insensitive representative.
2. **`MaybeSimpleCaseFolding` in `v` + `ignoreCase` (`i`)**
   * When `UnicodeSets` is true (`v` flag) *and* `IgnoreCase` is true, many character set operations
     and escape expansions flow through `MaybeSimpleCaseFolding`.
   * This uses the **simple case folding** function `scf(cp)` derived from `CaseFolding.txt` (a
     1→1 mapping). This is *not* full case folding (which can expand to multiple code points).

The `v`-mode behavior is particularly easy to get wrong: folding a range can produce a *set* that
is no longer representable as a single range (the spec even calls this out).
