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
fn element_scoped_query_selectors_do_not_include_the_scope_element() {
  let dom = make_dom(
    r#"<!doctype html><html><body><div id="p" class="x"><span id="a" class="x"></span><span id="b" class="x"></span></div></body></html>"#,
  );

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r##"
        (() => {
          try {
            const parent = document.getElementById("p");
            const a = document.getElementById("a");
            const b = document.getElementById("b");
            if (!parent || !a || !b) return "missing";

            const list = parent.querySelectorAll(".x");
            if (list.length !== 2) return "bad_len:" + String(list.length);
            if (list[0] !== a || list[1] !== b) return "bad_identity";

            const first = parent.querySelector(".x");
            if (first !== a) return "bad_qs";

            const none = a.querySelector(".x");
            if (none !== null) return "bad_descend";

            const scope = parent.querySelector(":scope");
            if (scope !== parent) return "bad_scope_qs";
            const scopes = parent.querySelectorAll(":scope");
            if (scopes.length !== 1 || scopes[0] !== parent) return "bad_scope_qsa";

            if (a.closest(".x") !== a) return "bad_closest_inclusive";
            if (a.closest("#p") !== parent) return "bad_closest_ancestor";
            if (a.closest("body") !== document.body) return "bad_closest_body";
            if (a.closest("section") !== null) return "bad_closest_null";

            // Invalid selector should throw SyntaxError.
            try {
              parent.querySelectorAll("[");
              return "no_throw";
            } catch (e) {
              if (String(e && e.name) !== "SyntaxError") return String(e && e.name);
            }

            // Invalid selector should throw SyntaxError for closest() as well.
            try {
              a.closest("[");
              return "no_throw_closest";
            } catch (e) {
              return String(e && e.name);
            }
          } catch (e) {
            if (!e) return "unknown";
            return String(e) + "\n" + String(e.stack || "");
          }
        })()
      "##,
      )
      .unwrap();
    assert_eq!(outcome, "SyntaxError", "element querySelector(All) failed: {outcome}");
  });
}

#[test]
fn element_closest_returns_ancestors_and_null() {
  let dom = make_dom(
    r#"<!doctype html><html><body><div id="p"><span id="a"></span></div></body></html>"#,
  );

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r##"
        (() => {
          try {
            const parent = document.getElementById("p");
            const a = document.getElementById("a");
            if (!parent || !a) return "missing";

            if (a.closest("span") !== a) return "bad_self";
            if (a.closest("div") !== parent) return "bad_parent";
            if (a.closest(".nope") !== null) return "bad_null";

            try {
              a.closest("[");
              return "no_throw";
            } catch (e) {
              return String(e && e.name);
            }
          } catch (e) {
            if (!e) return "unknown";
            return String(e) + "\n" + String(e.stack || "");
          }
        })()
      "##,
      )
      .unwrap();
    assert_eq!(outcome, "SyntaxError", "element.closest failed: {outcome}");
  });
}

#[test]
fn child_nodes_is_an_array_and_updates_after_mutations() {
  let dom = make_dom(
    r#"<!doctype html><html><body><div id="x"><span id="a"></span><span id="b"></span></div></body></html>"#,
  );

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r##"
        (() => {
          try {
            const x = document.getElementById("x");
            const a = document.getElementById("a");
            const b = document.getElementById("b");
            if (!x || !a || !b) return "missing";

            const list1 = x.childNodes;
            const list2 = x.childNodes;
            if (!Array.isArray(list1)) return "not_array";
            if (list1 !== list2) return "not_live_object";
            if (list1.length !== 2) return "bad_len:" + String(list1.length);
            if (list1[0] !== a || list1[1] !== b) return "bad_identity";

            // Element traversal APIs should align with real DOM behavior.
            if (a.parentElement !== x) return "bad_parentElement";
            if (a.nextElementSibling !== b) return "bad_nextElementSibling";
            if (b.previousElementSibling !== a) return "bad_previousElementSibling";
            if (x.firstElementChild !== a) return "bad_firstElementChild";
            if (x.lastElementChild !== b) return "bad_lastElementChild";
            if (x.childElementCount !== 2) return "bad_childElementCount:" + String(x.childElementCount);
            if (!Array.isArray(x.children) || x.children.length !== 2) return "bad_children";
            if (x.children[0] !== a || x.children[1] !== b) return "bad_children_identity";

            x.removeChild(a);
            if (a.parentNode !== null) return "bad_parent";
            if (a.parentElement !== null) return "bad_parentElement_after_remove";
            if (list1.length !== 1 || list1[0] !== b) return "bad_live_after_remove";
            if (x.childNodes.length !== 1 || x.childNodes[0] !== b) return "bad_after_remove";

            while (x.childNodes.length) {
              x.removeChild(x.childNodes[0]);
            }
            if (x.childNodes.length !== 0) return "bad_clear";
            if (list1.length !== 0) return "bad_live_clear";
            return "ok";
          } catch (e) {
            if (!e) return "unknown";
            return String(e) + "\n" + String(e.stack || "");
          }
        })()
      "##,
      )
      .unwrap();
    assert_eq!(outcome, "ok", "childNodes smoke failed: {outcome}");
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

