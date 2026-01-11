use effect_js::{recognize_patterns_best_effort_untyped, GuardKind, RecognizedPattern};
use hir_js::{BinaryOp, ExprKind, Literal};

const SRC: &str = r#"
const a = "a";
const b = "b";
const tmpl = `${a} ${b}`;

const base = { x: 1 };
const obj = { foo: 1, ...base, bar: 2 };

const arr = [1, 2, 3];
const [first, , third, ...rest] = arr;

let x: unknown = null;
if (x == null) throw new Error("x");
"#;

#[test]
fn detects_wave2_patterns_once() {
  let lowered = hir_js::lower_from_source(SRC).expect("lowering succeeds");
  let root_body = lowered.root_body();
  let body = lowered.body(root_body).expect("root body exists");

  let patterns = recognize_patterns_best_effort_untyped(&lowered, root_body);

  let templates: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::StringTemplate { template } => Some(*template),
      _ => None,
    })
    .collect();
  assert_eq!(templates.len(), 1, "expected exactly one StringTemplate pattern");
  let template_expr = &body.exprs[templates[0].0 as usize];
  match &template_expr.kind {
    ExprKind::Template(template) => assert!(
      template.spans.len() >= 2,
      "expected template literal to have 2+ spans"
    ),
    other => panic!("expected template ExprKind::Template, got {other:?}"),
  }

  let spreads: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::ObjectSpread {
        object,
        spreads,
        keys,
      } => Some((*object, spreads.clone(), keys.clone())),
      _ => None,
    })
    .collect();
  assert_eq!(spreads.len(), 1, "expected exactly one ObjectSpread pattern");
  let (object, spread_exprs, keys) = spreads[0].clone();
  assert_eq!(
    keys,
    vec!["foo".to_string(), "bar".to_string()],
    "expected static keys to be collected in source order"
  );
  assert_eq!(
    spread_exprs.len(),
    1,
    "expected object literal to contain exactly one spread"
  );
  assert!(
    matches!(body.exprs[object.0 as usize].kind, ExprKind::Object(_)),
    "expected ObjectSpread.object to reference an object literal"
  );
  let spread0 = &body.exprs[spread_exprs[0].0 as usize];
  match &spread0.kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("base")),
    other => panic!("expected spread expr to be Ident(base), got {other:?}"),
  }

  let destructures: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::ArrayDestructure {
        source,
        bindings,
        has_rest,
      } => Some((*source, *bindings, *has_rest)),
      _ => None,
    })
    .collect();
  assert_eq!(
    destructures.len(),
    1,
    "expected exactly one ArrayDestructure pattern"
  );
  let (source, bindings, has_rest) = destructures[0];
  assert_eq!(bindings, 2, "expected ArrayDestructure.bindings to ignore holes");
  assert!(has_rest, "expected ArrayDestructure.has_rest to be true");
  let source_expr = &body.exprs[source.0 as usize];
  match &source_expr.kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("arr")),
    other => panic!("expected ArrayDestructure.source to be Ident(arr), got {other:?}"),
  }

  let guards: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::GuardClause {
        test,
        guard_kind,
        subject,
      } => Some((*test, *guard_kind, *subject)),
      _ => None,
    })
    .collect();
  assert_eq!(guards.len(), 1, "expected exactly one GuardClause pattern");
  let (test, guard_kind, subject) = guards[0];
  assert_eq!(guard_kind, GuardKind::Throw);

  let test_expr = &body.exprs[test.0 as usize];
  match &test_expr.kind {
    ExprKind::Binary { op, left, right } => {
      assert!(
        matches!(op, BinaryOp::Equality | BinaryOp::StrictEquality),
        "expected guard clause test to be ==/=== comparison"
      );
      let left_kind = &body.exprs[left.0 as usize].kind;
      let right_kind = &body.exprs[right.0 as usize].kind;
      assert!(
        matches!(
          (left_kind, right_kind),
          (ExprKind::Literal(Literal::Null | Literal::Undefined), _)
            | (_, ExprKind::Literal(Literal::Null | Literal::Undefined))
        ),
        "expected guard clause test to compare against null/undefined"
      );
    }
    other => panic!("expected binary guard test, got {other:?}"),
  }

  let subject_expr = &body.exprs[subject.0 as usize];
  match &subject_expr.kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("x")),
    other => panic!("expected guard clause subject to be Ident(x), got {other:?}"),
  }
}
