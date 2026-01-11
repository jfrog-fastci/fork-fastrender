use effect_js::{ApiDatabase, ArrayChainOp, ArrayTerminal, RecognizedPattern};
use hir_js::{BodyId, ExprId, ExprKind};
use std::collections::BTreeSet;

#[cfg(feature = "typed")]
use effect_js::{recognize_patterns_typed, typed::TypedProgram};
#[cfg(not(feature = "typed"))]
use effect_js::recognize_patterns_best_effort_untyped;
#[cfg(feature = "typed")]
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
declare function require(spec: string): any;

// Demonstrate knowledge-base resolution (CommonJS require binding).
const fs = require("node:fs");
fs.readFile("x", () => {});

// Typed array chain pattern.
const arr: number[] = [1, 2, 3];
arr.filter(x => x > 1);
arr.reduce((a, b) => a + b, 0);
const total = arr
  .map(x => x + 1)
  .filter(x => x > 1)
  .reduce((a, b) => a + b, 0);

// Array destructuring pattern.
const [firstNum, secondNum] = arr;

// String template literal pattern (2+ spans).
const firstName = "Alice";
const lastName = "Bob";
const greeting = `${firstName} ${lastName}`;

// Object spread pattern.
const baseObj = { a: 1 };
const merged = { ...baseObj, x: 1, y: 2 };

// Guard clause pattern.
function guardDemo(x?: string) {
  if (!x) return;
  return x.toLowerCase();
}

// Async iterator pattern.
declare const asyncIterable: AsyncIterable<number>;
async function asyncIterDemo() {
  for await (const item of asyncIterable) {
    break;
  }
}

// Typed Map.get-or-default pattern.
const m: Map<string, number> = new Map();
const key = "a";
const v = m.has(key) ? m.get(key) : 0;

// Annotation-driven JSON.parse pattern.
const parsed: { x: number } = JSON.parse("{\"x\": 1}");

// Promise.all + fetch (declare fetch so typecheck works without DOM libs).
//
// `Array.prototype.map`'s callback type includes `(value, index, array)`, so we
// spell out the full parameter list to keep the type checker happy.
declare function fetch(url: string, index: number, array: string[]): Promise<number>;
const urls: string[] = ["https://example.com"];
Promise.all(urls.map(fetch) as any as Promise<number>[]).then(xs => xs[0]);

// Best-effort Promise.all([fetch(...), ...]) pattern.
Promise.all([
  fetch(urls[0] as string, 0, urls),
  fetch(urls[0] as string, 0, urls),
]).then(xs => xs[0]);
"#;

fn format_kb_semantics(db: &ApiDatabase, api: &str) -> String {
  match db.get(api) {
    Some(sem) => format!("effects={:?} purity={:?}", sem.effects, sem.purity),
    None => "kb=<missing>".to_string(),
  }
}

