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
fn span_map_def_at_offset_finds_object_literal_method_defs_on_property_key() {
  let source = r#"
    const obj = {
      methodKey() { return 1; },
      get getterKey() { return 2; },
      set setterKey(v) { v; },
    };
  "#;
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);

  let method_offset = (source.find("methodKey").expect("methodKey") + 2) as u32;
  let method_def_id = lowered
    .hir
    .span_map
    .def_at_offset(method_offset)
    .expect("def at methodKey offset");
  let method_def = lowered.def(method_def_id).expect("methodKey def data");
  assert_eq!(method_def.path.kind, DefKind::Method);
  assert_eq!(lowered.names.resolve(method_def.name), Some("methodKey"));

  let getter_offset = (source.find("getterKey").expect("getterKey") + 2) as u32;
  let getter_def_id = lowered
    .hir
    .span_map
    .def_at_offset(getter_offset)
    .expect("def at getterKey offset");
  let getter_def = lowered.def(getter_def_id).expect("getterKey def data");
  assert_eq!(getter_def.path.kind, DefKind::Getter);
  assert_eq!(lowered.names.resolve(getter_def.name), Some("getterKey"));

  let setter_offset = (source.find("setterKey").expect("setterKey") + 2) as u32;
  let setter_def_id = lowered
    .hir
    .span_map
    .def_at_offset(setter_offset)
    .expect("def at setterKey offset");
  let setter_def = lowered.def(setter_def_id).expect("setterKey def data");
  assert_eq!(setter_def.path.kind, DefKind::Setter);
  assert_eq!(lowered.names.resolve(setter_def.name), Some("setterKey"));
}

#[test]
fn span_map_def_at_offset_finds_computed_object_literal_method_defs_on_property_key_expression() {
  // Computed method keys are still definitions; `SpanMap::def_at_offset` should
  // be able to find them when querying within the computed key expression.
  let source = r#"
    const obj = {
      [computedKey]() { return 1; },
    };
  "#;
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);

  let offset = (source.find("computedKey").expect("computedKey") + 2) as u32;
  let def_id = lowered
    .hir
    .span_map
    .def_at_offset(offset)
    .expect("def at computed key offset");
  let def = lowered.def(def_id).expect("def data");
  assert_eq!(def.path.kind, DefKind::Method);
}

#[test]
fn unary_postfix_traversal_collects_nested_defs() {
  let source = r#"
    const obj: any = { a: 0 };
    obj[(function inner() { return "a"; })()]++;
  "#;
  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
  assert_no_missing_exprs(&lowered);
}

