use fastrender::dom2::NodeKind;
use fastrender::{BrowserDocument2, FastRender, RenderOptions, Result};

#[test]
fn browser_document2_rerenders_after_dom_mutation() -> Result<()> {
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

  let mut doc = BrowserDocument2::from_html(html_a, options)?;
  let frame1 = doc.render_frame()?;
  assert_eq!(
    frame1.data(),
    baseline_a.data(),
    "first BrowserDocument2 frame should match render_html_with_options output"
  );

  let changed = doc.mutate_dom(|dom| {
    let node_id = dom
      .subtree_preorder(dom.root())
      .find(|&id| match &dom.node(id).kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
          .iter()
          .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == "box"),
        _ => false,
      })
      .expect("expected #box element");

    let attrs = match &mut dom.node_mut(node_id).kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
      _ => panic!("expected #box to be an element"),
    };
    if let Some((_, class)) = attrs.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case("class")) {
      if class == "b" {
        false
      } else {
        class.clear();
        class.push_str("b");
        true
      }
    } else {
      attrs.push(("class".to_string(), "b".to_string()));
      true
    }
  });
  assert!(changed, "expected class mutation to report a change");

  let frame2 = doc
    .render_if_needed()?
    .expect("expected BrowserDocument2 to produce a new frame after mutation");
  assert_ne!(frame2.data(), frame1.data(), "expected pixmap to change");
  assert_eq!(
    frame2.data(),
    baseline_b.data(),
    "mutated BrowserDocument2 frame should match baseline B"
  );

  assert!(
    doc.render_if_needed()?.is_none(),
    "expected render_if_needed() to return None when nothing changed"
  );

  Ok(())
}
