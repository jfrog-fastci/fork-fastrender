use axum::body;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use optimize_js_debugger::PostCompileDumpReq;
use rmp_serde::{from_slice, to_vec};
use serde::Deserialize;
use std::collections::BTreeMap;
use tower::ServiceExt;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgramDumpPartial {
  top_level: FunctionDumpPartial,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FunctionDumpPartial {
  cfg: CfgDumpPartial,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CfgDumpPartial {
  bblocks: BTreeMap<u32, Vec<InstDumpPartial>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstDumpPartial {
  t: String,
  meta: InstMetaPartial,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstMetaPartial {
  effects: EffectsPartial,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectsPartial {
  unknown: bool,
}

fn build_http_request(uri: &str, body: Vec<u8>) -> Request<Body> {
  Request::builder()
    .uri(uri)
    .method("POST")
    .header("content-type", "application/msgpack")
    .body(Body::from(body))
    .unwrap()
}

#[tokio::test]
async fn compile_dump_endpoint_includes_instruction_meta() {
  let app = optimize_js_debugger::build_app();
  let body = to_vec(&PostCompileDumpReq {
    source: "g();".to_string(),
    is_global: false,
    typed: false,
    semantic_ops: false,
    run_analyses: true,
  })
  .expect("serialize request");

  let response = app
    .oneshot(build_http_request("/compile_dump", body))
    .await
    .expect("response");
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body::to_bytes(response.into_body(), usize::MAX)
    .await
    .expect("read body");
  let parsed: ProgramDumpPartial = from_slice(&bytes).expect("decode msgpack body");

  // The dump should contain per-instruction metadata (at least effects).
  let has_unknown_call_effects = parsed
    .top_level
    .cfg
    .bblocks
    .values()
    .flat_map(|insts| insts.iter())
    .any(|inst| inst.t == "Call" && inst.meta.effects.unknown);
  assert!(
    has_unknown_call_effects,
    "expected at least one Call instruction to be annotated with unknown effects"
  );
}
