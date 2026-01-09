use fastrender::{
  BrowserDocument2, BrowserDocumentDom2, FastRender, FontConfig, RenderOptions, Result,
};
use std::cell::RefCell;
use std::rc::Rc;

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
    let node_id = dom.get_element_by_id("box").expect("expected #box element");
    dom
      .set_attribute(node_id, "class", "b")
      .expect("set_attribute should succeed")
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

#[test]
fn browser_document_dom2_rerenders_after_js_dom_mutation() -> Result<()> {
  let options = RenderOptions::new().with_viewport(64, 64);

  let renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()?;

  let doc = BrowserDocumentDom2::new(
    renderer,
    "<!doctype html><html><body><div>Hello</div></body></html>",
    options,
  )?;
  let doc = Rc::new(RefCell::new(doc));

  // First render clears dirty flags.
  assert!(doc.borrow_mut().render_if_needed()?.is_some());
  assert!(doc.borrow_mut().render_if_needed()?.is_none());

  // Install a minimal JS shim for `document.documentElement.classList.add(token)` that mutates the
  // live `dom2::Document` via the `DomHost` interface.
  let rt = rquickjs::Runtime::new().expect("js runtime");
  let ctx = rquickjs::Context::full(&rt).expect("js context");

  let element_id = fastrender::js::dom2_bindings::document_element(&*doc.borrow())
    .expect("documentElement should exist");

  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      let globals = ctx.globals();

      let document = rquickjs::Object::new(ctx.clone())?;
      let document_element = rquickjs::Object::new(ctx.clone())?;
      let class_list = rquickjs::Object::new(ctx.clone())?;

      let host = Rc::clone(&doc);
      let add = rquickjs::Function::new(ctx.clone(), move |token: String| {
        let mut host = host.borrow_mut();
        fastrender::js::dom2_bindings::class_list_add(&mut *host, element_id, &token)
          .expect("classList.add should succeed");
      })?;

      class_list.set("add", add)?;
      document_element.set("classList", class_list)?;
      document.set("documentElement", document_element)?;
      globals.set("document", document)?;
      Ok(())
    })
    .expect("install bindings");

  // Mutation should mark the document dirty and trigger a rerender.
  ctx
    .with(|ctx| ctx.eval::<(), _>("document.documentElement.classList.add('x')"))
    .expect("js eval");
  assert!(doc.borrow_mut().render_if_needed()?.is_some());
  assert!(doc.borrow_mut().render_if_needed()?.is_none());

  // No-op mutation should not dirty the document.
  ctx
    .with(|ctx| ctx.eval::<(), _>("document.documentElement.classList.add('x')"))
    .expect("js eval");
  assert!(doc.borrow_mut().render_if_needed()?.is_none());

  Ok(())
}
