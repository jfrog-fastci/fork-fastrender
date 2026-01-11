#![cfg(feature = "typed")]

use effect_js::{load_default_api_database, recognize_patterns_typed, RecognizedPattern};
use effect_js::typed::TypedProgram;
use hir_js::{ExprId, ExprKind};
use std::sync::Arc;
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
const m: Map<string, number> = new Map();
const k = "x";
const v = m.has(k) ? m.get(k) : 123;
"#;

const INDEX_TS_COMPUTED: &str = r#"
const m: Map<string, number> = new Map();
const k = "x";
const v = m["has"](k) ? m["get"](k) : 123;
"#;

#[test]
fn recognizes_map_get_or_default_conditional() {
  let index_key = FileKey::new("index.ts");

  let mut host = MemoryHost::new();
  host.insert(index_key.clone(), INDEX_TS);

  let program = Arc::new(Program::new(host, vec![index_key.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:#?}"
  );

  let file = program.file_id(&index_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let root_body = lowered.root_body();
  let body = lowered.body(root_body).expect("root body exists");

  let cond_expr_id = body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| {
      matches!(&expr.kind, ExprKind::Conditional { .. }).then_some(ExprId(idx as u32))
    })
    .expect("found conditional expression");

  let ExprKind::Conditional {
    test,
    consequent,
    alternate,
  } = &body.exprs[cond_expr_id.0 as usize].kind
  else {
    unreachable!("conditional expr id points at conditional node")
  };
  let test = *test;
  let consequent = *consequent;
  let alternate = *alternate;

  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let kb = load_default_api_database();
  let map_has = kb.id_of("Map.prototype.has").unwrap();
  let map_get = kb.id_of("Map.prototype.get").unwrap();
  let patterns = recognize_patterns_typed(&kb, &lowered, root_body, &types);

  // Call resolution: `m.has(k)` and `m.get(k)` should resolve to Map prototype APIs.
  assert!(
    patterns.iter().any(|pat| matches!(
      pat,
      RecognizedPattern::CanonicalCall { call, api } if *call == test && *api == map_has
    )),
    "expected conditional test call to resolve to Map.prototype.has"
  );
  assert!(
    patterns.iter().any(|pat| matches!(
      pat,
      RecognizedPattern::CanonicalCall { call, api } if *call == consequent && *api == map_get
    )),
    "expected conditional consequent call to resolve to Map.prototype.get"
  );

  // Pattern recognition: `m.has(k) ? m.get(k) : 123`.
  let matches: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::MapGetOrDefault {
        conditional,
        map,
        key,
        default,
      } if *conditional == cond_expr_id => Some((*map, *key, *default)),
      _ => None,
    })
    .collect();

  assert_eq!(matches.len(), 1, "expected one MapGetOrDefault pattern");
  let (map_expr, key_expr, default_expr) = matches[0];
  assert_eq!(
    default_expr, alternate,
    "expected MapGetOrDefault default to point at conditional alternate"
  );

  // Ensure the pattern points at the `m`/`k` nodes in the `m.has(k)` test.
  let ExprKind::Call(test_call) = &body.exprs[test.0 as usize].kind else {
    panic!("conditional test should be a call expression");
  };
  let ExprKind::Member(test_member) = &body.exprs[test_call.callee.0 as usize].kind else {
    panic!("conditional test callee should be a member expression");
  };
  assert_eq!(map_expr, test_member.object);
  assert_eq!(key_expr, test_call.args[0].expr);
}

#[test]
fn recognizes_map_get_or_default_conditional_via_computed_key() {
  let index_key = FileKey::new("index.ts");

  let mut host = MemoryHost::new();
  host.insert(index_key.clone(), INDEX_TS_COMPUTED);

  let program = Arc::new(Program::new(host, vec![index_key.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:#?}"
  );

  let file = program.file_id(&index_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let root_body = lowered.root_body();
  let body = lowered.body(root_body).expect("root body exists");

  let cond_expr_id = body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| {
      matches!(&expr.kind, ExprKind::Conditional { .. }).then_some(ExprId(idx as u32))
    })
    .expect("found conditional expression");

  let ExprKind::Conditional {
    test,
    consequent,
    alternate,
  } = &body.exprs[cond_expr_id.0 as usize].kind
  else {
    unreachable!("conditional expr id points at conditional node")
  };
  let test = *test;
  let consequent = *consequent;
  let alternate = *alternate;

  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let kb = load_default_api_database();
  let map_has = kb.id_of("Map.prototype.has").unwrap();
  let map_get = kb.id_of("Map.prototype.get").unwrap();
  let patterns = recognize_patterns_typed(&kb, &lowered, root_body, &types);

  // Call resolution: `m["has"](k)` and `m["get"](k)` should resolve to Map prototype APIs.
  assert!(
    patterns.iter().any(|pat| matches!(
      pat,
      RecognizedPattern::CanonicalCall { call, api } if *call == test && *api == map_has
    )),
    "expected conditional test call to resolve to Map.prototype.has"
  );
  assert!(
    patterns.iter().any(|pat| matches!(
      pat,
      RecognizedPattern::CanonicalCall { call, api } if *call == consequent && *api == map_get
    )),
    "expected conditional consequent call to resolve to Map.prototype.get"
  );

  // Pattern recognition: `m["has"](k) ? m["get"](k) : 123`.
  let matches: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::MapGetOrDefault {
        conditional,
        map,
        key,
        default,
      } if *conditional == cond_expr_id => Some((*map, *key, *default)),
      _ => None,
    })
    .collect();

  assert_eq!(matches.len(), 1, "expected one MapGetOrDefault pattern");
  let (map_expr, key_expr, default_expr) = matches[0];
  assert_eq!(
    default_expr, alternate,
    "expected MapGetOrDefault default to point at conditional alternate"
  );

  // Ensure the pattern points at the `m`/`k` nodes in the `m["has"](k)` test.
  let ExprKind::Call(test_call) = &body.exprs[test.0 as usize].kind else {
    panic!("conditional test should be a call expression");
  };
  let ExprKind::Member(test_member) = &body.exprs[test_call.callee.0 as usize].kind else {
    panic!("conditional test callee should be a member expression");
  };
  assert_eq!(map_expr, test_member.object);
  assert_eq!(key_expr, test_call.args[0].expr);
}
