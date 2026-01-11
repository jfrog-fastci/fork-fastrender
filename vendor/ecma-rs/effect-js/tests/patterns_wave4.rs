use effect_js::{recognize_patterns_best_effort_untyped, RecognizedPattern};
use hir_js::ExprKind;

const SRC: &str = r#"
const m = new Map();
const k = "x";
const v = m.has(k) ? m.get(k) : 0;
"#;

#[test]
fn detects_map_get_or_default_conditional_once() {
  let lowered = hir_js::lower_from_source(SRC).expect("lowering succeeds");
  let root_body = lowered.root_body();
  let body = lowered.body(root_body).expect("root body exists");

  let patterns = recognize_patterns_best_effort_untyped(&lowered, root_body);
  let found: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::MapGetOrDefault { map, key, default, .. } => Some((*map, *key, *default)),
      _ => None,
    })
    .collect();

  assert_eq!(found.len(), 1, "expected exactly one MapGetOrDefault pattern");
  let (map, key, default) = found[0];

  let map_expr = &body.exprs[map.0 as usize];
  match &map_expr.kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("m")),
    other => panic!("expected MapGetOrDefault.map to be Ident(m), got {other:?}"),
  }

  let key_expr = &body.exprs[key.0 as usize];
  match &key_expr.kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("k")),
    other => panic!("expected MapGetOrDefault.key to be Ident(k), got {other:?}"),
  }

  assert!(
    matches!(body.exprs[default.0 as usize].kind, ExprKind::Literal(_)),
    "expected MapGetOrDefault.default to be a literal"
  );
}
