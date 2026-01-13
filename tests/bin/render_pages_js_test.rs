use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn pixel_at_origin(path: &std::path::Path) -> image::Rgba<u8> {
  image::open(path)
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
fn render_pages_js_flag_executes_inline_script_and_mutates_dom() {
  let temp = TempDir::new().expect("tempdir");
  let html_dir = temp.path().join("fetches/html");
  fs::create_dir_all(&html_dir).expect("create html dir");

  fs::write(
    html_dir.join("example.com.html"),
    r#"<!doctype html><html class="no-js"><head><style>
html, body { margin: 0; width: 100%; height: 100%; }
html.no-js body { background: rgb(255, 0, 0); }
html.js-enabled body { background: rgb(0, 255, 0); }
</style>
<script>document.documentElement.className = 'js-enabled';</script>
</head><body></body></html>"#,
  )
  .expect("write html");

  let out_no_js = temp.path().join("out_no_js");
  let out_js = temp.path().join("out_js");

  let status = Command::new(env!("CARGO_BIN_EXE_render_pages"))
    .current_dir(temp.path())
    .args(["--pages", "example.com", "--jobs", "1", "--viewport", "64x64"])
    .arg("--out-dir")
    .arg(&out_no_js)
    .status()
    .expect("run render_pages (no js)");
  assert!(status.success(), "baseline render_pages should succeed");

  let status = Command::new(env!("CARGO_BIN_EXE_render_pages"))
    .current_dir(temp.path())
    .args([
      "--pages",
      "example.com",
      "--jobs",
      "1",
      "--viewport",
      "64x64",
      "--js",
      "--js-max-frames",
      "10",
    ])
    .arg("--out-dir")
    .arg(&out_js)
    .status()
    .expect("run render_pages --js");
  assert!(status.success(), "JS render_pages should succeed");

  let no_js_pixel = pixel_at_origin(&out_no_js.join("example.com.png"));
  let js_pixel = pixel_at_origin(&out_js.join("example.com.png"));

  assert_red(no_js_pixel, "baseline render should not execute scripts");
  assert_green(js_pixel, "JS render should apply DOM mutation from inline script");
}

