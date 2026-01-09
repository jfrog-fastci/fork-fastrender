use std::fs;
use std::process::Command;

#[test]
fn fetch_and_render_animation_time_flag_changes_output() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let html_path = tmp.path().join("page.html");

  // Keep the render deterministic and ensure the animation affects every pixel so PNG bytes differ.
  let html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 100px;
        background: black;
        animation: fade 1000ms linear both;
      }
      @keyframes fade {
        from { opacity: 0; }
        to { opacity: 1; }
      }
    </style>
  </head>
  <body>
    <div id="box"></div>
  </body>
</html>
"#;
  fs::write(&html_path, html).expect("write html");

  let url = format!("file://{}", html_path.display());
  let out_0 = tmp.path().join("t0.png");
  let out_800 = tmp.path().join("t800.png");

  let status_0 = Command::new(env!("CARGO_BIN_EXE_fetch_and_render"))
    .env("FASTR_DETERMINISTIC_PAINT", "1")
    .args(["--viewport", "100x100"])
    .args(["--animation-time-ms", "0"])
    .arg(&url)
    .arg(&out_0)
    .status()
    .expect("run fetch_and_render at t=0");
  assert!(status_0.success(), "expected success for t=0");
  assert!(out_0.exists(), "expected output file to exist for t=0");

  let status_800 = Command::new(env!("CARGO_BIN_EXE_fetch_and_render"))
    .env("FASTR_DETERMINISTIC_PAINT", "1")
    .args(["--viewport", "100x100"])
    .args(["--animation-time-ms", "800"])
    .arg(&url)
    .arg(&out_800)
    .status()
    .expect("run fetch_and_render at t=800ms");
  assert!(status_800.success(), "expected success for t=800ms");
  assert!(out_800.exists(), "expected output file to exist for t=800ms");

  let png_0 = fs::read(&out_0).expect("read t=0 png");
  let png_800 = fs::read(&out_800).expect("read t=800 png");
  assert_ne!(
    png_0, png_800,
    "expected different PNG output when sampling animation at different timestamps"
  );
}

