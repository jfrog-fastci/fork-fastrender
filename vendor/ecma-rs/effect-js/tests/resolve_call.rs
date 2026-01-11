use diagnostics::TextRange;
use effect_js::{resolve_call, ApiId};
use hir_js::{ExprId, ExprKind, FileKind};

fn range_of(source: &str, needle: &str) -> TextRange {
  let start = source.find(needle).expect("needle not found") as u32;
  TextRange::new(start, start + needle.len() as u32)
}

fn find_call_expr(body: &hir_js::Body, span: TextRange) -> ExprId {
  body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| match &expr.kind {
      ExprKind::Call(_) if expr.span == span => Some(ExprId(idx as u32)),
      _ => None,
    })
    .expect("call expression not found for span")
}

#[test]
fn resolves_static_known_calls() {
  let source = "const a = JSON.parse(\"x\");\nconst b = Promise.all([]);";
  let lower = hir_js::lower_from_source_with_kind(FileKind::Ts, source).unwrap();
  let body_id = lower.root_body();
  let body = lower.body(body_id).unwrap();
  let db = effect_js::load_default_api_database();

  let json_call_span = range_of(source, "JSON.parse(\"x\")");
  let json_call = find_call_expr(body, json_call_span);
  let resolved = resolve_call(&lower, body_id, body, json_call, &db, None).expect("resolve JSON");
  assert_eq!(resolved.api, "JSON.parse");
  assert_eq!(resolved.api_id, Some(ApiId::JsonParse));
  assert_eq!(resolved.args.len(), 1);

  let promise_call_span = range_of(source, "Promise.all([])");
  let promise_call = find_call_expr(body, promise_call_span);
  let resolved =
    resolve_call(&lower, body_id, body, promise_call, &db, None).expect("resolve Promise");
  assert_eq!(resolved.api, "Promise.all");
  assert_eq!(resolved.api_id, Some(ApiId::PromiseAll));
  assert_eq!(resolved.args.len(), 1);
}

#[cfg(feature = "typed")]
#[test]
fn resolves_typed_array_prototype_methods() {
  use effect_js::typed::TypedProgram;
  use std::sync::Arc;
  use typecheck_ts::{FileKey, MemoryHost, Program};

  let source = "const xs: number[] = [1];\nxs.map(x => x + 1);";
  let file = FileKey::new("file0.ts");
  let mut host = MemoryHost::new();
  host.insert(file.clone(), source);
  let program = Arc::new(Program::new(host, vec![file.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let lowered = program.hir_lowered(file_id).expect("HIR lowered");
  let lower = lowered.as_ref();
  let body_id = lower.root_body();
  let body = lower.body(body_id).unwrap();
  let types = TypedProgram::from_program(program.clone(), file_id);
  let db = effect_js::load_default_api_database();

  let call_span = range_of(source, "xs.map(x => x + 1)");
  let call_expr = find_call_expr(body, call_span);
  let resolved =
    resolve_call(lower, body_id, body, call_expr, &db, Some(&types)).expect("resolve map");
  assert_eq!(resolved.api, "Array.prototype.map");
  assert_eq!(resolved.api_id, Some(ApiId::ArrayPrototypeMap));
}
