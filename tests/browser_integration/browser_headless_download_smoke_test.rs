#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

use tempfile::tempdir;
use url::Url;

#[test]
fn browser_headless_download_smoke_mode_downloads_into_configured_directory() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site_dir = tempdir().expect("temp site dir");
  let download_dir = tempdir().expect("temp download dir");
  let session_dir = tempdir().expect("temp session dir");
  let session_path = session_dir.path().join("session.json");
  let trace_path = session_dir.path().join("browser_trace.json");

  let payload = b"download smoke payload\n";
  let payload_path = site_dir.path().join("payload.bin");
  std::fs::write(&payload_path, payload).expect("write payload");

  let download_name = "downloaded.bin";
  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          a {{ position: absolute; left: 0; top: 0; width: 200px; height: 40px; background: rgb(255, 0, 0); }}
        </style>
      </head>
      <body>
        <a id="dl" download="{download_name}" href="payload.bin">download</a>
      </body>
    </html>
  "#
  );
  let page_path = site_dir.path().join("page.html");
  std::fs::write(&page_path, html).expect("write page");

  let page_url = Url::from_file_path(&page_path)
    .unwrap_or_else(|()| panic!("failed to build file:// url for {}", page_path.display()))
    .to_string();

  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--headless-download-smoke")
    .arg("--download-dir")
    .arg(download_dir.path())
    .arg("--trace-out")
    .arg(&trace_path)
    .arg(page_url)
    // Avoid inherited env vars overriding the requested path or changing trace retention behavior.
    .env_remove("FASTR_BROWSER_TRACE_OUT")
    .env_remove("FASTR_PERF_TRACE_OUT")
    .env_remove("FASTR_TRACE_MAX_EVENTS")
    // Keep the smoke test cheap/deterministic even if the parent environment has a larger Rayon
    // pool configured.
    .env("RAYON_NUM_THREADS", "1")
    // Avoid touching the developer's real session file when running locally.
    .env("FASTR_BROWSER_SESSION_PATH", &session_path)
    .output()
    .expect("spawn browser");

  assert!(
    output.status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("HEADLESS_DOWNLOAD_SMOKE_OK"),
    "expected headless download smoke success marker, got stdout:\n{stdout}"
  );

  let downloaded_path = download_dir.path().join(download_name);
  assert!(
    downloaded_path.exists(),
    "expected downloaded file to exist at {}, but it does not\nstdout:\n{stdout}",
    downloaded_path.display()
  );

  let downloaded = std::fs::read(&downloaded_path).expect("read downloaded file");
  assert_eq!(downloaded, payload, "downloaded bytes mismatch");

  let part_path = download_dir.path().join(format!("{download_name}.part"));
  assert!(
    !part_path.exists(),
    "expected no .part file after completion, but {} exists",
    part_path.display()
  );

  let raw = std::fs::read_to_string(&trace_path)
    .unwrap_or_else(|err| panic!("expected trace file at {}: {err}", trace_path.display()));
  let parsed: serde_json::Value =
    serde_json::from_str(&raw).expect("trace JSON should be parseable");
  assert!(
    parsed.get("traceEvents").is_some(),
    "expected traceEvents key, got: {parsed}"
  );
}
