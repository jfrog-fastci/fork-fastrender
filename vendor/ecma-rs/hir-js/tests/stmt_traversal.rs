use hir_js::ExprKind;
use hir_js::FileKind;
use hir_js::lower_from_source_with_kind;
use hir_js::DefKind;

fn assert_no_missing_exprs(lowered: &hir_js::LowerResult) {
  for body in lowered.bodies.iter() {
    for expr in body.exprs.iter() {
      assert!(
        !matches!(expr.kind, ExprKind::Missing),
        "found ExprKind::Missing at span {:?}",
        expr.span
      );
    }
  }
}

#[test]
fn traverses_throw_statement_children() {
  let source = r#"
    function outer() {
      throw (function inner() { return 1; });
    }
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn traverses_labeled_statement_children() {
  let source = r#"
     function outer() {
       label: (function inner() { return 1; });
     }
   "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn span_map_finds_for_of_binding_defs() {
  let source = r#"
    function outer(list) {
      for (const bound of list) {}
    }
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  let offset = source.find("bound").expect("bound offset") as u32;
  let def_id = lowered
    .hir
    .span_map
    .def_at_offset(offset)
    .expect("def at offset");
  let def = lowered.def(def_id).expect("def data");
  assert_eq!(def.path.kind, DefKind::Var);
  assert_eq!(lowered.names.resolve(def.name), Some("bound"));
  assert_no_missing_exprs(&lowered);
}

#[test]
fn span_map_finds_catch_binding_defs() {
  let source = r#"
    function outer() {
      try { throw 1; } catch (caught) { }
    }
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  let offset = source.find("caught").expect("caught offset") as u32;
  let def_id = lowered
    .hir
    .span_map
    .def_at_offset(offset)
    .expect("def at offset");
  let def = lowered.def(def_id).expect("def data");
  assert_eq!(def.path.kind, DefKind::Var);
  assert_eq!(lowered.names.resolve(def.name), Some("caught"));
  assert_no_missing_exprs(&lowered);
}

#[test]
fn traverses_param_destructuring_pattern_expressions() {
  let source = r#"
    function f({ [(() => 1)()]: a = (() => 2)() }) {}
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  let arrow_defs: Vec<_> = lowered
    .defs
    .iter()
    .filter(|def| def.path.kind == DefKind::Function && lowered.names.resolve(def.name) == Some("<arrow>"))
    .collect();
  assert_eq!(
    arrow_defs.len(),
    2,
    "expected to pre-collect nested arrow defs inside param patterns"
  );
  assert_no_missing_exprs(&lowered);
}

#[test]
fn traverses_for_of_destructuring_pattern_expressions() {
  let source = r#"
    function f(list) {
      for (const { [(() => 1)()]: a = (() => 2)() } of list) {}
    }
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  let arrow_defs: Vec<_> = lowered
    .defs
    .iter()
    .filter(|def| def.path.kind == DefKind::Function && lowered.names.resolve(def.name) == Some("<arrow>"))
    .collect();
  assert_eq!(
    arrow_defs.len(),
    2,
    "expected to pre-collect nested arrow defs inside for-of patterns"
  );
  assert_no_missing_exprs(&lowered);
}

#[test]
fn traverses_catch_destructuring_pattern_expressions() {
  let source = r#"
    try { throw 1; } catch ({ [(() => 1)()]: a = (() => 2)() }) {}
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  let arrow_defs: Vec<_> = lowered
    .defs
    .iter()
    .filter(|def| def.path.kind == DefKind::Function && lowered.names.resolve(def.name) == Some("<arrow>"))
    .collect();
  assert_eq!(
    arrow_defs.len(),
    2,
    "expected to pre-collect nested arrow defs inside catch patterns"
  );
  assert_no_missing_exprs(&lowered);
}
