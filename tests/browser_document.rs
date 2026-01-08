use fastrender::{BrowserDocument, FastRender, RenderOptions, Result};
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::dom_mutation;

#[test]
fn browser_document_rerenders_after_dom_mutation() -> Result<()> {
  let options = RenderOptions::new().with_viewport(64, 64);
  let html_a = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="a"></div>
      </body>
    </html>
  "#;
  let html_b = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="b"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new()?;
  let baseline_a = renderer.render_html_with_options(html_a, options.clone())?;
  let baseline_b = renderer.render_html_with_options(html_b, options.clone())?;

  let mut doc = BrowserDocument::from_html(html_a, options)?;
  let frame1 = doc.render_frame()?;
  assert_eq!(
    frame1.data(),
    baseline_a.data(),
    "first BrowserDocument frame should match render_html_with_options output"
  );

  let changed = doc.mutate_dom(|dom| {
    let mut index = DomIndex::build(dom);
    let node_id = *index
      .id_by_element_id
      .get("box")
      .expect("expected #box element");
    index
      .with_node_mut(node_id, |node| dom_mutation::set_attr(node, "class", "b"))
      .unwrap_or(false)
  });
  assert!(changed, "expected class mutation to report a change");

  let frame2 = doc
    .render_if_needed()?
    .expect("expected BrowserDocument to produce a new frame after mutation");
  assert_ne!(frame2.data(), frame1.data(), "expected pixmap to change");
  assert_eq!(
    frame2.data(),
    baseline_b.data(),
    "mutated BrowserDocument frame should match baseline B"
  );

  assert!(
    doc.render_if_needed()?.is_none(),
    "expected render_if_needed() to return None when nothing changed"
  );

  Ok(())
}

