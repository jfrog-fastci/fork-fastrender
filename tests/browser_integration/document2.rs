use fastrender::render_control::{push_stage_listener, StageHeartbeat};
use fastrender::{
  BrowserDocument2, BrowserDocumentDom2, FastRender, FontConfig, RenderOptions, Result,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use super::support;

fn capture_stages<T>(f: impl FnOnce() -> Result<T>) -> Result<Vec<StageHeartbeat>> {
  let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
  let stages_for_listener = Arc::clone(&stages);
  let _guard = push_stage_listener(Some(Arc::new(move |stage| {
    stages_for_listener
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(stage);
  })));
  let _ = f()?;
  let captured = stages
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone();
  Ok(captured)
}

#[test]
fn browser_document2_rerenders_after_dom_mutation() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
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

  let mut renderer = support::deterministic_renderer();
  let baseline_a = renderer.render_html_with_options(html_a, options.clone())?;
  let baseline_b = renderer.render_html_with_options(html_b, options.clone())?;

  let mut doc = BrowserDocument2::new(support::deterministic_renderer(), html_a, options)?;
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

#[cfg(feature = "quickjs")]
#[test]
fn browser_document_dom2_rerenders_after_js_dom_mutation() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
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

#[test]
fn browser_document_dom2_dom2_bindings_query_selector_and_attribute_mutations() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);
  let renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()?;
  let mut doc = BrowserDocumentDom2::new(
    renderer,
    "<!doctype html><html><body><div id=\"box\" class=\"a b\">Hello</div></body></html>",
    options,
  )?;

  // First render clears dirty flags.
  assert!(doc.render_if_needed()?.is_some());
  assert!(doc.render_if_needed()?.is_none());

  // querySelector is a read-only operation and should not mark the document dirty.
  let box_id = fastrender::js::dom2_bindings::query_selector(&mut doc, "#box", None)
    .expect("querySelector should succeed")
    .expect("expected #box to exist");
  assert!(doc.render_if_needed()?.is_none());

  // classList.replace should dirty only when it changes the underlying `class` attribute.
  // - If `newToken` already exists, it just removes the old one.
  let found = fastrender::js::dom2_bindings::class_list_replace(&mut doc, box_id, "a", "b")
    .expect("classList.replace should succeed");
  assert!(found);
  assert!(doc.render_if_needed()?.is_some());
  assert!(doc.render_if_needed()?.is_none());

  // Token not present => return false + no dirty.
  let found = fastrender::js::dom2_bindings::class_list_replace(&mut doc, box_id, "a", "c")
    .expect("classList.replace should succeed");
  assert!(!found);
  assert!(doc.render_if_needed()?.is_none());

  // Token present but token == newToken => return true + no dirty.
  let found = fastrender::js::dom2_bindings::class_list_replace(&mut doc, box_id, "b", "b")
    .expect("classList.replace should succeed");
  assert!(found);
  assert!(doc.render_if_needed()?.is_none());

  // setAttribute should dirty only when it changes the underlying attribute value.
  let changed = fastrender::js::dom2_bindings::set_attribute(&mut doc, box_id, "data-x", "1")
    .expect("setAttribute should succeed");
  assert!(changed);
  assert!(doc.render_if_needed()?.is_some());
  assert!(doc.render_if_needed()?.is_none());

  let changed = fastrender::js::dom2_bindings::set_attribute(&mut doc, box_id, "data-x", "1")
    .expect("setAttribute should succeed");
  assert!(!changed);
  assert!(doc.render_if_needed()?.is_none());

  // removeAttribute should dirty only when the attribute existed.
  let changed = fastrender::js::dom2_bindings::remove_attribute(&mut doc, box_id, "data-x")
    .expect("removeAttribute should succeed");
  assert!(changed);
  assert!(doc.render_if_needed()?.is_some());
  assert!(doc.render_if_needed()?.is_none());

  let changed = fastrender::js::dom2_bindings::remove_attribute(&mut doc, box_id, "data-x")
    .expect("removeAttribute should succeed");
  assert!(!changed);
  assert!(doc.render_if_needed()?.is_none());

  Ok(())
}

