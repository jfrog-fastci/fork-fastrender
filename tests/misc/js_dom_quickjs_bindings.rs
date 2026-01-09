use fastrender::dom::parse_html;
use fastrender::dom2::{Document, NodeId, NodeKind};
use fastrender::js::quickjs_dom::{install_dom2_bindings, SharedDom2Document};
use rquickjs::{Context, Runtime};
use std::cell::RefCell;
use std::rc::Rc;

fn find_first_element(dom: &Document, tag: &str) -> Option<NodeId> {
  let mut stack = vec![dom.root()];
  while let Some(id) = stack.pop() {
    let node = dom.node(id);
    match &node.kind {
      NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case(tag) => return Some(id),
      _ => {}
    }
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_element_by_id(dom: &Document, id_value: &str) -> Option<NodeId> {
  let mut stack = vec![dom.root()];
  while let Some(id) = stack.pop() {
    if dom.id(id).ok().flatten().is_some_and(|v| v == id_value) {
      return Some(id);
    }
    let node = dom.node(id);
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn quickjs_dom_node_and_element_navigation_mvp() {
  let html = r#"<!doctype html><html><body><template><span>inert</span></template><div id="a" class="c1 c2"><span>hello</span><span>world</span></div><p id="b"></p></body></html>"#;
  let renderer_dom = parse_html(html).expect("parse html");
  let dom = Document::from_renderer_dom(&renderer_dom);
  let dom: SharedDom2Document = Rc::new(RefCell::new(dom));

  let rt = Runtime::new().expect("quickjs runtime");
  let ctx = Context::full(&rt).expect("quickjs context");

  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      install_dom2_bindings(ctx.clone(), Rc::clone(&dom))?;

      assert_eq!(ctx.eval::<i32, _>("document.nodeType")?, 9);
      assert_eq!(ctx.eval::<String, _>("document.nodeName")?, "#document");

      // Basic navigation: Document -> HTML -> BODY -> TEMPLATE/DIV/P.
      assert_eq!(
        ctx.eval::<String, _>("document.firstChild.nodeName")?,
        "HTML"
      );
      assert_eq!(
        ctx.eval::<String, _>("document.firstChild.lastChild.nodeName")?,
        "BODY"
      );

      // Inert template contents should not be exposed via child navigation.
      assert_eq!(
        ctx.eval::<String, _>("document.firstChild.lastChild.firstChild.nodeName")?,
        "TEMPLATE"
      );
      assert_eq!(
        ctx.eval::<bool, _>("document.firstChild.lastChild.firstChild.firstChild === null")?,
        true
      );

      // Sibling navigation order under <body>.
      assert_eq!(
        ctx.eval::<String, _>("document.firstChild.lastChild.firstChild.nextSibling.nodeName")?,
        "DIV"
      );
      assert_eq!(
        ctx.eval::<String, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.nextSibling.nodeName"
        )?,
        "P"
      );
      assert_eq!(
        ctx.eval::<bool, _>(
          "document.firstChild.lastChild.firstChild.previousSibling === null"
        )?,
        true
      );

      // nodeValue behavior.
      assert_eq!(
        ctx.eval::<bool, _>("document.firstChild.nodeValue === null")?,
        true
      );

      // Text nodes.
      assert_eq!(
        ctx.eval::<i32, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.firstChild.firstChild.nodeType"
        )?,
        3
      );
      assert_eq!(
        ctx.eval::<String, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.firstChild.firstChild.nodeName"
        )?,
        "#text"
      );
      assert_eq!(
        ctx.eval::<String, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.firstChild.firstChild.nodeValue"
        )?,
        "hello"
      );
      assert_eq!(
        ctx.eval::<bool, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.firstChild.firstChild.firstChild === null"
        )?,
        true
      );

      // Wrapper identity is preserved via the binding cache.
      assert_eq!(
        ctx.eval::<bool, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.firstChild === document.firstChild.lastChild.firstChild.nextSibling.firstChild"
        )?,
        true
      );

      // Element reflected attributes.
      assert_eq!(
        ctx.eval::<String, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.id"
        )?,
        "a"
      );
      assert_eq!(
        ctx.eval::<String, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.className"
        )?,
        "c1 c2"
      );
      ctx.eval::<(), _>("document.firstChild.lastChild.firstChild.nextSibling.id = 'z'")?;
      assert_eq!(
        ctx.eval::<String, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.id"
        )?,
        "z"
      );
      // Restore the original id so the Rust-side mutation logic can find the node deterministically.
      ctx.eval::<(), _>("document.firstChild.lastChild.firstChild.nextSibling.id = 'a'")?;

      // contains(other) semantics (null => false).
      assert_eq!(
        ctx.eval::<bool, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.contains(document.firstChild.lastChild.firstChild.nextSibling)"
        )?,
        true
      );
      assert_eq!(
        ctx.eval::<bool, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.contains(null)"
        )?,
        false
      );
      assert_eq!(
        ctx.eval::<bool, _>(
          "document.firstChild.lastChild.firstChild.nextSibling.firstChild.contains(document.firstChild.lastChild.firstChild.nextSibling)"
        )?,
        false
      );

      // Capture a stable wrapper for connectedness toggling.
      ctx.eval::<(), _>(
        "globalThis.__savedDiv = document.firstChild.lastChild.firstChild.nextSibling;",
      )?;
      assert_eq!(ctx.eval::<bool, _>("__savedDiv.isConnected")?, true);

      Ok(())
    })
    .expect("js eval");

  let (body_id, div_id) = {
    let dom_ref = dom.borrow();
    (
      find_first_element(&dom_ref, "body").expect("body"),
      find_element_by_id(&dom_ref, "a").expect("div#a"),
    )
  };

  // Toggle connectedness by mutating the underlying `dom2` document from Rust.
  {
    let mut dom_mut = dom.borrow_mut();
    dom_mut
      .remove_child(body_id, div_id)
      .expect("remove_child");
  }

  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      assert_eq!(ctx.eval::<bool, _>("__savedDiv.isConnected")?, false);
      Ok(())
    })
    .expect("js eval");

  {
    let mut dom_mut = dom.borrow_mut();
    dom_mut
      .append_child(body_id, div_id)
      .expect("append_child");
  }

  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      assert_eq!(ctx.eval::<bool, _>("__savedDiv.isConnected")?, true);

      // innerText setter replaces children with a single text node (MVP behavior).
      ctx.eval::<(), _>("__savedDiv.innerText = 'Hi'")?;
      assert_eq!(ctx.eval::<i32, _>("__savedDiv.firstChild.nodeType")?, 3);
      assert_eq!(
        ctx.eval::<String, _>("__savedDiv.firstChild.nodeValue")?,
        "Hi"
      );
      assert_eq!(ctx.eval::<String, _>("__savedDiv.innerText")?, "Hi");

      Ok(())
    })
    .expect("js eval");
}
