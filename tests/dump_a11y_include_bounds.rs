use std::io::Write;
use std::process::Command;

#[test]
fn dump_a11y_include_bounds_emits_bounds_css() {
  let mut file = tempfile::NamedTempFile::new().expect("temp html file");
  write!(
    file,
    "<!doctype html><html><head><style>html,body{{margin:0;padding:0;}}</style></head><body><button style=\"width:10px;height:12px;\">Hi</button></body></html>"
  )
  .expect("write html");

  let output = Command::new(env!("CARGO_BIN_EXE_dump_a11y"))
    .arg(file.path())
    .arg("--include-bounds")
    .arg("--compact")
    .arg("--viewport")
    .arg("100x100")
    .output()
    .expect("run dump_a11y");

  assert!(
    output.status.success(),
    "dump_a11y exited with {:?}\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let value: serde_json::Value =
    serde_json::from_slice(&output.stdout).expect("dump_a11y output should be valid JSON");
  let bounds = value
    .get("bounds_css")
    .and_then(|v| v.as_object())
    .expect("expected bounds_css map in output");

  let mut found = false;
  for (_node_id, rect) in bounds {
    let Some(obj) = rect.as_object() else {
      continue;
    };
    let Some(x) = obj.get("x").and_then(|v| v.as_f64()) else {
      continue;
    };
    let Some(y) = obj.get("y").and_then(|v| v.as_f64()) else {
      continue;
    };
    let Some(w) = obj.get("width").and_then(|v| v.as_f64()) else {
      continue;
    };
    let Some(h) = obj.get("height").and_then(|v| v.as_f64()) else {
      continue;
    };
    if x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() {
      found = true;
      break;
    }
  }

  assert!(
    found,
    "expected bounds_css to contain at least one entry with finite numbers; got {:?}",
    bounds
  );
}

