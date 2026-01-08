use std::fs;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn fetch_and_render_soft_timeout_exits_with_stage_message() {
  let temp = TempDir::new().expect("tempdir");
  let html_path = temp.path().join("soft_timeout.html");
  // Make DOM parsing reliably slower than the configured soft timeout so we deterministically
  // time out in the `dom_parse` stage without relying on the unit-test-only
  // `FASTR_TEST_RENDER_DELAY_MS` hook (which does not apply to CLI binaries).
  let mut body = String::new();
  for _ in 0..200_000 {
    body.push_str("<div>slow</div>");
  }
  fs::write(
    &html_path,
    format!("<!doctype html><title>Soft timeout</title><body>{body}</body>"),
  )
  .expect("write html");

  let output_path = temp.path().join("out.png");
  let url = format!("file://{}", html_path.display());

  let output = Command::new(env!("CARGO_BIN_EXE_fetch_and_render"))
    .args([
      "--timeout",
      "2",
      "--soft-timeout-ms",
      "10",
      &url,
      output_path.to_str().expect("output path"),
    ])
    .output()
    .expect("run fetch_and_render");

  assert!(
    !output.status.success(),
    "expected non-zero exit status, got {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
    status = output.status,
    stdout = String::from_utf8_lossy(&output.stdout),
    stderr = String::from_utf8_lossy(&output.stderr)
  );

  let combined = format!(
    "{}\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    combined.contains("Rendering timed out during"),
    "expected cooperative timeout message, got:\n{combined}"
  );
  assert!(
    combined.contains("dom_parse"),
    "expected timeout to be attributed to dom_parse, got:\n{combined}"
  );
  assert!(
    !combined.contains("Render timed out after"),
    "expected cooperative timeout (not hard-kill timeout), got:\n{combined}"
  );
}

