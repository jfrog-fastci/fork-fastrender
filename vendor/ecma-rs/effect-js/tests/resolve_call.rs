use diagnostics::TextRange;
use effect_js::{resolve_call, ApiId};
use hir_js::{ExprId, ExprKind, FileKind};

#[cfg(feature = "typed")]
fn es2015_host() -> typecheck_ts::MemoryHost {
  use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
  typecheck_ts::MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  })
}

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
        | ExprKind::ArrayFind { .. }
        | ExprKind::ArrayEvery { .. }
        | ExprKind::ArraySome { .. }
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
  let source = "const a = JSON.parse(\"x\");\nconst b = Promise.all([]);\nconst c = Promise.race([]);\nconst d = JSON[\"parse\"](\"x\");\nconst e = globalThis[\"fetch\"](\"x\");\nconst f = crypto.subtle.encrypt(algo, key, data);\nconst g = crypto.subtle.decrypt(algo, key, data);\nconst h = globalThis.crypto.subtle.sign(algo, key, data);\nconst i = crypto.subtle[\"verify\"](algo, key, sig, data);";
  let lower = hir_js::lower_from_source_with_kind(FileKind::Ts, source).unwrap();
  let body_id = lower.root_body();
  let body = lower.body(body_id).unwrap();
  let db = effect_js::load_default_api_database();

  let json_call_span = range_of(source, "JSON.parse(\"x\")");
  let json_call = find_call_expr(body, json_call_span);
  let resolved = resolve_call(&lower, body_id, body, json_call, &db, None).expect("resolve JSON");
  assert_eq!(resolved.api, "JSON.parse");
  assert_eq!(resolved.api_id, ApiId::from_name("JSON.parse"));
  assert_eq!(resolved.args.len(), 1);

  let promise_call_span = range_of(source, "Promise.all([])");
  let promise_call = find_call_expr(body, promise_call_span);
  let resolved =
    resolve_call(&lower, body_id, body, promise_call, &db, None).expect("resolve Promise");
  assert_eq!(resolved.api, "Promise.all");
  assert_eq!(resolved.api_id, ApiId::from_name("Promise.all"));
  assert_eq!(resolved.args.len(), 1);

  let promise_race_span = range_of(source, "Promise.race([])");
  let promise_race = find_call_expr(body, promise_race_span);
  let resolved =
    resolve_call(&lower, body_id, body, promise_race, &db, None).expect("resolve Promise.race");
  assert_eq!(resolved.api, "Promise.race");
  assert_eq!(resolved.api_id, ApiId::from_name("Promise.race"));
  assert_eq!(resolved.args.len(), 1);

  let json_parse_computed_span = range_of(source, "JSON[\"parse\"](\"x\")");
  let json_parse_computed = find_call_expr(body, json_parse_computed_span);
  let resolved =
    resolve_call(&lower, body_id, body, json_parse_computed, &db, None).expect("resolve JSON[\"parse\"]");
  assert_eq!(resolved.api, "JSON.parse");
  assert_eq!(resolved.api_id, ApiId::from_name("JSON.parse"));
  assert_eq!(resolved.args.len(), 1);

  let fetch_computed_span = range_of(source, "globalThis[\"fetch\"](\"x\")");
  let fetch_computed = find_call_expr(body, fetch_computed_span);
  let resolved =
    resolve_call(&lower, body_id, body, fetch_computed, &db, None).expect("resolve globalThis[\"fetch\"]");
  assert_eq!(resolved.api, "fetch");
  assert_eq!(resolved.api_id, ApiId::from_name("fetch"));
  assert_eq!(resolved.args.len(), 1);

  let encrypt_span = range_of(source, "crypto.subtle.encrypt(algo, key, data)");
  let encrypt_call = find_call_expr(body, encrypt_span);
  let resolved =
    resolve_call(&lower, body_id, body, encrypt_call, &db, None).expect("resolve crypto.subtle.encrypt");
  assert_eq!(resolved.api, "crypto.subtle.encrypt");
  assert_eq!(resolved.api_id, ApiId::from_name("crypto.subtle.encrypt"));
  assert_eq!(resolved.args.len(), 3);

  let decrypt_span = range_of(source, "crypto.subtle.decrypt(algo, key, data)");
  let decrypt_call = find_call_expr(body, decrypt_span);
  let resolved =
    resolve_call(&lower, body_id, body, decrypt_call, &db, None).expect("resolve crypto.subtle.decrypt");
  assert_eq!(resolved.api, "crypto.subtle.decrypt");
  assert_eq!(resolved.api_id, ApiId::from_name("crypto.subtle.decrypt"));
  assert_eq!(resolved.args.len(), 3);

  let sign_span = range_of(source, "globalThis.crypto.subtle.sign(algo, key, data)");
  let sign_call = find_call_expr(body, sign_span);
  let resolved = resolve_call(&lower, body_id, body, sign_call, &db, None)
    .expect("resolve globalThis.crypto.subtle.sign");
  assert_eq!(resolved.api, "crypto.subtle.sign");
  assert_eq!(resolved.api_id, ApiId::from_name("crypto.subtle.sign"));
  assert_eq!(resolved.args.len(), 3);

  let verify_span = range_of(source, "crypto.subtle[\"verify\"](algo, key, sig, data)");
  let verify_call = find_call_expr(body, verify_span);
  let resolved =
    resolve_call(&lower, body_id, body, verify_call, &db, None).expect("resolve crypto.subtle.verify");
  assert_eq!(resolved.api, "crypto.subtle.verify");
  assert_eq!(resolved.api_id, ApiId::from_name("crypto.subtle.verify"));
  assert_eq!(resolved.args.len(), 4);
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn resolves_known_api_call_nodes() {
  use effect_js::hir_rewrite::annotate_known_api_calls;

  let source = "JSON.parse(\"x\");";
  let lower = hir_js::lower_from_source_with_kind(FileKind::Ts, source).unwrap();
  let body_id = lower.root_body();
  let body = lower.body(body_id).unwrap();
  let db = effect_js::load_default_api_database();

  let json_call_span = range_of(source, "JSON.parse(\"x\")");
  let json_call = find_call_expr(body, json_call_span);

  let rewritten = annotate_known_api_calls(&lower, &db, None);
  let rewritten_body = rewritten.body(body_id).unwrap();
  let resolved = resolve_call(&rewritten, body_id, rewritten_body, json_call, &db, None)
    .expect("resolve rewritten JSON.parse");
  assert_eq!(resolved.api, "JSON.parse");
  assert_eq!(resolved.api_id, ApiId::from_name("JSON.parse"));
  assert_eq!(resolved.receiver, None);
  assert_eq!(resolved.args.len(), 1);
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn resolves_semantic_array_find_calls_untyped() {
  let source = "const v = [1].find(x => x);";
  let lower = hir_js::lower_from_source_with_kind(FileKind::Ts, source).unwrap();
  let body_id = lower.root_body();
  let body = lower.body(body_id).unwrap();
  let db = effect_js::load_default_api_database();

  let call_span = range_of(source, "[1].find(x => x)");
  let call_expr = find_call_expr(body, call_span);
  let resolved = resolve_call(&lower, body_id, body, call_expr, &db, None).expect("resolve find");
  assert_eq!(resolved.api, "Array.prototype.find");
  assert_eq!(resolved.api_id, ApiId::from_name("Array.prototype.find"));
  assert_eq!(resolved.args.len(), 1);
  let recv = resolved.receiver.expect("receiver");
  match &body.exprs[recv.0 as usize].kind {
    ExprKind::Array(arr) => assert_eq!(
      arr.elements.len(),
      1,
      "expected receiver to be the `[1]` array literal"
    ),
    other => panic!("expected receiver to be array literal, got {other:?}"),
  }
}

#[cfg(feature = "typed")]
#[test]
fn resolves_typed_array_prototype_methods() {
  use effect_js::typed::TypedProgram;
  use std::sync::Arc;
  use typecheck_ts::{FileKey, Program};

  let source = "const xs: number[] = [1];\nxs.map(x => x + 1);\nxs[\"map\"](x => x + 1);";
  let file = FileKey::new("file0.ts");
  let mut host = es2015_host();
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
  assert_eq!(resolved.api_id, ApiId::from_name("Array.prototype.map"));

  let computed_call_span = range_of(source, "xs[\"map\"](x => x + 1)");
  let computed_call_expr = find_call_expr(body, computed_call_span);
  let resolved = resolve_call(lower, body_id, body, computed_call_expr, &db, Some(&types))
    .expect("resolve xs[\"map\"]");
  assert_eq!(resolved.api, "Array.prototype.map");
  assert_eq!(resolved.api_id, ApiId::from_name("Array.prototype.map"));
}

#[cfg(feature = "typed")]
#[test]
fn resolves_typed_map_and_promise_instance_methods() {
  use effect_js::typed::TypedProgram;
  use std::sync::Arc;
  use typecheck_ts::{FileKey, Program};

  let source = r#"
 const m: Map<string, number> = new Map();
  m.has("a");
  m["has"]("a");
  m.get("a");
  m.set("a", 1);
 
 const s: string = "ABC";
 s.trim();
 
 const xs: number[] = [1];
 xs.find(x => x === 1);
 
  const p: Promise<number> = Promise.resolve(1);
  p.then(x => x + 1);
  p["then"](x => x + 1);
  "#;
  let file = FileKey::new("file0.ts");
  let mut host = es2015_host();
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
  assert_eq!(resolved.api_id, ApiId::from_name("Map.prototype.has"));

  let map_has_computed_span = range_of(source, "m[\"has\"](\"a\")");
  let map_has_computed = find_call_expr(body, map_has_computed_span);
  let resolved = resolve_call(lower, body_id, body, map_has_computed, &db, Some(&types))
    .expect("resolve m[\"has\"]");
  assert_eq!(resolved.api, "Map.prototype.has");
  assert_eq!(resolved.api_id, ApiId::from_name("Map.prototype.has"));

  let map_get_span = range_of(source, "m.get(\"a\")");
  let map_get = find_call_expr(body, map_get_span);
  let resolved =
    resolve_call(lower, body_id, body, map_get, &db, Some(&types)).expect("resolve Map.get");
  assert_eq!(resolved.api, "Map.prototype.get");
  assert_eq!(resolved.api_id, ApiId::from_name("Map.prototype.get"));

  let map_set_span = range_of(source, "m.set(\"a\", 1)");
  let map_set = find_call_expr(body, map_set_span);
  let resolved =
    resolve_call(lower, body_id, body, map_set, &db, Some(&types)).expect("resolve Map.set");
  assert_eq!(resolved.api, "Map.prototype.set");
  assert_eq!(resolved.api_id, ApiId::from_name("Map.prototype.set"));

  let string_trim_span = range_of(source, "s.trim()");
  let string_trim = find_call_expr(body, string_trim_span);
  let resolved =
    resolve_call(lower, body_id, body, string_trim, &db, Some(&types)).expect("resolve String.trim");
  assert_eq!(resolved.api, "String.prototype.trim");
  assert_eq!(resolved.api_id, ApiId::from_name("String.prototype.trim"));

  let array_find_span = range_of(source, "xs.find(x => x === 1)");
  let array_find = find_call_expr(body, array_find_span);
  let resolved =
    resolve_call(lower, body_id, body, array_find, &db, Some(&types)).expect("resolve Array.find");
  assert_eq!(resolved.api, "Array.prototype.find");
  assert_eq!(resolved.api_id, ApiId::from_name("Array.prototype.find"));

  let promise_then_span = range_of(source, "p.then(x => x + 1)");
  let promise_then = find_call_expr(body, promise_then_span);
  let resolved = resolve_call(lower, body_id, body, promise_then, &db, Some(&types))
    .expect("resolve Promise.then");
  assert_eq!(resolved.api, "Promise.prototype.then");
  assert_eq!(resolved.api_id, ApiId::from_name("Promise.prototype.then"));

  let promise_then_computed_span = range_of(source, "p[\"then\"](x => x + 1)");
  let promise_then_computed = find_call_expr(body, promise_then_computed_span);
  let resolved = resolve_call(lower, body_id, body, promise_then_computed, &db, Some(&types))
    .expect("resolve p[\"then\"]");
  assert_eq!(resolved.api, "Promise.prototype.then");
  assert_eq!(resolved.api_id, ApiId::from_name("Promise.prototype.then"));
}
