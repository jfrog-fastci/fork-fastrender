use std::cell::RefCell;
use std::rc::Rc;

use fastrender::dom2::{Document, NodeKind};
use rquickjs::{Context, Runtime};

use js_dom_bindings_quickjs::install_dom_bindings;

fn make_dom(html: &str) -> Rc<RefCell<Document>> {
  let root = fastrender::dom::parse_html(html).unwrap();
  Rc::new(RefCell::new(Document::from_renderer_dom(&root)))
}

#[test]
fn identity_and_selectors() {
  let dom = make_dom(
    r#"<!doctype html><html><body><div id="x" class="a b">hi</div><div class="b"></div></body></html>"#,
  );

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let same: bool = ctx
      .eval(
        r##"
        (() => {
          const a = document.getElementById("x");
          const b = document.getElementById("x");
          const c = document.querySelector("#x");
          return a === b && a === c;
        })()
      "##,
      )
      .unwrap();
    assert!(same);

    let n: i32 = ctx.eval(r#"document.querySelectorAll("div.b").length"#).unwrap();
    assert_eq!(n, 2);
  });
}

#[test]
fn element_creation_append_and_text_content() {
  let dom = make_dom(r#"<!doctype html><html><body></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    ctx
      .eval::<(), _>(
        r#"
        const body = document.querySelector("body");
        const div = document.createElement("div");
        div.id = "y";
        div.textContent = "hello";
        body.appendChild(div);
      "#,
      )
      .unwrap();
  });

  let doc = dom.borrow();
  let y = doc.get_element_by_id("y").expect("element should exist");
  let children = doc.children(y).unwrap();
  assert_eq!(children.len(), 1);
  let child = children[0];
  match &doc.node(child).kind {
    NodeKind::Text { content } => assert_eq!(content, "hello"),
    other => panic!("expected text node, got {other:?}"),
  }
}

#[test]
fn class_list_add_remove_toggle() {
  let dom = make_dom(r#"<!doctype html><html><body><div id="x" class="a b"></div></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r#"
        (() => {
          try {
            const el = document.getElementById("x");
            el.classList.add("c");
            el.classList.remove("a");
            el.classList.toggle("d");
            el.classList.toggle("d"); // remove
            return "ok";
          } catch (e) {
            if (!e) return "unknown";
            return String(e) + "\n" + String(e.stack || "");
          }
        })()
      "#,
      )
      .unwrap();
    assert_eq!(outcome, "ok", "classList JS threw: {outcome}");
  });

  let doc = dom.borrow();
  let x = doc.get_element_by_id("x").unwrap();
  let class_attr = doc.get_attribute(x, "class").unwrap_or("");
  // Order is not specified by the MVP; we only require membership.
  let tokens: std::collections::HashSet<&str> = class_attr.split_whitespace().collect();
  assert!(tokens.contains("b"));
  assert!(tokens.contains("c"));
  assert!(!tokens.contains("a"));
  assert!(!tokens.contains("d"));
}
