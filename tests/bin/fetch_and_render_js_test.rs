use std::fs;
use std::process::Command;

#[test]
fn js_flag_executes_inline_script_and_mutates_dom() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let html_path = tmp.path().join("page.html");
  fs::write(
    &html_path,
    r#"<!doctype html><html class="no-js"><head><style>
html, body { margin: 0; width: 100%; height: 100%; }
html.no-js body { background: rgb(255, 0, 0); }
html.js-enabled body { background: rgb(0, 255, 0); }
</style>
<script>document.documentElement.className = 'js-enabled';</script>
</head><body></body></html>"#,
  )
  .expect("write html fixture");

  let url = format!("file://{}", html_path.display());
  let no_js_png = tmp.path().join("no_js.png");
  let js_png = tmp.path().join("js.png");

  let status = Command::new(env!("CARGO_BIN_EXE_fetch_and_render"))
    .args([&url, no_js_png.to_str().unwrap()])
    .args(["--viewport", "64x64"])
    .status()
    .expect("run fetch_and_render (no js)");
  assert!(
    status.success(),
    "baseline render should exit successfully without --js"
  );

  let status = Command::new(env!("CARGO_BIN_EXE_fetch_and_render"))
    .args(["--js", &url, js_png.to_str().unwrap()])
    .args(["--viewport", "64x64"])
    .status()
    .expect("run fetch_and_render --js");
  assert!(
    status.success(),
    "JS render should exit successfully with --js"
  );

  let no_js_image = image::open(&no_js_png)
    .expect("open baseline render")
    .into_rgba8();
  let js_image = image::open(&js_png).expect("open JS render").into_rgba8();

  let no_js_pixel = no_js_image.get_pixel(0, 0);
  let js_pixel = js_image.get_pixel(0, 0);

  assert!(
    no_js_pixel.0[0] > 200 && no_js_pixel.0[1] < 80,
    "baseline run should keep the red background from html.no-js"
  );
  assert!(
    js_pixel.0[1] > 200 && js_pixel.0[0] < 80,
    "JS run should flip to the green background from html.js-enabled"
  );
}
