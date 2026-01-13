use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn write_fixture(root: &std::path::Path, stem: &str, index_html: &str) {
  let dir = root.join(stem);
  fs::create_dir_all(&dir).expect("create fixture dir");
  fs::write(dir.join("index.html"), index_html).expect("write index.html");
}

#[test]
fn render_fixtures_js_executes_scripts_and_changes_output_pixels() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  // Use `requestAnimationFrame` so `--js` must actually drive the event loop before the final frame
  // is rendered.
  write_fixture(
    &fixtures_dir,
    "js_color",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; width: 100%; height: 100%; }
      #box { position: fixed; inset: 0; background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <div id="box"></div>
    <script>
      requestAnimationFrame(() => {
        // Use `setProperty` so we exercise FastRender's supported CSSStyleDeclaration surface.
        document.getElementById('box')
          .style
          .setProperty('backgroundColor', 'rgb(0, 255, 0)');
      });
    </script>
  </body>
</html>"#,
  );

  let common_args = [
    "--fixtures-dir",
    fixtures_dir.to_str().unwrap(),
    "--fixtures",
    "js_color",
    "--viewport",
    "16x16",
    "--jobs",
    "1",
    "--timeout",
    "2",
  ];

  let out_no_js = temp.path().join("out_no_js");
  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    // Keep the child process predictable and avoid spinning up a huge global Rayon pool on large
    // CI machines. `render_fixtures` uses its own harness pool for fixture-level concurrency.
    .env("RAYON_NUM_THREADS", "2")
    // Ensure the paint pipeline stays on the global pool for this harness-level test.
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--out-dir",
      out_no_js.to_str().unwrap(),
      // With JS disabled, the page should stay red.
    ])
    .args(common_args)
    .status()
    .expect("run render_fixtures without --js");
  assert!(status.success(), "expected render_fixtures to succeed");

  let img = image::open(out_no_js.join("js_color.png"))
    .expect("decode js_color.png (no js)")
    .into_rgba8();
  let px = img.get_pixel(0, 0).0;
  assert!(
    px[0] > 200 && px[1] < 80,
    "expected red output without --js (got {:?})",
    px
  );

  let out_js = temp.path().join("out_js");
  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    .env("RAYON_NUM_THREADS", "2")
    .env("FASTR_PAINT_THREADS", "1")
    .args(["--out-dir", out_js.to_str().unwrap(), "--js"])
    .args(common_args)
    .status()
    .expect("run render_fixtures with --js");
  assert!(status.success(), "expected render_fixtures --js to succeed");

  let img = image::open(out_js.join("js_color.png"))
    .expect("decode js_color.png (--js)")
    .into_rgba8();
  let px = img.get_pixel(0, 0).0;
  assert!(
    px[1] > 200 && px[0] < 80,
    "expected green output with --js (got {:?})",
    px
  );
}
