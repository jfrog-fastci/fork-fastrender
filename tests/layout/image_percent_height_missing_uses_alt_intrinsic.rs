use fastrender::{FastRender, InspectQuery};

#[test]
fn image_percent_height_with_missing_resource_uses_alt_intrinsic_size() {
  // This regression guards against incorrectly treating percentage-based width/height as "definite"
  // for the purpose of skipping intrinsic sizing.
  //
  // If the percentage height can't resolve (because the containing block's height is indefinite),
  // replaced sizing falls back to the element's intrinsic ratio/height. Without an intrinsic
  // fallback for a missing image, that can incorrectly become CSS2.1's 150px default height.
  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
        <!doctype html>
        <html>
          <head>
            <style>
              body { margin: 0; }
              #wrap { width: 98px; }
              img { display: block; width: 100%; height: 100%; object-fit: cover; }
            </style>
          </head>
          <body>
            <div id="wrap">
              <img id="img" alt="XXXXXXXXXXXXXXXXXXXX" src="data:image/png;base64," />
            </div>
          </body>
        </html>
      "#;
      let dom = renderer.parse_html(html).expect("parse");
      let results = renderer
        .inspect(&dom, 200, 200, InspectQuery::Id("img".to_string()))
        .expect("inspect");
      assert_eq!(results.len(), 1);

      let snapshot = &results[0];
      let replaced = snapshot
        .fragments
        .iter()
        .find(|f| f.kind == "replaced")
        .expect("replaced fragment");

      assert!(
        (replaced.bounds.width - 98.0).abs() < 0.1,
        "expected width to resolve from percentage: got {}",
        replaced.bounds.width
      );
      assert!(
        replaced.bounds.height > 0.0 && replaced.bounds.height < 100.0,
        "expected missing-image fallback to avoid 150px default height: got {}",
        replaced.bounds.height
      );
    })
    .expect("spawn")
    .join()
    .expect("join");
}

