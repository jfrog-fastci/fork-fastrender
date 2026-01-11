use effect_js::{load_default_api_database, recognize_patterns_best_effort_untyped, RecognizedPattern};
use hir_js::ExprKind;

const SRC: &str = r#"
async function f(asyncIterable: any) {
  for await (const x of asyncIterable) {
    x;
  }
}
"#;

#[test]
fn detects_async_iterator_pattern_once() {
  let lowered = hir_js::lower_from_source(SRC).expect("lowering succeeds");
  let kb = load_default_api_database();

  let mut matches = Vec::new();
  for body_id in lowered.hir.bodies.iter().copied() {
    let Some(body) = lowered.body(body_id) else {
      continue;
    };
    for pat in recognize_patterns_best_effort_untyped(&kb, &lowered, body_id) {
      let RecognizedPattern::AsyncIterator { iterable, .. } = pat else {
        continue;
      };
      matches.push((body_id, iterable));
      let iterable_expr = &body.exprs[iterable.0 as usize];
      match &iterable_expr.kind {
        ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("asyncIterable")),
        other => panic!("expected async iterator iterable to be Ident(asyncIterable), got {other:?}"),
      }
    }
  }

  assert_eq!(matches.len(), 1, "expected exactly one AsyncIterator pattern");
}
