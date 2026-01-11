use effect_js::{load_default_api_database, recognize_patterns_best_effort_untyped, GuardKind, RecognizedPattern};
use hir_js::ExprKind;

const SRC: &str = r#"
const a = "a";
const b = "b";
const tmpl = `${a} ${b}`;

const base = { x: 1 };
const obj = { foo: 1, ...base, bar: 2 };

const arr = [1, 2, 3];
const [first, third] = arr;

let x: unknown = null;
if (!x) throw new Error("x");
"#;

#[test]
fn detects_wave2_patterns_once() {
  let lowered = hir_js::lower_from_source(SRC).expect("lowering succeeds");
  let root_body = lowered.root_body();
  let body = lowered.body(root_body).expect("root body exists");

  let kb = load_default_api_database();
  let patterns = recognize_patterns_best_effort_untyped(&kb, &lowered, root_body);

  let templates: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::StringTemplate { expr, span_count } => Some((*expr, *span_count)),
      _ => None,
    })
    .collect();
  assert_eq!(templates.len(), 1, "expected exactly one StringTemplate pattern");
  let (template, span_count) = templates[0];
  let template_expr = &body.exprs[template.0 as usize];
  match &template_expr.kind {
    ExprKind::Template(template) => assert!(
      template.spans.len() == span_count && span_count >= 2,
      "expected template literal to have 2+ spans"
    ),
    other => panic!("expected template ExprKind::Template, got {other:?}"),
  }

  let spreads: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::ObjectSpread { expr, spread_count } => Some((*expr, *spread_count)),
      _ => None,
    })
    .collect();
  assert_eq!(spreads.len(), 1, "expected exactly one ObjectSpread pattern");
  let (object, spread_count) = spreads[0];
  assert_eq!(spread_count, 1, "expected one spread property");
  assert!(
    matches!(body.exprs[object.0 as usize].kind, ExprKind::Object(_)),
    "expected ObjectSpread.object to reference an object literal"
  );

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
  assert_eq!(arity, 2, "expected ArrayDestructure.arity to match pattern length");
  let source_expr = &body.exprs[source.0 as usize];
  match &source_expr.kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("arr")),
    other => panic!("expected ArrayDestructure.source to be Ident(arr), got {other:?}"),
  }

  let guards: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      RecognizedPattern::GuardClause { test, kind, .. } => Some((*test, *kind)),
      _ => None,
    })
    .collect();
  assert_eq!(guards.len(), 1, "expected exactly one GuardClause pattern");
  let (test, kind) = guards[0];
  assert_eq!(kind, GuardKind::Throw);

  let test_expr = &body.exprs[test.0 as usize];
  match &test_expr.kind {
    ExprKind::Ident(name) => assert_eq!(lowered.names.resolve(*name), Some("x")),
    other => panic!("expected guard clause test to be Ident(x), got {other:?}"),
  }
}
