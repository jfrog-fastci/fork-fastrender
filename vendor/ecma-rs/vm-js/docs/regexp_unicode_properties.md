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

## Vendored Unicode input files

RegExp property escapes draw from a small set of Unicode data files:

Unicode Character Database (UCD):

* `PropertyAliases.txt` (property name + alias spellings)
* `PropertyValueAliases.txt` (property value + alias spellings)
* `DerivedGeneralCategory.txt` (General_Category sets)
* `Scripts.txt` (Script sets)
* `ScriptExtensions.txt` (Script_Extensions sets)
* `DerivedBinaryProperties.txt` (binary code-point property sets)
* `CaseFolding.txt` (simple/common case folding; used by `Canonicalize` + `MaybeSimpleCaseFolding`)

Emoji / UTS #51 (for `v`-mode properties of strings):

* `emoji-data.txt` (code point properties like `Emoji`, `Emoji_Component`, etc.)
* `emoji-sequences.txt`
* `emoji-zwj-sequences.txt`

Where these live in-tree is intentionally part of the contract: the goal is to avoid depending on
network access during builds or during regeneration.

## Regenerating the generated tables

RegExp Unicode property escapes should be backed by **generated Rust tables** (typically range
lists and tries) derived from the vendored Unicode input files above.

The generator must support:

* A normal mode that **writes** the generated tables.
* A `--check` mode that **verifies** the checked-in tables are up-to-date and fails otherwise
  (for CI and for developer sanity).

When implemented, the workflow should look like:

```bash
# From the repo root (agent-safe):

# Regenerate (writes files):
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin gen_regexp_unicode_tables

# Check-only mode (does not modify files; exits non-zero if stale):
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh run -p vm-js --bin gen_regexp_unicode_tables -- --check
```

After regenerating, run the relevant conformance suites (at minimum, RegExp-related test262
subsets).

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

