#![cfg(feature = "typed")]

use diagnostics::TextRange;
use effect_js::{load_default_api_database, resolve_call, resolve_member, ApiId};
use effect_js::typed::TypedProgram;
use hir_js::{BodyId, ExprId, ExprKind};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es2015_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  })
}

fn range_of(source: &str, needle: &str) -> TextRange {
  let start = source.find(needle).expect("needle not found") as u32;
  TextRange::new(start, start + needle.len() as u32)
}

fn find_call_expr(lowered: &hir_js::LowerResult, span: TextRange) -> (BodyId, ExprId) {
  for body_id in lowered.body_index.keys().copied() {
    let Some(body) = lowered.body(body_id) else {
      continue;
    };
    for (idx, expr) in body.exprs.iter().enumerate() {
      if expr.span != span {
        continue;
      }
      match &expr.kind {
        ExprKind::Call(_) => return (body_id, ExprId(idx as u32)),
        _ => continue,
      }
    }
  }
  panic!("call expression not found for span {span:?}")
}

fn find_member_expr(lowered: &hir_js::LowerResult, span: TextRange) -> (BodyId, ExprId) {
  for body_id in lowered.body_index.keys().copied() {
    let Some(body) = lowered.body(body_id) else {
      continue;
    };
    for (idx, expr) in body.exprs.iter().enumerate() {
      if expr.span != span {
        continue;
      }
      match &expr.kind {
        ExprKind::Member(_) => return (body_id, ExprId(idx as u32)),
        _ => continue,
      }
    }
  }
  panic!("member expression not found for span {span:?}")
}

#[test]
fn typed_resolves_common_web_prototype_calls_and_getters() {
  let source = r#"
export {};

interface URL {
  readonly pathname: string;
  toString(): string;
}

declare const URL: {
  prototype: URL;
  new (url: string): URL;
};

interface URLSearchParams {
  get(name: string): string | null;
}

declare const URLSearchParams: {
  prototype: URLSearchParams;
  new (init: string): URLSearchParams;
};

interface Response {
  json(): Promise<any>;
}

declare function fetch(url: string): Promise<Response>;

new URL("https://x").pathname;
new URL("https://x").toString();
new URLSearchParams("a=1").get("a");
fetch("x").then((r: Response) => r.json());
"#;

  let file = FileKey::new("index.ts");
  let mut host = es2015_host();
  host.insert(file.clone(), source);

  let program = Arc::new(Program::new(host, vec![file.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:#?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let lowered = program.hir_lowered(file_id).expect("HIR lowered");
  let lower = lowered.as_ref();
  let types = TypedProgram::from_program(Arc::clone(&program), file_id);
  let kb = load_default_api_database();

  let pathname_span = range_of(source, r#"new URL("https://x").pathname"#);
  let (pathname_body, pathname_expr) = find_member_expr(lower, pathname_span);
  let resolved_pathname =
    resolve_member(&kb, lower, pathname_body, pathname_expr, &types).expect("resolve URL.pathname");
  assert_eq!(resolved_pathname.api, "URL.prototype.pathname");
  assert_eq!(resolved_pathname.api_id, ApiId::from_name("URL.prototype.pathname"));

  let url_to_string_span = range_of(source, r#"new URL("https://x").toString()"#);
  let (to_string_body, to_string_expr) = find_call_expr(lower, url_to_string_span);
  let to_string_body_ref = lower.body(to_string_body).expect("body");
  let resolved_to_string = resolve_call(
    lower,
    to_string_body,
    to_string_body_ref,
    to_string_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve URL.toString()");
  assert_eq!(resolved_to_string.api, "URL.prototype.toString");
  assert_eq!(resolved_to_string.api_id, ApiId::from_name("URL.prototype.toString"));

  let params_get_span = range_of(source, r#"new URLSearchParams("a=1").get("a")"#);
  let (params_body, params_expr) = find_call_expr(lower, params_get_span);
  let params_body_ref = lower.body(params_body).expect("body");
  let resolved_params_get = resolve_call(
    lower,
    params_body,
    params_body_ref,
    params_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve URLSearchParams.get()");
  assert_eq!(resolved_params_get.api, "URLSearchParams.prototype.get");
  assert_eq!(
    resolved_params_get.api_id,
    ApiId::from_name("URLSearchParams.prototype.get")
  );

  let json_span = range_of(source, "r.json()");
  let (json_body, json_expr) = find_call_expr(lower, json_span);
  let json_body_ref = lower.body(json_body).expect("body");
  let resolved_json = resolve_call(
    lower,
    json_body,
    json_body_ref,
    json_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve Response.json()");
  assert_eq!(resolved_json.api, "Response.prototype.json");
  assert_eq!(resolved_json.api_id, ApiId::from_name("Response.prototype.json"));
}
