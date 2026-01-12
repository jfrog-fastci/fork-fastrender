#![cfg(feature = "quickjs")]

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
    dom_mut.remove_child(body_id, div_id).expect("remove_child");
  }

  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      assert_eq!(ctx.eval::<bool, _>("__savedDiv.isConnected")?, false);
      Ok(())
    })
    .expect("js eval");

  {
    let mut dom_mut = dom.borrow_mut();
    dom_mut.append_child(body_id, div_id).expect("append_child");
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

#[test]
fn quickjs_dom_inner_html_and_outer_html_round_trip() {
  let html = "<!doctype html><html><head></head><body><div id=target></div></body></html>";
  let renderer_dom = parse_html(html).expect("parse html");
  let dom = Document::from_renderer_dom(&renderer_dom);
  let dom: SharedDom2Document = Rc::new(RefCell::new(dom));

  let rt = Runtime::new().expect("quickjs runtime");
  let ctx = Context::full(&rt).expect("quickjs context");

  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      install_dom2_bindings(ctx.clone(), Rc::clone(&dom))?;

      // Document -> HTML -> BODY -> DIV (target).
      ctx.eval::<(), _>("globalThis.div = document.firstChild.lastChild.firstChild;")?;
      assert_eq!(ctx.eval::<String, _>("div.tagName")?, "DIV");
      assert_eq!(ctx.eval::<String, _>("div.id")?, "target");
      assert_eq!(ctx.eval::<String, _>("div.innerHTML")?, "");

      // innerHTML setter + getter should round trip.
      ctx.eval::<(), _>("div.innerHTML = '<span id=child>hi</span>tail';")?;
      assert_eq!(
        ctx.eval::<String, _>("div.innerHTML")?,
        "<span id=\"child\">hi</span>tail"
      );
      assert_eq!(ctx.eval::<String, _>("div.firstChild.nodeName")?, "SPAN");
      assert_eq!(ctx.eval::<String, _>("div.firstChild.id")?, "child");
      assert_eq!(
        ctx.eval::<String, _>("div.firstChild.nextSibling.nodeValue")?,
        "tail"
      );

      // outerHTML getter should serialize the element itself.
      assert_eq!(
        ctx.eval::<String, _>("div.outerHTML")?,
        "<div id=\"target\"><span id=\"child\">hi</span>tail</div>"
      );

      // Setting innerHTML to a script should insert it but scripts must be marked inert for later
      // execution (see Rust-side assertion below).
      ctx.eval::<(), _>("div.innerHTML = '<script id=s>console.log(1)</script>';")?;
      assert_eq!(ctx.eval::<String, _>("div.firstChild.nodeName")?, "SCRIPT");

      Ok(())
    })
    .expect("js eval");

  // Verify that scripts inserted via `innerHTML` are marked "already started" so they never execute
  // even if later moved/reinserted.
  let script_id = {
    let dom_ref = dom.borrow();
    dom_ref
      .get_element_by_id("s")
      .expect("expected script inserted via innerHTML")
  };
  assert!(
    dom.borrow().node(script_id).script_already_started,
    "expected innerHTML-inserted script to be marked already started"
  );

  // outerHTML setter should replace the element in the tree and disconnect the old wrapper.
  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      ctx.eval::<(), _>("div.outerHTML = '<p id=replaced>ok</p>';")?;
      assert_eq!(ctx.eval::<bool, _>("div.isConnected")?, false);
      assert_eq!(
        ctx.eval::<String, _>("document.firstChild.lastChild.firstChild.nodeName")?,
        "P"
      );
      assert_eq!(
        ctx.eval::<String, _>("document.firstChild.lastChild.firstChild.id")?,
        "replaced"
      );
      Ok(())
    })
    .expect("js eval");
}

