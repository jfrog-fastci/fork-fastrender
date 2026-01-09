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
fn comment_nodes_support_text_content() {
  let dom = make_dom(r#"<!doctype html><html><body></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r#"
        (() => {
          try {
            const c = document.createComment("hello");
            if (c.textContent !== "hello") return "bad_get";
            c.textContent = "bye";
            if (c.textContent !== "bye") return "bad_set";
            document.body.appendChild(c);
            // Comment nodes must not contribute to element `textContent`.
            if (document.body.textContent !== "") return "bad_body_text";
            return "ok";
          } catch (e) {
            if (!e) return "unknown";
            return String(e) + "\n" + String(e.stack || "");
          }
        })()
      "#,
      )
      .unwrap();
    assert_eq!(outcome, "ok", "comment textContent JS threw: {outcome}");
  });
}

#[test]
fn node_type_and_name_are_spec_shaped() {
  let dom = make_dom(r#"<!doctype html><html><body></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r##"
        (() => {
          try {
            if (document.nodeType !== 9) return "bad_document_nodeType:" + document.nodeType;
            if (document.nodeName !== "#document") return "bad_document_nodeName:" + document.nodeName;

            const body = document.body;
            if (!body) return "missing_body";
            if (body.nodeType !== 1) return "bad_body_nodeType:" + body.nodeType;
            if (body.nodeName !== "BODY") return "bad_body_nodeName:" + body.nodeName;
            if (body.tagName !== "BODY") return "bad_body_tagName:" + body.tagName;

            const div = document.createElement("div");
            if (div.nodeType !== 1) return "bad_div_nodeType:" + div.nodeType;
            if (div.nodeName !== "DIV") return "bad_div_nodeName:" + div.nodeName;
            if (div.tagName !== "DIV") return "bad_div_tagName:" + div.tagName;

            const t = document.createTextNode("x");
            if (t.nodeType !== 3) return "bad_text_nodeType:" + t.nodeType;
            if (t.nodeName !== "#text") return "bad_text_nodeName:" + t.nodeName;

            const c = document.createComment("x");
            if (c.nodeType !== 8) return "bad_comment_nodeType:" + c.nodeType;
            if (c.nodeName !== "#comment") return "bad_comment_nodeName:" + c.nodeName;

            return "ok";
          } catch (e) {
            if (!e) return "unknown";
            return String(e) + "\n" + String(e.stack || "");
          }
        })()
      "##,
      )
      .unwrap();
    assert_eq!(outcome, "ok", "nodeType/nodeName JS threw: {outcome}");
  });
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
  let class_attr = doc
    .get_attribute(x, "class")
    .expect("get_attribute should succeed")
    .unwrap_or("");
  // Order is not specified by the MVP; we only require membership.
  let tokens: std::collections::HashSet<&str> = class_attr.split_whitespace().collect();
  assert!(tokens.contains("b"));
  assert!(tokens.contains("c"));
  assert!(!tokens.contains("a"));
  assert!(!tokens.contains("d"));
}

#[test]
fn document_head_and_body_getters_work() {
  let dom = make_dom(r#"<!doctype html><html><head></head><body></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r#"
        (() => {
          try {
            if (!document.head || !document.body) return "missing";
            const headOk = String(document.head.tagName || "").toLowerCase() === "head";
            const bodyOk = String(document.body.tagName || "").toLowerCase() === "body";
            document.body.appendChild(document.createElement("div"));
            return headOk && bodyOk ? "ok" : "bad_tag";
          } catch (e) {
            if (!e) return "unknown";
            return String(e) + "\n" + String(e.stack || "");
          }
        })()
      "#,
      )
      .unwrap();
    assert_eq!(outcome, "ok", "document.head/body JS threw: {outcome}");
  });
}
