use effect_js::{recognize_patterns_best_effort_untyped, ApiDatabase, RecognizedPattern};
use hir_js::{BodyId, ExprId, ExprKind};
use std::collections::BTreeSet;

#[cfg(feature = "typed")]
use effect_js::{recognize_patterns_typed, typed::TypedProgram};
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

// Typed Map.get-or-default pattern.
const m: Map<string, number> = new Map();
const v = m.get("a") ?? 0;

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
      let api_name = api.as_str();
      format!(
        "CanonicalCall(call={}, api={}, {})",
        call.0,
        api_name,
        format_kb_semantics(db, api_name)
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
    RecognizedPattern::PromiseAllFetch {
      all_call,
      fetch_calls,
    } => format!(
      "PromiseAllFetch(all_call={}, fetch_calls={})",
      all_call.0,
      fetch_calls.len()
    ),
    RecognizedPattern::MapGetOrDefault { map, key, default } => format!(
      "MapGetOrDefault(map={}, key={}, default={})",
      map.0, key.0, default.0
    ),
    RecognizedPattern::JsonParseTyped { call, target } => {
      format!("JsonParseTyped(call={}, target_type={})", call.0, target.0)
    }
    RecognizedPattern::StringTemplate { template } => {
      format!("StringTemplate(template={})", template.0)
    }
    RecognizedPattern::ObjectSpread {
      object,
      spreads,
      keys,
    } => format!(
      "ObjectSpread(object={}, spreads={}, keys={:?})",
      object.0,
      spreads.len(),
      keys
    ),
    RecognizedPattern::ArrayDestructure {
      source,
      bindings,
      has_rest,
    } => format!(
      "ArrayDestructure(source={}, bindings={}, has_rest={})",
      source.0, bindings, has_rest
    ),
    RecognizedPattern::GuardClause {
      test,
      guard_kind,
      subject,
    } => format!(
      "GuardClause(test={}, kind={:?}, subject={})",
      test.0, guard_kind, subject.0
    ),
    RecognizedPattern::AsyncIterator { iterable } => {
      format!("AsyncIterator(iterable={})", iterable.0)
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
          RecognizedPattern::MapGetOrDefault { .. } => seen.insert("MapGetOrDefault"),
          RecognizedPattern::PromiseAllFetch { .. } => seen.insert("PromiseAllFetch"),
          RecognizedPattern::JsonParseTyped { .. } => seen.insert("JsonParseTyped"),
          RecognizedPattern::StringTemplate { .. } => seen.insert("StringTemplate"),
          RecognizedPattern::ObjectSpread { .. } => seen.insert("ObjectSpread"),
          RecognizedPattern::ArrayDestructure { .. } => seen.insert("ArrayDestructure"),
          RecognizedPattern::GuardClause { .. } => seen.insert("GuardClause"),
          RecognizedPattern::AsyncIterator { .. } => seen.insert("AsyncIterator"),
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

  let index_key = FileKey::new("index.ts");
  let mut host = MemoryHost::new();
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
  run(lowered, &db, |body_id| {
    let mut patterns = recognize_patterns_typed(lowered, body_id, &types);
    patterns.extend(
      recognize_patterns_best_effort_untyped(lowered, body_id)
        .into_iter()
        .filter(|pat| matches!(pat, RecognizedPattern::PromiseAllFetch { .. })),
    );
    patterns
  });
}

#[cfg(not(feature = "typed"))]
fn main() {
  let lowered = hir_js::lower_from_source_with_kind(hir_js::FileKind::Ts, INDEX_TS).unwrap();

  let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
  db.validate().expect("knowledge base validates");

  run(&lowered, &db, |body_id| recognize_patterns_best_effort_untyped(&lowered, body_id));
}