#[test]
fn node_remove_detaches_from_dom() {
  let dom = make_dom(r#"<!doctype html><html><body><div id="x"><span id="y"></span></div></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r#"
        (() => {
          const y = document.getElementById("y");
          if (!y) return "missing";
          y.remove();

          // Calling remove on a detached node should be a no-op.
           const detached = document.createElement("div");
           detached.remove();
 
           const afterLookup = document.getElementById("y");
           const afterLookupOk = afterLookup === null;
           const yParentOk = y.parentNode === null;
           const detachedParentOk = detached.parentNode === null;
           return [afterLookupOk, yParentOk, detachedParentOk].join(",");
         })()
      "#,
      )
      .unwrap();
    assert_eq!(outcome, "true,true,true");
  });

  let doc = dom.borrow();
  assert!(doc.get_element_by_id("y").is_none());
}

#[test]
fn error_mapping_and_invalid_selectors() {
  let dom = make_dom(r#"<!doctype html><html><body></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    // Wrapper identity: document.body === document.querySelector("body")
    let same: bool = ctx.eval(r#"document.body === document.querySelector("body")"#).unwrap();
    assert!(same);

    // Invalid selector should throw a SyntaxError DOMException (not a JS SyntaxError object).
    let selector_out: String = ctx
      .eval(
        r#"(() => {
          try { document.querySelector("["); return "no throw"; }
          catch (e) { return String(e.name) + "|" + String(e instanceof DOMException) + "|" + String(e instanceof SyntaxError); }
        })()"#,
      )
      .unwrap();
    assert_eq!(selector_out, "SyntaxError|true|false");

    // DOM mutation errors should be surfaced as DOMException instances with the right name.
    let hierarchy_out: String = ctx
      .eval(
        r#"(() => {
          try {
            const t = document.createTextNode("x");
            const el = document.createElement("div");
            t.appendChild(el);
            return "no throw";
          } catch (e) {
            return String(e.name) + "|" + String(e instanceof DOMException);
          }
        })()"#,
      )
      .unwrap();
    assert_eq!(hierarchy_out, "HierarchyRequestError|true");

    let not_found_out: String = ctx
      .eval(
        r#"(() => {
          try {
            const parent = document.createElement("div");
            const child = document.createElement("span");
            parent.removeChild(child);
            return "no throw";
          } catch (e) {
            return String(e.name) + "|" + String(e instanceof DOMException);
          }
        })()"#,
      )
      .unwrap();
    assert_eq!(not_found_out, "NotFoundError|true");
  });
}

#[test]
fn document_fragment_create_and_query_selector() {
  let dom = make_dom(r#"<!doctype html><html><body><div id="in-document"></div></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let outcome: String = ctx
      .eval(
        r##"
        (() => {
          try {
            const frag = document.createDocumentFragment();
            if (frag.nodeType !== 11) return "bad_nodeType:" + String(frag.nodeType);
            if (frag.nodeName !== "#document-fragment") return "bad_nodeName:" + String(frag.nodeName);

            // DocumentFragment.querySelector must operate within the fragment, not the full document.
            if (frag.querySelector("#in-document") !== null) return "bad_scope";

            const el = document.createElement("div");
            el.id = "in-fragment";
            frag.appendChild(el);

            const found = frag.querySelector("#in-fragment");
            if (found !== el) return "bad_found";
            const all = frag.querySelectorAll("#in-fragment");
            if (all.length !== 1 || all[0] !== el) return "bad_found_all";

            // Invalid selector should still throw SyntaxError.
            try {
              frag.querySelectorAll("[");
              return "no_throw";
            } catch (e) {
              if (String(e && e.name) !== "SyntaxError") return String(e && e.name);
            }

            return "ok";
          } catch (e) {
            if (!e) return "unknown";
            return String(e) + "\n" + String(e.stack || "");
          }
        })()
      "##,
      )
      .unwrap();
    assert_eq!(outcome, "ok", "DocumentFragment querySelector failed: {outcome}");
  });
}

#[test]
fn wrapper_cache_installs_finalizer_when_supported() {
  let dom = make_dom(r#"<!doctype html><html><body></body></html>"#);

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    install_dom_bindings(ctx.clone(), Rc::clone(&dom)).unwrap();

    let has_weakref: bool = ctx.eval(r#"typeof WeakRef === "function""#).unwrap();
    assert!(has_weakref, "WeakRef intrinsic should be installed by bindings");

    let has_finalization_registry: bool =
      ctx.eval(r#"typeof FinalizationRegistry === "function""#).unwrap();
    if has_finalization_registry {
      let has_finalizer: bool = ctx
        .eval(r#"typeof globalThis.__fastrender_dom_node_cache_finalizer === "object""#)
        .unwrap();
      assert!(has_finalizer, "expected node cache FinalizationRegistry to be installed");

      let has_register_fn: bool = ctx
        .eval(r#"typeof globalThis.__fastrender_dom_node_cache_register_finalizer === "function""#)
        .unwrap();
      assert!(
        has_register_fn,
        "expected node cache finalizer register helper to be installed"
      );
    }
  });
}
