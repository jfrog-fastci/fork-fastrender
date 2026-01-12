use fastrender::{FastRender, InspectQuery};
use std::fs;
use url::Url;

#[test]
fn missing_image_placeholder_does_not_scale_to_width_via_1x1_ratio() {
  // Offline fixtures substitute missing image bytes with a deterministic 1×1 PNG placeholder.
  // That placeholder must *not* be treated as a real intrinsic size for replaced layout, otherwise
  // `width: 100%` + `height: auto` produces a huge square box.
  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(|| {
      let dir = tempfile::tempdir().expect("tempdir");
      let assets_dir = dir.path().join("assets");
      fs::create_dir_all(&assets_dir).expect("create assets dir");
      fs::write(assets_dir.join("missing.bin"), b"").expect("write empty image");

      let html = r#"
        <!doctype html>
        <html>
          <head>
            <style>
              body { margin: 0; }
              #wrap { width: 400px; }
              img { display: block; width: 100%; height: auto; font-size: 16px; line-height: 16px; }
            </style>
          </head>
          <body>
            <div id="wrap">
              <img id="img" src="assets/missing.bin" alt="ALT" />
            </div>
          </body>
        </html>
      "#;

      let index_path = dir.path().join("index.html");
      fs::write(&index_path, html).expect("write html");

      let mut renderer = FastRender::new().expect("renderer");
      renderer.set_base_url(Url::from_file_path(&index_path).unwrap().to_string());
      let dom = renderer.parse_html(html).expect("parse");

      let results = renderer
        .inspect(&dom, 800, 600, InspectQuery::Id("img".to_string()))
        .expect("inspect");
      assert_eq!(results.len(), 1);

      let snapshot = &results[0];
      let replaced = snapshot
        .fragments
        .iter()
        .find(|f| f.kind == "replaced")
        .expect("replaced fragment");

      assert!(
        (replaced.bounds.width - 400.0).abs() < 0.1,
        "expected width to resolve from percentage: got {}",
        replaced.bounds.width
      );
      assert!(
        replaced.bounds.height > 0.0 && replaced.bounds.height < 50.0,
        "expected missing-image placeholder to avoid scaling a 1×1 intrinsic ratio to full width: got {}",
        replaced.bounds.height
      );
    })
    .expect("spawn")
    .join()
    .expect("join");
}