#[test]
fn quickjs_dom_insert_adjacent_html_round_trip() {
  let html = "<!doctype html><html><head></head><body><div id=target></div></body></html>";
  let renderer_dom = parse_html(html).expect("parse html");
  let dom = Document::from_renderer_dom(&renderer_dom);
  let dom: SharedDom2Document = Rc::new(RefCell::new(dom));

  let rt = Runtime::new().expect("quickjs runtime");
  let ctx = Context::full(&rt).expect("quickjs context");

  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      install_dom2_bindings(ctx.clone(), Rc::clone(&dom))?;

      ctx.eval::<(), _>("globalThis.div = document.firstChild.lastChild.firstChild;")?;
      assert_eq!(ctx.eval::<String, _>("div.tagName")?, "DIV");
      assert_eq!(ctx.eval::<String, _>("div.id")?, "target");

      // Live insertion via insertAdjacentHTML.
      ctx.eval::<(), _>("div.insertAdjacentHTML('beforeend', '<span id=child>hi</span>tail');")?;
      assert_eq!(ctx.eval::<String, _>("div.innerHTML")?, "<span id=\"child\">hi</span>tail");

      // Invalid position throws SyntaxError.
      ctx.eval::<(), _>(
        "globalThis.__bad = false; try { div.insertAdjacentHTML('nope', '<b>x</b>'); } catch(e) { __bad = (e.name === 'SyntaxError'); }",
      )?;
      assert_eq!(ctx.eval::<bool, _>("__bad")?, true);

      // Insert a <script>; it should be marked already started in Rust.
      ctx.eval::<(), _>("div.insertAdjacentHTML('beforeend', '<script id=s>console.log(1)</script>');")?;
      assert_eq!(ctx.eval::<String, _>("div.lastChild.nodeName")?, "SCRIPT");
      Ok(())
    })
    .expect("js eval");

  let script_id = {
    let dom_ref = dom.borrow();
    dom_ref
      .get_element_by_id("s")
      .expect("expected script inserted via insertAdjacentHTML")
  };
  assert!(
    dom.borrow().node(script_id).script_already_started,
    "expected insertAdjacentHTML-inserted script to be marked already started"
  );
}

#[test]
fn quickjs_dom_clone_node_deep_clones_detached_subtree() {
  let html = "<!doctype html><html><body><div id=a><span>hello</span></div></body></html>";
  let renderer_dom = parse_html(html).expect("parse html");
  let dom = Document::from_renderer_dom(&renderer_dom);
  let dom: SharedDom2Document = Rc::new(RefCell::new(dom));

  let rt = Runtime::new().expect("quickjs runtime");
  let ctx = Context::full(&rt).expect("quickjs context");

  ctx
    .with(|ctx| -> rquickjs::Result<()> {
      install_dom2_bindings(ctx.clone(), Rc::clone(&dom))?;

      ctx.eval::<(), _>("globalThis.div = document.firstChild.lastChild.firstChild;")?;
      ctx.eval::<(), _>("globalThis.clone = div.cloneNode(true);")?;

      assert_eq!(ctx.eval::<bool, _>("clone !== div")?, true);
      assert_eq!(ctx.eval::<String, _>("clone.tagName")?, "DIV");
      assert_eq!(ctx.eval::<String, _>("clone.id")?, "a");

      assert_eq!(ctx.eval::<bool, _>("clone.isConnected")?, false);
      assert_eq!(ctx.eval::<bool, _>("clone.parentNode === null")?, true);

      assert_eq!(ctx.eval::<String, _>("clone.firstChild.nodeName")?, "SPAN");
      assert_eq!(
        ctx.eval::<bool, _>("clone.firstChild !== div.firstChild")?,
        true
      );
      assert_eq!(
        ctx.eval::<String, _>("clone.firstChild.firstChild.nodeValue")?,
        "hello"
      );

      ctx.eval::<(), _>("globalThis.shallow = div.cloneNode();")?;
      assert_eq!(ctx.eval::<bool, _>("shallow.firstChild === null")?, true);

      // Document.cloneNode should return a detached Document object (not throw).
      ctx.eval::<(), _>("globalThis.docClone = document.cloneNode(true);")?;
      assert_eq!(ctx.eval::<bool, _>("docClone !== document")?, true);
      assert_eq!(ctx.eval::<i32, _>("docClone.nodeType")?, 9);
      assert_eq!(ctx.eval::<String, _>("docClone.nodeName")?, "#document");
      assert_eq!(ctx.eval::<bool, _>("docClone.parentNode === null")?, true);
      assert_eq!(ctx.eval::<bool, _>("docClone.isConnected")?, false);
      assert_eq!(
        ctx.eval::<String, _>("docClone.firstChild.nodeName")?,
        "HTML"
      );
      assert_eq!(
        ctx.eval::<String, _>("docClone.firstChild.lastChild.firstChild.id")?,
        "a"
      );

      ctx.eval::<(), _>("globalThis.docShallow = document.cloneNode();")?;
      assert_eq!(ctx.eval::<bool, _>("docShallow.firstChild === null")?, true);

      Ok(())
    })
    .expect("js eval");
}
