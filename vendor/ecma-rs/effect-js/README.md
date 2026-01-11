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
      - `Promise.all([fetch(...), ...])`
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

`effect-js` commonly refers to known APIs by a stable identifier type
`effect_js::ApiId` (a re-export of `knowledge_base::ApiId`).

An `ApiId` is a deterministic 64-bit FNV-1a hash of an API's *canonical* knowledge
base name (e.g. `"JSON.parse"`). This allows analyses to store compact IDs while
still being able to recover names via `ApiDatabase::get_by_id(...)`.

To construct an ID directly (e.g. in tests), hash the canonical name:

```rust
use effect_js::ApiId;

let json_parse = ApiId::from_name("JSON.parse");
```

## Running pattern recognition on a TypeScript file

The easiest way to get a `hir-js::LowerResult` (and, optionally, types) is via
`typecheck-ts`:

```rust
use effect_js::recognize_patterns_best_effort_untyped;
use typecheck_ts::{FileKey, MemoryHost, Program};

let key = FileKey::new("index.ts");
let mut host = MemoryHost::new();
host.insert(key.clone(), r#"const parsed: { x: number } = JSON.parse("{\"x\": 1}");"#);

let program = Program::new(host, vec![key.clone()]);
assert!(program.check().is_empty());

let file_id = program.file_id(&key).unwrap();
let lowered = program.hir_lowered(file_id).unwrap();

for body_id in lowered.hir.bodies.iter().copied() {
  let patterns = recognize_patterns_best_effort_untyped(&lowered, body_id);
  for pat in patterns {
    println!("{body_id:?}: {pat:?}");
  }
}
```

### Untyped vs best-effort

`effect-js` exposes two untyped entry points:

- `recognize_patterns_untyped`: patterns that are safe to infer from HIR alone
  (e.g. `JsonParseTyped` using a declared type annotation).
- `recognize_patterns_best_effort_untyped`: a superset that includes additional
  conservative heuristics such as `PromiseAllFetch` (which currently does not
  require full typing).

### Typed patterns (`--features typed`)

Enable `effect-js`'s `typed` feature and pass a `TypeProvider`:

```rust
use std::sync::Arc;
use effect_js::typed::TypedProgram;
use effect_js::recognize_patterns_typed;
use typecheck_ts::Program;

// `TypedProgram` snapshots per-body typing tables out of `typecheck-ts`.
let program = Arc::new(program);
let types = TypedProgram::from_program(program.clone(), file_id);
let patterns = recognize_patterns_typed(&lowered, lowered.root_body(), &types);
```

## Runnable example

```bash
# From the `vendor/ecma-rs/` workspace root:
bash scripts/cargo_agent.sh run -p effect-js --example recognize --features typed
```
