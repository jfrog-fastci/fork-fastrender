use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use optimize_js::TopLevelMode;
use optimize_js_debugger::{compile_program_dump, PostCompileErrorRes, PostCompileReq, ProgramDump};
use rmp_serde;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::str::FromStr;
use tower_http::cors::Any;
use tower_http::cors::CorsLayer;

/// MessagePack request/response wrapper.
///
/// We implement this internally rather than depending on `axum-msgpack` to keep
/// the server's dependency graph (and compile time) minimal.
#[derive(Debug)]
pub struct MsgPack<T>(pub T);

#[axum::async_trait]
impl<S, T> axum::extract::FromRequest<S> for MsgPack<T>
where
  S: Send + Sync,
  T: serde::de::DeserializeOwned,
{
  type Rejection = (StatusCode, String);

  async fn from_request(
    req: axum::http::Request<axum::body::Body>,
    _state: &S,
  ) -> Result<Self, Self::Rejection> {
    let (parts, body) = req.into_parts();
    if let Some(content_type) = parts.headers.get(axum::http::header::CONTENT_TYPE) {
      let content_type = content_type.as_bytes();
      let ok = content_type.starts_with(b"application/msgpack")
        || content_type.starts_with(b"application/x-msgpack");
      if !ok {
        return Err((
          StatusCode::UNSUPPORTED_MEDIA_TYPE,
          "expected application/msgpack".to_string(),
        ));
      }
    }

    let bytes = axum::body::to_bytes(body, usize::MAX)
      .await
      .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    rmp_serde::from_slice(&bytes).map(MsgPack).map_err(|err| {
      (
        StatusCode::BAD_REQUEST,
        format!("invalid msgpack payload: {err}"),
      )
    })
  }
}

impl<T> axum::response::IntoResponse for MsgPack<T>
where
  T: serde::Serialize,
{
  fn into_response(self) -> axum::response::Response {
    match rmp_serde::to_vec_named(&self.0) {
      Ok(buf) => (
        [(axum::http::header::CONTENT_TYPE, "application/msgpack")],
        buf,
      )
        .into_response(),
      Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
  }
}

pub async fn handle_post_compile(
  MsgPack(PostCompileReq { source, is_global }): MsgPack<PostCompileReq>,
) -> Result<MsgPack<ProgramDump>, (StatusCode, MsgPack<PostCompileErrorRes>)> {
  let top_level_mode = if is_global {
    TopLevelMode::Global
  } else {
    TopLevelMode::Module
  };
  match compile_program_dump(&source, top_level_mode) {
    Ok(dump) => Ok(MsgPack(dump)),
    Err(diagnostics) => Err((
      StatusCode::BAD_REQUEST,
      MsgPack(PostCompileErrorRes {
        ok: false,
        diagnostics,
      }),
    )),
  }
}

fn build_app() -> Router {
  Router::new()
    .route("/compile", post(handle_post_compile))
    .layer(
      CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any),
    )
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
  let mut iter = args.iter();
  while let Some(arg) = iter.next() {
    if arg == flag {
      return iter.next().map(|v| v.to_string());
    }
    if let Some(value) = arg.strip_prefix(&(flag.to_owned() + "=")) {
      return Some(value.to_string());
    }
  }
  None
}

fn read_source(path: Option<PathBuf>) -> io::Result<String> {
  if let Some(path) = path {
    fs::read_to_string(path)
  } else {
    let mut src = String::new();
    io::stdin().read_to_string(&mut src)?;
    Ok(src)
  }
}

