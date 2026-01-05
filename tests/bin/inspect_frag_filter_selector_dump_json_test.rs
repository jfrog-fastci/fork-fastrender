use std::process::Command;

#[test]
fn inspect_frag_filter_selector_dump_json_does_not_panic() {
  let tmp = tempfile::tempdir().expect("temp dir");
  let html_path = tmp.path().join("page.html");
  std::fs::write(
    &html_path,
    "<!doctype html><html><body><div class=\"target\">hello</div></body></html>",
  )
  .expect("write html");

  let dump_dir = tmp.path().join("dump");
  let output = Command::new(env!("CARGO_BIN_EXE_inspect_frag"))
    .arg(&html_path)
    .args(["--filter-selector", ".target", "--dump-json"])
    .arg(&dump_dir)
    .output()
    .expect("run inspect_frag");

  assert!(
    output.status.success(),
    "inspect_frag should succeed with --filter-selector + --dump-json. stderr={}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    dump_dir.join("dom.json").is_file(),
    "expected inspect_frag to write dom.json into dump directory"
  );
}

