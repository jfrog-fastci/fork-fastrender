use fastrender::FastRender;
use tiny_skia::Pixmap;

#[test]
fn inline_svg_renders_with_injected_document_css() {
  // The root-style injected by box generation is not enough to apply this rule; the fill override
  // must come from document-level CSS applied by the SVG renderer.
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      .shape { fill: rgb(255, 0, 0); }
    </style>
    <svg width="20" height="20" viewBox="0 0 20 20">
     <rect class="shape" width="20" height="20" />
    </svg>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap: Pixmap = renderer.render_html(html, 30, 30).expect("render html");
  let idx = (10usize * pixmap.width() as usize + 10usize) * 4;
  let data = pixmap.data();
  assert_eq!(
    [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]],
    [255, 0, 0, 255],
    "document-level CSS should affect inline SVG rendering"
  );
}

