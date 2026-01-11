# ts-erase

Shared **TypeScript → JavaScript** erasure and lowering for the `ecma-rs`
toolchain.

This crate operates on a mutable [`parse-js`](../parse-js/) AST:

- removes TypeScript-only syntax (types, interfaces, type-only imports/exports,
  and expression wrappers like `as`, non-null `!`, instantiation type arguments,
  `satisfies`, etc.)
- optionally lowers some TypeScript *runtime* constructs (e.g. `enum`,
  `namespace`) into JavaScript so the result can be parsed/executed as strict
  ECMAScript

It is used by:

- [`minify-js`](../minify-js/) (TS/TSX inputs)
- [`native-oracle-harness`](../native-oracle-harness/) (TS fixtures → JS → `vm-js`)

## APIs

### Full erasure/lowering

Use this when you want the output to be runnable JavaScript and you want TS
runtime constructs lowered where supported.

```rust
use diagnostics::FileId;
use parse_js::SourceType;
use ts_erase::erase_types;

// `top_level` is a `parse-js` AST parsed with Dialect::Ts/Tsx.
erase_types(FileId(0), SourceType::Module, &mut top_level)?;
```

### Strict-native erasure

Strict-native mode only erases TS-only syntax and deterministically rejects TS
runtime constructs (e.g. `enum`, `namespace`, `import =`, `export =`).

It also rejects syntax that is not valid strict ECMAScript (e.g. decorators and
JSX/TSX), since strict-native output is intended to be parsed/executed as
`Dialect::Ecma`.

This is intended for oracle tooling where we want a stable TS→JS pipeline while
still enforcing a strict TS subset.

```rust
use diagnostics::FileId;
use parse_js::SourceType;
use ts_erase::erase_types_strict_native;

erase_types_strict_native(FileId(0), SourceType::Script, &mut top_level)?;
```

## Diagnostics

This crate emits stable diagnostics with the `MINIFYTS####` prefix (legacy from
the original `minify-js` implementation). See `docs/diagnostic-codes.md` for the
repo-wide registry.
