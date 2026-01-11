use effect_js::{recognize_patterns_best_effort_untyped, GuardKind, RecognizedPattern};
use hir_js::ExprKind;

#[test]
fn guard_clause_nullish_comparison_is_recognized() {
  let src = r#"
let x: unknown = null;
if (x == null) throw new Error("x");
x;
"#;
  let lowered = hir_js::lower_from_source(src).expect("lower");
  let body_id = lowered.root_body();
  let body = lowered.body(body_id).expect("root body");

  let patterns = recognize_patterns_best_effort_untyped(&lowered, body_id);
  let guards: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::GuardClause { test, kind, .. } => Some((*test, *kind)),
      _ => None,
    })
    .collect();

  assert_eq!(guards.len(), 1, "expected exactly one GuardClause");
  let (subject, kind) = guards[0];
  assert_eq!(kind, GuardKind::Throw);

  match &body.exprs[subject.0 as usize].kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("x")),
    other => panic!("expected GuardClause subject to be Ident(x), got {other:?}"),
  }
}

#[test]
fn guard_clause_undefined_comparison_is_recognized() {
  let src = r#"
let x: unknown;
if (x === undefined) throw new Error("x");
x;
"#;
  let lowered = hir_js::lower_from_source(src).expect("lower");
  let body_id = lowered.root_body();
  let body = lowered.body(body_id).expect("root body");

  let patterns = recognize_patterns_best_effort_untyped(&lowered, body_id);
  let guards: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::GuardClause { test, kind, .. } => Some((*test, *kind)),
      _ => None,
    })
    .collect();

  assert_eq!(guards.len(), 1, "expected exactly one GuardClause");
  let (subject, kind) = guards[0];
  assert_eq!(kind, GuardKind::Throw);

  match &body.exprs[subject.0 as usize].kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("x")),
    other => panic!("expected GuardClause subject to be Ident(x), got {other:?}"),
  }
}

#[test]
fn array_destructure_with_holes_is_recognized() {
  let src = r#"
const arr = [1, 2, 3];
const [first, , third] = arr;
"#;
  let lowered = hir_js::lower_from_source(src).expect("lower");
  let body_id = lowered.root_body();
  let body = lowered.body(body_id).expect("root body");

  let patterns = recognize_patterns_best_effort_untyped(&lowered, body_id);
  let destructures: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::ArrayDestructure { source, arity, .. } => Some((*source, *arity)),
      _ => None,
    })
    .collect();

  assert_eq!(
    destructures.len(),
    1,
    "expected exactly one ArrayDestructure pattern"
  );
  let (source, arity) = destructures[0];
  assert_eq!(arity, 2, "expected ArrayDestructure.arity to count bindings");

  match &body.exprs[source.0 as usize].kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("arr")),
    other => panic!("expected ArrayDestructure.source to be Ident(arr), got {other:?}"),
  }
}

#[test]
fn object_spread_skips_computed_keys() {
  let src = r#"
const a = { x: 1 };
const k = "y";
const o = { ...a, [k]: 2 };
"#;
  let lowered = hir_js::lower_from_source(src).expect("lower");
  let body_id = lowered.root_body();

  let patterns = recognize_patterns_best_effort_untyped(&lowered, body_id);
  assert!(
    !patterns
      .iter()
      .any(|pat| matches!(pat, RecognizedPattern::ObjectSpread { .. })),
    "expected ObjectSpread to be skipped when computed keys are present"
  );
}

#[test]
fn object_spread_skips_accessors() {
  let src = r#"
const a = { x: 1 };
const o = {
  ...a,
  get y() { return 1; },
};
"#;
  let lowered = hir_js::lower_from_source(src).expect("lower");
  let body_id = lowered.root_body();

  let patterns = recognize_patterns_best_effort_untyped(&lowered, body_id);
  assert!(
    !patterns
      .iter()
      .any(|pat| matches!(pat, RecognizedPattern::ObjectSpread { .. })),
    "expected ObjectSpread to be skipped when accessors are present"
  );
}