fn format_pattern(db: &ApiDatabase, pat: &RecognizedPattern) -> String {
  match pat {
    RecognizedPattern::CanonicalCall { call, api } => {
      let api_name = db
        .get_by_id(*api)
        .map(|sem| sem.name.clone())
        .unwrap_or_else(|| format!("id: 0x{:x}", api.raw()));
      format!(
        "CanonicalCall(call={}, api={}, {})",
        call.0,
        api_name,
        format_kb_semantics(db, &api_name)
      )
    }
    RecognizedPattern::MapFilterReduce {
      base,
      map_call,
      filter_call,
      reduce_call,
    } => format!(
      "MapFilterReduce(base={}, map_call={}, filter_call={}, reduce_call={})",
      base.0, map_call.0, filter_call.0, reduce_call.0
    ),
    RecognizedPattern::ArrayChain { base, ops, terminal } => {
      let ops: Vec<&'static str> = ops
        .iter()
        .map(|op| match op {
          ArrayChainOp::Map { .. } => "map",
          ArrayChainOp::Filter { .. } => "filter",
          ArrayChainOp::FlatMap { .. } => "flatMap",
        })
        .collect();
      let terminal_kind = match terminal {
        Some(ArrayTerminal::Reduce { .. }) => "reduce",
        Some(ArrayTerminal::Find { .. }) => "find",
        Some(ArrayTerminal::Every { .. }) => "every",
        Some(ArrayTerminal::Some { .. }) => "some",
        Some(ArrayTerminal::ForEach { .. }) => "forEach",
        None => "none",
      };
      format!("ArrayChain(base={}, ops={ops:?}, terminal={terminal_kind})", base.0)
    }
    RecognizedPattern::PromiseAllFetch {
      promise_all_call,
      fetch_call_count,
      map_call,
      urls_expr,
    } => format!(
      "PromiseAllFetch(call={}, urls_expr={} map_call={:?} fetch_calls={})",
      promise_all_call.0,
      urls_expr.0,
      map_call.map(|id| id.0),
      fetch_call_count
    ),
    RecognizedPattern::MapGetOrDefault {
      conditional,
      map,
      key,
      default,
    } => format!(
      "MapGetOrDefault(conditional={}, map={}, key={}, default={})",
      conditional.0, map.0, key.0, default.0
    ),
    RecognizedPattern::JsonParseTyped { call, target } => {
      format!("JsonParseTyped(call={}, target_type={})", call.0, target.0)
    }
    RecognizedPattern::AsyncIterator { stmt, iterable, .. } => {
      format!("AsyncIterator(stmt={}, iterable={})", stmt.0, iterable.0)
    }
    RecognizedPattern::StringTemplate { expr, span_count } => {
      format!("StringTemplate(expr={}, spans={})", expr.0, span_count)
    }
    RecognizedPattern::ObjectSpread { expr, spread_count } => {
      format!("ObjectSpread(expr={}, spreads={})", expr.0, spread_count)
    }
    RecognizedPattern::ArrayDestructure {
      stmt,
      pat,
      arity,
      source,
    } => format!(
      "ArrayDestructure(stmt={}, pat={}, arity={}, source={})",
      stmt.0, pat.0, arity, source.0
    ),
    RecognizedPattern::GuardClause { stmt, test, kind } => {
      format!("GuardClause(stmt={}, test={}, kind={kind:?})", stmt.0, test.0)
    }
  }
}

