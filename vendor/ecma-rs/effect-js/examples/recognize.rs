use effect_js::{ApiDatabase, RecognizedPattern};
use hir_js::{ExprId, ExprKind};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[cfg(feature = "typed")]
use effect_js::{recognize_patterns_typed, typed::TypecheckProgram};
#[cfg(not(feature = "typed"))]
use effect_js::recognize_patterns_untyped;

const INDEX_TS: &str = r#"
declare function require(spec: string): any;

// Demonstrate knowledge-base resolution (CommonJS require binding).
const fs = require("node:fs");
fs.readFile("x", () => {});

// Typed array chain pattern.
const arr: number[] = [1, 2, 3];
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
    RecognizedPattern::MapGetOrDefault { map, key, default } => format!(
      "MapGetOrDefault(map={}, key={}, default={})",
      map.0, key.0, default.0
    ),
    RecognizedPattern::JsonParseTyped { call, target } => {
      format!("JsonParseTyped(call={}, target_type={})", call.0, target.0)
    }
  }
}

fn main() {
  let index_key = FileKey::new("index.ts");
  let mut host = MemoryHost::new();
  host.insert(index_key.clone(), INDEX_TS);

  let program = Program::new(host, vec![index_key.clone()]);
  let diagnostics = program.check();
  if !diagnostics.is_empty() {
    eprintln!("typecheck diagnostics: {diagnostics:#?}");
    std::process::exit(1);
  }

  let file = program.file_id(&index_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");

  let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
  db.validate().expect("knowledge base validates");

  #[cfg(feature = "typed")]
  let types = TypecheckProgram::new(&program);

  let mut seen = std::collections::BTreeSet::<&'static str>::new();

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
      if let Some(api) = effect_js::resolve_api_call(&db, &lowered, body_id, expr_id) {
        any_resolved = true;
        println!("  - call {} -> {} ({})", expr_id.0, api, format_kb_semantics(&db, api));
      }
    }
    if !any_resolved {
      println!("  (none)");
    }

    // 2) Recognize higher-level patterns (optionally typed).
    println!("recognized_patterns:");
    #[cfg(feature = "typed")]
    let patterns = recognize_patterns_typed(&lowered, body_id, &types);
    #[cfg(not(feature = "typed"))]
    let patterns = recognize_patterns_untyped(&lowered, body_id);

    if patterns.is_empty() {
      println!("  (none)");
    } else {
      for pat in &patterns {
        match pat {
          RecognizedPattern::CanonicalCall { .. } => seen.insert("CanonicalCall"),
          RecognizedPattern::MapFilterReduce { .. } => seen.insert("MapFilterReduce"),
          RecognizedPattern::MapGetOrDefault { .. } => seen.insert("MapGetOrDefault"),
          RecognizedPattern::JsonParseTyped { .. } => seen.insert("JsonParseTyped"),
        };
        println!("  - {}", format_pattern(&db, pat));
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
