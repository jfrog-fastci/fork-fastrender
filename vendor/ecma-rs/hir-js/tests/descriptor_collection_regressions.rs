use hir_js::{lower_from_source_with_kind, DefKind, ExprKind, FileKind};

fn assert_no_missing_exprs(result: &hir_js::LowerResult) {
  for body in result.bodies.iter() {
    for (idx, expr) in body.exprs.iter().enumerate() {
      assert!(
        !matches!(expr.kind, ExprKind::Missing),
        "unexpected ExprKind::Missing in {:?} body {:?} at expr #{idx} (span {:?})",
        body.kind,
        body.owner,
        expr.span,
      );
    }
  }
}

#[test]
fn unary_nesting_collects_child_expressions() {
  let source = "const x = !(function inner() { return 1; });";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn new_with_class_expression_nesting_collects_child_expressions() {
  let source = "const x = new (class Inner { constructor() {} })();";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn await_with_arrow_nesting_collects_child_expressions() {
  let source = "async function f(){ return await (() => 1)(); }";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn dynamic_import_expression_collects_children() {
  let source = r#"
const x = import(function inner() { return 1; });
const y = import("m", { with: { type: (() => "json")() } });
"#;
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn object_literal_computed_key_collects_children() {
  let source = r#"const obj = { [(() => "k")()]: 1 };"#;
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn parameter_destructuring_default_collects_children() {
  let source = "function f({a = (() => 1)()}) { return a; }";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn for_of_decl_lhs_creates_var_defs_and_traverses_pattern_defaults() {
  let source = "for (const {a = (()=>1)()} of xs) { }";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);

  let a_offset = (source.find("{a =").expect("pattern binding") + 1) as u32;
  let def_id = lowered
    .hir
    .span_map
    .def_at_offset(a_offset)
    .expect("def at binding offset");
  let def = lowered.def(def_id).expect("def data for span map entry");

  assert_eq!(def.path.kind, DefKind::Var, "expected `a` binding to be a var def");
  assert_eq!(
    lowered.names.resolve(def.name),
    Some("a"),
    "expected `a` binding to resolve to name \"a\""
  );
}

#[test]
fn catch_binding_creates_var_defs() {
  let source = "try { throw 1; } catch (e) { e; }";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);

  let e_offset = (source.find("catch (e)").expect("catch binding") + "catch (".len()) as u32;
  let def_id = lowered
    .hir
    .span_map
    .def_at_offset(e_offset)
    .expect("def at catch binding offset");
  let def = lowered.def(def_id).expect("def data for span map entry");

  assert_eq!(def.path.kind, DefKind::Var, "expected `e` binding to be a var def");
  assert_eq!(
    lowered.names.resolve(def.name),
    Some("e"),
    "expected `e` binding to resolve to name \"e\""
  );
}

#[test]
fn ts_export_assignment_traverses_expression_children() {
  let source = "export = (function inner() { return 1; });";
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);
}