fn run(
  lowered: &hir_js::LowerResult,
  db: &ApiDatabase,
  recognize: impl Fn(BodyId) -> Vec<RecognizedPattern>,
) {
  let mut seen = BTreeSet::<&'static str>::new();

  for (body_idx, body_id) in lowered.hir.bodies.iter().copied().enumerate() {
    let Some(body) = lowered.body(body_id) else {
      println!("== Body #{body_idx} ({body_id:?}) ==");
      println!("(missing body data)");
      continue;
    };

    let owner_name = lowered
      .def(body.owner)
      .and_then(|def| lowered.names.resolve(def.name))
      .unwrap_or("<unknown>");
    println!(
      "== Body #{body_idx} ({kind:?}, owner={owner_name}, {body_id:?}) ==",
      kind = body.kind
    );

    // 1) Resolve calls through the knowledge base (require/import binding → canonical API string).
    println!("resolved_calls:");
    let mut any_resolved = false;
    for (idx, expr) in body.exprs.iter().enumerate() {
      if !matches!(expr.kind, ExprKind::Call(_)) {
        continue;
      }
      let expr_id = ExprId(idx as u32);
      if let Some(api) = effect_js::resolve_api_call(db, lowered, body_id, expr_id) {
        any_resolved = true;
        println!("  - call {} -> {} ({})", expr_id.0, api, format_kb_semantics(db, api));
      }
    }
    if !any_resolved {
      println!("  (none)");
    }

    // 2) Recognize higher-level patterns (typed when available, plus best-effort untyped patterns).
    println!("recognized_patterns:");
    let patterns = recognize(body_id);

    if patterns.is_empty() {
      println!("  (none)");
    } else {
      for pat in &patterns {
        match pat {
          RecognizedPattern::CanonicalCall { .. } => seen.insert("CanonicalCall"),
          RecognizedPattern::MapFilterReduce { .. } => seen.insert("MapFilterReduce"),
          RecognizedPattern::ArrayChain { .. } => seen.insert("ArrayChain"),
          RecognizedPattern::MapGetOrDefault { .. } => seen.insert("MapGetOrDefault"),
          RecognizedPattern::PromiseAllFetch { .. } => seen.insert("PromiseAllFetch"),
          RecognizedPattern::JsonParseTyped { .. } => seen.insert("JsonParseTyped"),
          RecognizedPattern::AsyncIterator { .. } => seen.insert("AsyncIterator"),
          RecognizedPattern::StringTemplate { .. } => seen.insert("StringTemplate"),
          RecognizedPattern::ObjectSpread { .. } => seen.insert("ObjectSpread"),
          RecognizedPattern::ArrayDestructure { .. } => seen.insert("ArrayDestructure"),
          RecognizedPattern::GuardClause { .. } => seen.insert("GuardClause"),
        };
        println!("  - {}", format_pattern(db, pat));
      }
    }

    println!();
  }

  // Keep the example self-checking: if a pattern silently regresses, this should fail loudly.
  assert!(
    seen.contains("CanonicalCall"),
    "expected example to produce `CanonicalCall` pattern"
  );
  assert!(
    seen.contains("JsonParseTyped"),
    "expected example to produce `JsonParseTyped` pattern"
  );
  assert!(
    seen.contains("PromiseAllFetch"),
    "expected example to produce `PromiseAllFetch` pattern"
  );
  assert!(
    seen.contains("StringTemplate"),
    "expected example to produce `StringTemplate` pattern"
  );
  assert!(
    seen.contains("ObjectSpread"),
    "expected example to produce `ObjectSpread` pattern"
  );
  assert!(
    seen.contains("ArrayDestructure"),
    "expected example to produce `ArrayDestructure` pattern"
  );
  assert!(
    seen.contains("GuardClause"),
    "expected example to produce `GuardClause` pattern"
  );
  assert!(
    seen.contains("AsyncIterator"),
    "expected example to produce `AsyncIterator` pattern"
  );

  #[cfg(feature = "typed")]
  {
    assert!(
      seen.contains("MapFilterReduce"),
      "expected example to produce `MapFilterReduce` pattern"
    );
    assert!(
      seen.contains("MapGetOrDefault"),
      "expected example to produce `MapGetOrDefault` pattern"
    );
  }
}

#[cfg(feature = "typed")]
fn main() {
  use std::sync::Arc;
  use typecheck_ts::lib_support::{CompilerOptions, ScriptTarget};

  let index_key = FileKey::new("index.ts");
  let mut options = CompilerOptions::default();
  // `for await ... of` requires ES2018 lib definitions (AsyncIterable, etc).
  options.target = ScriptTarget::Es2018;

  let mut host = MemoryHost::with_options(options);
  host.insert(index_key.clone(), INDEX_TS);

  let program = Arc::new(Program::new(host, vec![index_key.clone()]));
  let diagnostics = program.check();
  if !diagnostics.is_empty() {
    eprintln!("typecheck diagnostics: {diagnostics:#?}");
    std::process::exit(1);
  }

  let file = program.file_id(&index_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let lowered = lowered.as_ref();

  let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
  db.validate().expect("knowledge base validates");

  let types = TypedProgram::from_program(program.clone(), file);
  run(lowered, &db, |body_id| recognize_patterns_typed(lowered, body_id, &types));
}

#[cfg(not(feature = "typed"))]
fn main() {
  let lowered = hir_js::lower_from_source_with_kind(hir_js::FileKind::Ts, INDEX_TS).unwrap();

  let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
  db.validate().expect("knowledge base validates");

  run(&lowered, &db, |body_id| recognize_patterns_best_effort_untyped(&lowered, body_id));
}