fn run_snapshot_mode(args: &[String]) -> Result<bool, Box<dyn std::error::Error>> {
  if !args.iter().any(|arg| arg == "--snapshot") {
    return Ok(false);
  }
  let input = arg_value(args, "--input").map(PathBuf::from);
  let output = arg_value(args, "--output").map(PathBuf::from);
  let mode = arg_value(args, "--mode")
    .and_then(|m| TopLevelMode::from_str(&m).ok())
    .unwrap_or(TopLevelMode::Module);

  let source = read_source(input)?;
  match compile_program_dump(&source, mode) {
    Ok(snapshot) => {
      let json = serde_json::to_string_pretty(&snapshot)?;
      if let Some(path) = output {
        fs::write(path, json)?;
      } else {
        println!("{json}");
      }
    }
    Err(diags) => {
      let json = serde_json::to_string_pretty(&PostCompileErrorRes {
        ok: false,
        diagnostics: diags,
      })?;
      let mut stderr = io::stderr();
      stderr.write_all(json.as_bytes())?;
      stderr.write_all(b"\n")?;
      std::process::exit(1);
    }
  }

  Ok(true)
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt::init();

  let args: Vec<String> = env::args().skip(1).collect();
  if let Ok(true) = run_snapshot_mode(&args) {
    return;
  }

  let app = build_app();
  let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
  axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
  #![allow(dead_code, non_snake_case)]
  use super::*;
  use axum::body;
  use axum::body::Body;
  use axum::http::Request;
  use rmp_serde::{from_slice, to_vec};
  use tower::ServiceExt;

  #[tokio::test]
  async fn handle_post_compile_succeeds() {
    let MsgPack(res) = handle_post_compile(MsgPack(PostCompileReq {
      source: "let x = 1; let y = x + 2; y;".to_string(),
      is_global: false,
    }))
    .await
    .expect("compile should succeed");

    let res = res.into_v1();
    assert!(res.symbols.is_some(), "symbols should be present");
    assert!(!res.top_level.debug.steps.is_empty(), "debug steps should be present");
  }

  #[tokio::test]
  async fn symbols_output_is_deterministic() {
    let req = PostCompileReq {
      source: r#"
        let x = 1;
        {
          let y = x + 1;
          y + x;
        }
        let z = x + 3;
        z + x;
      "#
      .to_string(),
      is_global: false,
    };

    let MsgPack(first) = handle_post_compile(MsgPack(req.clone()))
      .await
      .expect("first compile");
    let MsgPack(second) = handle_post_compile(MsgPack(req))
      .await
      .expect("second compile");

    assert_eq!(
      serde_json::to_string(&first).expect("serialize first symbols"),
      serde_json::to_string(&second).expect("serialize second symbols"),
      "symbol output should be deterministic"
    );
  }

  fn build_http_request(body: Vec<u8>) -> Request<Body> {
    Request::builder()
      .uri("/compile")
      .method("POST")
      .header("content-type", "application/msgpack")
      .body(Body::from(body))
      .unwrap()
  }

  #[tokio::test]
  async fn optimizer_output_matches_snapshot_fixture() {
    let app = build_app();
    let body = to_vec(&PostCompileReq {
      source: include_str!("../tests/fixtures/debug_input.js").to_string(),
      is_global: false,
    })
    .expect("serialize request");
    let response = app
      .oneshot(build_http_request(body))
      .await
      .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
      .await
      .expect("read body");
    let parsed: ProgramDump = from_slice(&bytes).expect("decode msgpack body");
    if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
      std::fs::write(
        "tests/fixtures/debug_input.snapshot.json",
        serde_json::to_string_pretty(&parsed).expect("serialize snapshot"),
      )
      .expect("write snapshot");
      return;
    }
    let expected: ProgramDump =
      serde_json::from_str(include_str!("../tests/fixtures/debug_input.snapshot.json"))
        .expect("parse snapshot");
    assert_eq!(
      parsed, expected,
      "debugger response should match recorded snapshot"
    );
  }

  #[tokio::test]
  async fn snapshot_endpoint_is_deterministic_over_http() {
    let app = build_app();
    let req = PostCompileReq {
      source: "let a = 1; const b = a + 1; b;".to_string(),
      is_global: false,
    };
    let body = to_vec(&req).expect("serialize request");
    let first = app
      .clone()
      .oneshot(build_http_request(body.clone()))
      .await
      .expect("first response");
    let second = app
      .oneshot(build_http_request(body))
      .await
      .expect("second response");

    let first_parsed: ProgramDump =
      from_slice(&body::to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let second_parsed: ProgramDump = from_slice(
      &body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(first_parsed, second_parsed);
  }
}