#[test]
fn browser_document_dom2_text_mutation_skips_full_restyle() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 32);

  let html_a = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: white; color: black; }
          #box { font-size: 16px; font-family: "Noto Sans"; }
        </style>
      </head>
      <body>
        <div id="box">Hello</div>
      </body>
    </html>
  "#;
  let html_b = html_a.replace(">Hello<", ">Goodbye<");

  let mut renderer = support::deterministic_renderer();
  let baseline_a = renderer.render_html_with_options(html_a, options.clone())?;
  let baseline_b = renderer.render_html_with_options(&html_b, options.clone())?;

  let mut doc = BrowserDocumentDom2::new(support::deterministic_renderer(), html_a, options)?;
  let frame0 = doc.render_frame()?;
  assert_eq!(frame0.data(), baseline_a.data());

  let before = doc.invalidation_counters();

  let changed = doc.mutate_dom(|dom| {
    let box_id = dom.get_element_by_id("box").expect("#box element");
    let text_id = dom
      .children(box_id)
      .expect("#box children")
      .first()
      .copied()
      .expect("#box text child");
    dom
      .set_text_data(text_id, "Goodbye")
      .expect("set_text_data")
  });
  assert!(changed);

  let mut frame1: Option<fastrender::Pixmap> = None;
  let stages = capture_stages(|| {
    frame1 = doc.render_if_needed()?;
    Ok(())
  })?;
  let frame1 = frame1.expect("expected rerender after text mutation");
  assert_eq!(frame1.data(), baseline_b.data());

  assert!(
    stages.contains(&StageHeartbeat::Layout),
    "expected layout stage after text mutation; got {stages:?}"
  );
  assert!(
    !stages.contains(&StageHeartbeat::Cascade),
    "expected no cascade stage after text mutation; got {stages:?}"
  );
  assert!(
    !stages.contains(&StageHeartbeat::DomParse),
    "expected no dom_parse stage after text mutation; got {stages:?}"
  );

  let after = doc.invalidation_counters();
  assert_eq!(after.full_restyles, before.full_restyles);
  assert_eq!(after.full_relayouts, before.full_relayouts);
  assert_eq!(
    after.incremental_relayouts,
    before.incremental_relayouts + 1
  );

  // A successful incremental relayout should also satisfy generation-based dirty detection; we
  // should not need a follow-up full pipeline run to "clear" the dirty state.
  let mut frame2: Option<fastrender::Pixmap> = None;
  let stages2 = capture_stages(|| {
    frame2 = doc.render_if_needed()?;
    Ok(())
  })?;
  assert!(
    frame2.is_none(),
    "expected no rerender after dirty flags cleared"
  );
  assert!(
    stages2.is_empty(),
    "expected no pipeline stages once document is clean; got {stages2:?}"
  );

  Ok(())
}

#[test]
fn browser_document_dom2_attribute_mutation_triggers_restyle() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
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
  let html_b = html_a.replace("class=\"a\"", "class=\"b\"");

  let mut renderer = support::deterministic_renderer();
  let baseline_a = renderer.render_html_with_options(html_a, options.clone())?;
  let baseline_b = renderer.render_html_with_options(&html_b, options.clone())?;

  let mut doc = BrowserDocumentDom2::new(support::deterministic_renderer(), html_a, options)?;
  let frame0 = doc.render_frame()?;
  assert_eq!(frame0.data(), baseline_a.data());

  let before = doc.invalidation_counters();

  let changed = doc.mutate_dom(|dom| {
    let box_id = dom.get_element_by_id("box").expect("#box element");
    dom
      .set_attribute(box_id, "class", "b")
      .expect("set_attribute")
  });
  assert!(changed);

  let mut frame1: Option<fastrender::Pixmap> = None;
  let stages = capture_stages(|| {
    frame1 = doc.render_if_needed()?;
    Ok(())
  })?;
  let frame1 = frame1.expect("expected rerender after attribute mutation");
  assert_eq!(frame1.data(), baseline_b.data());

  assert!(
    stages.contains(&StageHeartbeat::Cascade),
    "expected cascade stage after attribute mutation; got {stages:?}"
  );

  let after = doc.invalidation_counters();
  assert_eq!(after.full_restyles, before.full_restyles + 1);
  assert_eq!(after.full_relayouts, before.full_relayouts + 1);
  assert_eq!(after.incremental_relayouts, before.incremental_relayouts);

  Ok(())
}

#[test]
fn browser_document_dom2_insert_remove_triggers_recompute() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  let html_empty = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
        </style>
      </head>
      <body></body>
    </html>
  "#;
  let html_with_box = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
        </style>
      </head>
      <body><div id="box"></div></body>
    </html>
  "#;

  let mut renderer = support::deterministic_renderer();
  let baseline_empty = renderer.render_html_with_options(html_empty, options.clone())?;
  let baseline_with_box = renderer.render_html_with_options(html_with_box, options.clone())?;

  let mut doc = BrowserDocumentDom2::new(support::deterministic_renderer(), html_empty, options)?;
  let frame0 = doc.render_frame()?;
  assert_eq!(frame0.data(), baseline_empty.data());

  let body = doc.dom().body().expect("body element");
  let box_node = doc.dom_mut().create_element("div", "");
  doc.render_frame()?; // clear unconditional invalidation from dom_mut() above

  doc.mutate_dom(|dom| {
    dom
      .set_attribute(box_node, "id", "box")
      .expect("set_attribute");
    dom.append_child(body, box_node).expect("append_child")
  });

  let mut inserted: Option<fastrender::Pixmap> = None;
  let stages_insert = capture_stages(|| {
    inserted = doc.render_if_needed()?;
    Ok(())
  })?;
  let inserted = inserted.expect("expected rerender after insertion");
  assert_eq!(inserted.data(), baseline_with_box.data());
  assert!(
    stages_insert.contains(&StageHeartbeat::Cascade),
    "expected cascade stage after insertion; got {stages_insert:?}"
  );

  doc.mutate_dom(|dom| dom.remove_child(body, box_node).expect("remove_child"));
  let mut removed: Option<fastrender::Pixmap> = None;
  let stages_remove = capture_stages(|| {
    removed = doc.render_if_needed()?;
    Ok(())
  })?;
  let removed = removed.expect("expected rerender after removal");
  assert_eq!(removed.data(), baseline_empty.data());
  assert!(
    stages_remove.contains(&StageHeartbeat::Cascade),
    "expected cascade stage after removal; got {stages_remove:?}"
  );

  Ok(())
}
