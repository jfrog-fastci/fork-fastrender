# effect-js

`effect-js` is a small analysis crate that sits between:

- **syntax** (`parse-js` → `hir-js`), and
- **typing** (`typecheck-ts`, optional),

and produces higher-level **semantic signals** that downstream passes can use for
effect/purity inference and optimization.

It provides two foundational pieces:

1. **API semantics**
   - A structured database (`knowledge-base::ApiDatabase`) describing known APIs
     (e.g. `node:fs.readFile`, `Map.prototype.get`) and their *effect*/*purity*
     templates.
2. **Pattern recognition**
   - Lightweight recognition of common idioms and call sites in `hir-js` (optionally
     using types), producing `RecognizedPattern` values such as:
     - `arr.map(...).filter(...).reduce(...)`
     - `map.get(key) ?? default`
     - `const x: T = JSON.parse(...)`

The long-term goal is for these pieces to feed an effect inference engine that
can prove that code is pure/read-only/IO/etc, enabling aggressive compilation
and scheduling decisions.

## Loading the API database

The semantic knowledge base lives in the sibling crate `knowledge-base/`.

At build time, `knowledge-base/build.rs`:

- scans `knowledge-base/{core,node,web,ecosystem}` for `.yaml`/`.toml` files,
- sorts them deterministically, and
- embeds them via `include_str!` into the compiled crate.

To load the bundled database:

```rust
use effect_js::ApiDatabase;

let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
db.validate().expect("knowledge base is internally consistent");
```

### Deterministic `ApiId`

In addition to string-keyed APIs from `ApiDatabase`, `effect-js` has a small
hand-curated `ApiId` enum for high-value "canonical" surfaces that we want to
recognize quickly and refer to by a stable ID. `ApiId::as_str()` maps each
variant to its canonical name.

## Running pattern recognition on a TypeScript file

The easiest way to get a `hir-js::LowerResult` (and, optionally, types) is via
`typecheck-ts`:

```rust
use effect_js::recognize_patterns_untyped;
use typecheck_ts::{FileKey, MemoryHost, Program};

let key = FileKey::new("index.ts");
let mut host = MemoryHost::new();
host.insert(key.clone(), r#"const parsed: { x: number } = JSON.parse("{\"x\": 1}");"#);

let program = Program::new(host, vec![key.clone()]);
assert!(program.check().is_empty());

let file_id = program.file_id(&key).unwrap();
let lowered = program.hir_lowered(file_id).unwrap();

for body_id in lowered.hir.bodies.iter().copied() {
  let patterns = recognize_patterns_untyped(&lowered, body_id);
  for pat in patterns {
    println!("{body_id:?}: {pat:?}");
  }
}
```

### Typed patterns (`--features typed`)

Enable `effect-js`'s `typed` feature and pass a `TypeProvider`:

```rust
use effect_js::typed::TypecheckProgram;
use effect_js::recognize_patterns_typed;

# // ...after creating `program` and `lowered` as above...
let types = TypecheckProgram::new(&program);
let patterns = recognize_patterns_typed(&lowered, lowered.root_body(), &types);
```

## Runnable example

```bash
# From the `vendor/ecma-rs/` workspace root:
cargo run -p effect-js --example recognize --features typed
```

