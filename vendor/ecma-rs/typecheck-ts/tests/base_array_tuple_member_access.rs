use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::FileId;
use hir_js::{lower_from_source, BodyKind};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use typecheck_ts::check::caches::CheckerCaches;
use typecheck_ts::check::hir_body::{check_body, AstIndex};
use typecheck_ts::lib_support::ScriptTarget;
use types_ts_interned::{TypeDisplay, TypeStore};

fn check_function_return_type(source: &str) -> String {
  let lowered = lower_from_source(source).expect("lower");
  let (body_id, body) = lowered
    .bodies
    .iter()
    .enumerate()
    .find(|(_, b)| matches!(b.kind, BodyKind::Function))
    .map(|(idx, b)| (lowered.hir.bodies[idx], b.as_ref()))
    .expect("function body");

  let ast = parse_with_options(
    source,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  )
  .expect("parse");
  let ast = Arc::new(ast);
  let ast_index = AstIndex::new(Arc::clone(&ast), FileId(0), None);

  let store = TypeStore::new();
  let caches = CheckerCaches::new(Default::default()).for_body();
  let bindings = HashMap::new();
  let result = check_body(
    body_id,
    body,
    &lowered.names,
    FileId(0),
    &ast_index,
    Arc::clone(&store),
    ScriptTarget::Es2015,
    true,
    &caches,
    &bindings,
    &HashMap::new(),
    None,
    None,
  );

  assert!(
    result.diagnostics().is_empty(),
    "expected no diagnostics, got {:?}",
    result.diagnostics()
  );
  assert_eq!(
    result.return_types().len(),
    1,
    "expected exactly one return type"
  );

  let rendered = TypeDisplay::new(&store, result.return_types()[0]).to_string();
  rendered
}

#[test]
fn array_index_access_with_literal_key_is_typed() {
  let source = "function f() { const arr = [1, 2]; return arr[0]; }";
  let ty = check_function_return_type(source);
  assert_eq!(ty, "number");
}

#[test]
fn array_length_property_is_typed() {
  let source = "function f() { const arr = [1, 2]; return arr.length; }";
  let ty = check_function_return_type(source);
  assert_eq!(ty, "number");
}

#[test]
fn tuple_index_access_is_typed_by_index() {
  let source = "function f() { const t: [string, number] = [\"a\", 1]; return t[1]; }";
  let ty = check_function_return_type(source);
  assert_eq!(ty, "number");
}
