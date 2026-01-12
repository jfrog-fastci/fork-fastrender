#[cfg(feature = "typed")]
mod typed {
  use effect_js::StringEncoding;
  use knowledge_base::{parse_api_semantics_yaml_str, ApiDatabase};
  use std::sync::Arc;
  use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
  use typecheck_ts::{FileKey, Program};

  fn es2015_host() -> typecheck_ts::MemoryHost {
    typecheck_ts::MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
      ..Default::default()
    })
  }

  fn find_first_expr(
    body: &hir_js::Body,
    pred: impl Fn(&hir_js::ExprKind) -> bool,
  ) -> hir_js::ExprId {
    body
      .exprs
      .iter()
      .enumerate()
      .find_map(|(idx, expr)| pred(&expr.kind).then_some(hir_js::ExprId(idx as u32)))
      .expect("expected to find matching expression in test body")
  }

  #[test]
  fn trim_start_preserves_ascii_via_kb() {
    use effect_js::typed::TypedProgram;

    let key = FileKey::new("index.ts");
    let mut host = es2015_host();
    host.insert(key.clone(), "\"  ABC\".trimStart();");

    let program = Arc::new(Program::new(host, vec![key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&key).expect("index.ts loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body_id = lowered.root_body();
    let root_body = lowered.body(root_body_id).expect("root body exists");

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));
    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let kb = effect_js::load_default_api_database();
    let results = effect_js::encoding::analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }

  #[test]
  fn trim_end_preserves_ascii_via_kb() {
    use effect_js::typed::TypedProgram;

    let key = FileKey::new("index.ts");
    let mut host = es2015_host();
    host.insert(key.clone(), "\"ABC  \".trimEnd();");

    let program = Arc::new(Program::new(host, vec![key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&key).expect("index.ts loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body_id = lowered.root_body();
    let root_body = lowered.body(root_body_id).expect("root body exists");

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));
    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let kb = effect_js::load_default_api_database();
    let results = effect_js::encoding::analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }

  #[test]
  fn substring_preserves_ascii_via_kb() {
    use effect_js::typed::TypedProgram;

    let key = FileKey::new("index.ts");
    let mut host = es2015_host();
    host.insert(key.clone(), "\"ABC\".substring(1);");

    let program = Arc::new(Program::new(host, vec![key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&key).expect("index.ts loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body_id = lowered.root_body();
    let root_body = lowered.body(root_body_id).expect("root body exists");

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));
    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let kb = effect_js::load_default_api_database();
    let results = effect_js::encoding::analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }

  #[test]
  fn concat_joins_encodings_across_args() {
    use effect_js::typed::TypedProgram;

    let key = FileKey::new("index.ts");
    let mut host = es2015_host();
    host.insert(key.clone(), "\"a\".concat(\"hé\", \"b\");");

    let program = Arc::new(Program::new(host, vec![key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&key).expect("index.ts loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body_id = lowered.root_body();
    let root_body = lowered.body(root_body_id).expect("root body exists");

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));
    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let kb = effect_js::load_default_api_database();
    let results = effect_js::encoding::analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Latin1);
  }

  #[test]
  fn number_to_string_encoding_is_kb_driven() {
    use effect_js::typed::TypedProgram;

    let key = FileKey::new("index.ts");
    let mut host = es2015_host();
    host.insert(key.clone(), "const n: number = 42; n.toString();");

    let program = Arc::new(Program::new(host, vec![key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&key).expect("index.ts loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body_id = lowered.root_body();
    let root_body = lowered.body(root_body_id).expect("root body exists");

    let expr_id = find_first_expr(root_body, |kind| {
      matches!(kind, hir_js::ExprKind::Call(call) if !call.is_new)
    });

    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let entries = parse_api_semantics_yaml_str(
      r#"
- name: Number.prototype.toString
  properties:
    encoding.output: utf8
"#,
    )
    .unwrap();
    let kb = ApiDatabase::from_entries(entries);
    let results = effect_js::encoding::analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Utf8);
  }
}
