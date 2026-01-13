use vm_js::{Heap, HeapLimits, ImportAttribute, JsString, ModuleRequest, SourceTextModuleRecord, VmError};

fn assert_syntax_error(source: &str) {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  match SourceTextModuleRecord::parse(&mut heap, source) {
    Err(VmError::Syntax(_)) => {}
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn parses_import_with_attributes() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let module =
    SourceTextModuleRecord::parse(&mut heap, r#"import x from "m" with { type: "json" };"#).unwrap();
  assert_eq!(
    module.requested_modules,
    vec![ModuleRequest::new(
      JsString::from_str("m").unwrap(),
      vec![ImportAttribute::new("type", "json")],
    )]
  );
}

#[test]
fn parses_export_with_attributes() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let module =
    SourceTextModuleRecord::parse(&mut heap, r#"export * from "m" with { type: "json" };"#).unwrap();
  assert_eq!(
    module.requested_modules,
    vec![ModuleRequest::new(
      JsString::from_str("m").unwrap(),
      vec![ImportAttribute::new("type", "json")],
    )]
  );
}

#[test]
fn sorts_attributes_and_dedupes_requested_modules() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let module = SourceTextModuleRecord::parse(
    &mut heap,
    r#"
      import x from "m" with { b: "2", a: "1" };
      import y from "m" with { a: "1", b: "2" };
    "#,
  )
  .unwrap();

  assert_eq!(module.requested_modules.len(), 1);
  assert_eq!(
    module.requested_modules[0].attributes,
    vec![
      ImportAttribute::new("a", "1"),
      ImportAttribute::new("b", "2"),
    ]
  );
}

#[test]
fn requested_modules_distinguish_different_attributes() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let module = SourceTextModuleRecord::parse(
    &mut heap,
    r#"
      import x from "m" with { type: "json" };
      import y from "m" with { type: "css" };
    "#,
  )
  .unwrap();
  assert_eq!(module.requested_modules.len(), 2);
}

#[test]
fn rejects_invalid_attribute_shapes() {
  assert_syntax_error(r#"import x from "m" with 1;"#);
  assert_syntax_error(r#"import x from "m" with { ["type"]: "json" };"#);
  assert_syntax_error(r#"import x from "m" with { type: 1 };"#);
  assert_syntax_error(r#"import x from "m" with { type: "json", type: "json" };"#);
}
