use fastrender::api::{BrowserDocumentDom2, RenderOptions};

#[test]
fn dom2_geometry_scrollport_accounts_for_scrollbar_gutter_reservation() {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          #root {
            width: 100px;
            height: 100px;
            overflow: auto;
            scrollbar-gutter: stable;
          }
        </style>
      </head>
      <body>
        <div id="root"></div>
      </body>
    </html>"#;

  let mut document = BrowserDocumentDom2::from_html(html, RenderOptions::default()).expect("document");

  let mut root_node = None;
  document.mutate_dom(|dom| {
    root_node = dom.query_selector("#root", None).expect("query selector");
    false
  });
  let root_node = root_node.expect("#root node id");

  document.render_frame().expect("render frame");

  let geometry = document.geometry_context().expect("geometry context");
  let padding_box = geometry
    .padding_box_in_viewport(root_node)
    .expect("padding box");
  let scrollport_box = geometry
    .scrollport_box_in_viewport(root_node)
    .expect("scrollport box");

  let hide_scrollbars = fastrender::debug::runtime::runtime_toggles().truthy("FASTR_HIDE_SCROLLBARS");
  let expected_gutter = if hide_scrollbars { 0.0 } else { 15.0 };

  let delta_w = padding_box.width() - scrollport_box.width();
  let delta_h = padding_box.height() - scrollport_box.height();
  let epsilon = 0.01;
  assert!(
    (delta_w - expected_gutter).abs() <= epsilon,
    "expected scrollport width to shrink by {expected_gutter}px, got {delta_w}px (padding={padding_box:?}, scrollport={scrollport_box:?})"
  );
  assert!(
    (delta_h - expected_gutter).abs() <= epsilon,
    "expected scrollport height to shrink by {expected_gutter}px, got {delta_h}px (padding={padding_box:?}, scrollport={scrollport_box:?})"
  );
}

