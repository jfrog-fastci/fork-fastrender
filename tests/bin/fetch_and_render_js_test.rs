use std::fs;
use std::path::Path;
use std::process::Command;

fn render_pixel(url: &str, out_path: &Path, js: bool) -> image::Rgba<u8> {
  let mut cmd = Command::new(env!("CARGO_BIN_EXE_fetch_and_render"));
  if js {
    cmd.arg("--js");
  }
  let status = cmd
    .arg(url)
    .arg(out_path)
    .args(["--viewport", "64x64"])
    .status()
    .expect("run fetch_and_render");
  assert!(status.success(), "fetch_and_render should exit successfully");

  image::open(out_path)
    .expect("open rendered image")
    .into_rgba8()
    .get_pixel(0, 0)
    .to_owned()
}

fn assert_red(pixel: image::Rgba<u8>, msg: &str) {
  assert!(pixel.0[0] > 200 && pixel.0[1] < 80, "{msg}");
}

fn assert_green(pixel: image::Rgba<u8>, msg: &str) {
  assert!(pixel.0[1] > 200 && pixel.0[0] < 80, "{msg}");
}

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

  let url = url::Url::from_file_path(&html_path).unwrap().to_string();
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

#[test]
fn js_flag_executes_external_script_src_and_mutates_dom() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let html_path = tmp.path().join("page.html");
  let script_path = tmp.path().join("script.js");

  fs::write(
    &script_path,
    r#"document.documentElement.className = 'js-enabled';"#,
  )
  .expect("write script fixture");

  fs::write(
    &html_path,
    r#"<!doctype html><html class="no-js"><head><style>
html, body { margin: 0; width: 100%; height: 100%; }
html.no-js body { background: rgb(255, 0, 0); }
html.js-enabled body { background: rgb(0, 255, 0); }
</style>
<script src="script.js"></script>
</head><body></body></html>"#,
  )
  .expect("write html fixture");

  let url = url::Url::from_file_path(&html_path).unwrap().to_string();
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

  let output = Command::new(env!("CARGO_BIN_EXE_fetch_and_render"))
    .args(["--js", &url, js_png.to_str().unwrap()])
    .args(["--viewport", "64x64"])
    .output()
    .expect("run fetch_and_render --js");
  assert!(
    output.status.success(),
    "JS render should exit successfully with --js"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    !stdout.contains("skipping external <script src"),
    "fetch_and_render should not skip external <script src> in --js mode; stdout={stdout}"
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
    "JS run should flip to the green background from external script"
  );
}

#[test]
fn js_flag_executes_module_script_and_mutates_dom() {
  let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pages/fixtures/module_simple");
  let html_path = fixture_dir.join("index.html");
  assert!(html_path.exists(), "fixture missing: {}", html_path.display());

  let tmp = tempfile::TempDir::new().expect("tempdir");
  let url = url::Url::from_file_path(&html_path).unwrap().to_string();
  let no_js_png = tmp.path().join("no_js.png");
  let js_png = tmp.path().join("js.png");

  let no_js_pixel = render_pixel(&url, &no_js_png, /* js */ false);
  let js_pixel = render_pixel(&url, &js_png, /* js */ true);

  assert_red(no_js_pixel, "baseline run should keep the red background from html.no-js");
  assert_green(js_pixel, "JS run should flip to the green background from html.js-enabled");
}

#[test]
fn js_flag_executes_external_scripts_and_respects_defer_and_async() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let html_path = tmp.path().join("page.html");
  fs::write(
    tmp.path().join("blocking.js"),
    r#"document.documentElement.setAttribute('data-a', '1');"#,
  )
  .expect("write blocking.js");
  fs::write(
    tmp.path().join("async.js"),
    r#"document.documentElement.setAttribute('data-async', '1');"#,
  )
  .expect("write async.js");
  fs::write(
    tmp.path().join("defer1.js"),
    r#"if (document.documentElement.getAttribute('data-c') === '1' && document.getElementById('tail')) {
  document.documentElement.setAttribute('data-defer1', '1');
}"#,
  )
  .expect("write defer1.js");
  fs::write(
    tmp.path().join("defer2.js"),
    r#"if (document.documentElement.getAttribute('data-defer1') === '1' && document.getElementById('tail')) {
  document.documentElement.setAttribute('data-defer2', '1');
}"#,
  )
  .expect("write defer2.js");

  fs::write(
    &html_path,
    r#"<!doctype html><html><head><style>
html, body { margin: 0; width: 100%; height: 100%; }
body { background: rgb(255, 0, 0); }
html[data-a="1"][data-b="1"][data-c="1"][data-defer1="1"][data-defer2="1"][data-async="1"] body { background: rgb(0, 255, 0); }
</style>
<script src="blocking.js"></script>
<script>
  if (document.documentElement.getAttribute('data-a') === '1') {
    document.documentElement.setAttribute('data-b', '1');
  }
</script>
<script async src="async.js"></script>
<script defer src="defer1.js"></script>
<script defer src="defer2.js"></script>
</head><body>
<script>
  if (document.documentElement.getAttribute('data-b') === '1') {
    document.documentElement.setAttribute('data-c', '1');
  }
</script>
<div id="tail"></div>
</body></html>"#,
  )
  .expect("write html fixture");

  let url = format!("file://{}", html_path.display());
  let no_js_png = tmp.path().join("no_js.png");
  let js_png = tmp.path().join("js.png");

  let no_js_pixel = render_pixel(&url, &no_js_png, /* js */ false);
  let js_pixel = render_pixel(&url, &js_png, /* js */ true);

  assert_red(no_js_pixel, "baseline run should not execute external scripts");
  assert_green(js_pixel, "JS run should execute external/async/defer scripts");
}

#[test]
fn js_flag_resolves_script_src_using_base_href_timing() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let html_path = tmp.path().join("page.html");
  let sub_dir = tmp.path().join("sub");
  fs::create_dir_all(&sub_dir).expect("create sub dir");

  // Script before <base href>: should resolve relative to the document URL.
  fs::write(tmp.path().join("a.js"), "document.documentElement.setAttribute('data-a', 'root');")
    .expect("write a.js");
  // If base URL timing is wrong, the first script might resolve to sub/a.js instead.
  fs::write(
    sub_dir.join("a.js"),
    "document.documentElement.setAttribute('data-a', 'sub');",
  )
  .expect("write sub/a.js");

  // Script after <base href>: should resolve relative to the new base URL.
  fs::write(tmp.path().join("b.js"), "document.documentElement.setAttribute('data-b', 'root');")
    .expect("write b.js");
  fs::write(
    sub_dir.join("b.js"),
    "document.documentElement.setAttribute('data-b', 'sub');",
  )
  .expect("write sub/b.js");

  fs::write(
    &html_path,
    r#"<!doctype html><html><head><style>
html, body { margin: 0; width: 100%; height: 100%; }
body { background: rgb(255, 0, 0); }
html[data-a="root"][data-b="sub"] body { background: rgb(0, 255, 0); }
</style>
<script src="a.js"></script>
<base href="sub/">
<script src="b.js"></script>
</head><body></body></html>"#,
  )
  .expect("write html fixture");

  let url = format!("file://{}", html_path.display());
  let no_js_png = tmp.path().join("no_js.png");
  let js_png = tmp.path().join("js.png");

  let no_js_pixel = render_pixel(&url, &no_js_png, /* js */ false);
  let js_pixel = render_pixel(&url, &js_png, /* js */ true);

  assert_red(no_js_pixel, "baseline run should not execute scripts");
  assert_green(js_pixel, "JS run should resolve script src with correct base URL timing");
}
