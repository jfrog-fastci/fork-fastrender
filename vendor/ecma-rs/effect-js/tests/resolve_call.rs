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
    .find_map(|(idx, expr)| {
      if expr.span != span {
        return None;
      }
      match &expr.kind {
        ExprKind::Call(_) => Some(ExprId(idx as u32)),
        #[cfg(feature = "hir-semantic-ops")]
        ExprKind::PromiseAll { .. }
        | ExprKind::PromiseRace { .. }
        | ExprKind::ArrayMap { .. }
        | ExprKind::ArrayFilter { .. }
        | ExprKind::ArrayReduce { .. }
        | ExprKind::ArrayChain { .. }
        | ExprKind::KnownApiCall { .. }
        | ExprKind::AwaitExpr { .. } => Some(ExprId(idx as u32)),
        _ => None,
      }
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

#[cfg(feature = "typed")]
#[test]
fn resolves_typed_map_and_promise_instance_methods() {
  use effect_js::typed::TypedProgram;
  use std::sync::Arc;
  use typecheck_ts::{FileKey, MemoryHost, Program};

  let source = r#"
const m: Map<string, number> = new Map();
m.has("a");
m.get("a");
m.set("a", 1);

const s: string = "ABC";
s.trim();

const xs: number[] = [1];
xs.find(x => x === 1);

const p: Promise<number> = Promise.resolve(1);
p.then(x => x + 1);
"#;
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

  let map_has_span = range_of(source, "m.has(\"a\")");
  let map_has = find_call_expr(body, map_has_span);
  let resolved =
    resolve_call(lower, body_id, body, map_has, &db, Some(&types)).expect("resolve Map.has");
  assert_eq!(resolved.api, "Map.prototype.has");
  assert_eq!(resolved.api_id, Some(ApiId::MapPrototypeHas));

  let map_get_span = range_of(source, "m.get(\"a\")");
  let map_get = find_call_expr(body, map_get_span);
  let resolved =
    resolve_call(lower, body_id, body, map_get, &db, Some(&types)).expect("resolve Map.get");
  assert_eq!(resolved.api, "Map.prototype.get");
  assert_eq!(resolved.api_id, Some(ApiId::MapPrototypeGet));

  let map_set_span = range_of(source, "m.set(\"a\", 1)");
  let map_set = find_call_expr(body, map_set_span);
  let resolved =
    resolve_call(lower, body_id, body, map_set, &db, Some(&types)).expect("resolve Map.set");
  assert_eq!(resolved.api, "Map.prototype.set");
  assert_eq!(resolved.api_id, None);

  let string_trim_span = range_of(source, "s.trim()");
  let string_trim = find_call_expr(body, string_trim_span);
  let resolved =
    resolve_call(lower, body_id, body, string_trim, &db, Some(&types)).expect("resolve String.trim");
  assert_eq!(resolved.api, "String.prototype.trim");
  assert_eq!(resolved.api_id, None);

  let array_find_span = range_of(source, "xs.find(x => x === 1)");
  let array_find = find_call_expr(body, array_find_span);
  let resolved =
    resolve_call(lower, body_id, body, array_find, &db, Some(&types)).expect("resolve Array.find");
  assert_eq!(resolved.api, "Array.prototype.find");
  assert_eq!(resolved.api_id, None);

  let promise_then_span = range_of(source, "p.then(x => x + 1)");
  let promise_then = find_call_expr(body, promise_then_span);
  let resolved = resolve_call(lower, body_id, body, promise_then, &db, Some(&types))
    .expect("resolve Promise.then");
  assert_eq!(resolved.api, "Promise.prototype.then");
  assert_eq!(resolved.api_id, Some(ApiId::PromisePrototypeThen));
}
