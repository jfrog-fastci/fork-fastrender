use std::env;

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt::init();

  let args: Vec<String> = env::args().skip(1).collect();
  if let Ok(true) = optimize_js_debugger::run_snapshot_mode(&args) {
    return;
  }

  let app = optimize_js_debugger::build_app();
  let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
  axum::serve(listener, app).await.unwrap();
}
