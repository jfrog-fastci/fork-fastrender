//! QuickJS-backed DOM bindings for `dom2`.
//!
//! These bindings are intentionally MVP-grade and focus on core `Node`/`Element` tree navigation
//! and identity properties so real-world scripts can walk the DOM without crashing.
//!
//! The bindings are implemented as:
//! - A small set of Rust host functions (`__dom_*`) that read/mutate the `dom2::Document`.
//! - A JS bootstrap snippet that defines `Node`/`Element`/`Document`/`Text` wrappers and maintains a
//!   wrapper cache so node identity is preserved (`node.firstChild === node.firstChild`).
#![cfg(feature = "quickjs")]

use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, DomError, NodeId, NodeKind};
use crate::js::cookie_jar::CookieJar;
use crate::resource::ResourceFetcher;
use rquickjs::{Ctx, Function};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

/// Shared handle for a mutable `dom2` document.
pub type SharedDom2Document = Rc<RefCell<Document>>;

const DOM_BOOTSTRAP: &str = r#"
(function () {
  // Minimal DOMException so host bindings can throw spec-shaped errors.
  if (typeof globalThis.DOMException !== "function") {
    const LEGACY_CODES = Object.freeze({
      IndexSizeError: 1,
      DOMStringSizeError: 2,
      HierarchyRequestError: 3,
      WrongDocumentError: 4,
      InvalidCharacterError: 5,
      NoDataAllowedError: 6,
      NoModificationAllowedError: 7,
      NotFoundError: 8,
      NotSupportedError: 9,
      InUseAttributeError: 10,
      InvalidStateError: 11,
      SyntaxError: 12,
      InvalidModificationError: 13,
      NamespaceError: 14,
      InvalidAccessError: 15,
      ValidationError: 16,
      TypeMismatchError: 17,
      SecurityError: 18,
      NetworkError: 19,
      AbortError: 20,
      URLMismatchError: 21,
      QuotaExceededError: 22,
      TimeoutError: 23,
      InvalidNodeTypeError: 24,
      DataCloneError: 25,
    });

    class DOMException extends Error {
      constructor(message = "", name = "Error") {
        // Follow WebIDL DOMString conversion semantics: `ToString` (throws on Symbols).
        const messageStr = message + "";
        const nameStr = name + "";
        super(messageStr);

        // Ensure these are own, non-enumerable data properties (match the Rust vm-js + heap-only
        // implementations and the web platform's observable behaviour).
        Object.defineProperty(this, "name", {
          value: nameStr,
          writable: true,
          enumerable: false,
          configurable: true,
        });
        Object.defineProperty(this, "message", {
          value: messageStr,
          writable: true,
          enumerable: false,
          configurable: true,
        });
        Object.defineProperty(this, "code", {
          value: LEGACY_CODES[nameStr] || 0,
          writable: true,
          enumerable: false,
          configurable: true,
        });
      }
    }

    // Legacy DOMException numeric constants (deprecated but present for web compatibility).
    // WebIDL `const`: non-writable, enumerable, non-configurable.
    for (const [name, value] of Object.entries({
      INDEX_SIZE_ERR: 1,
      DOMSTRING_SIZE_ERR: 2,
      HIERARCHY_REQUEST_ERR: 3,
      WRONG_DOCUMENT_ERR: 4,
      INVALID_CHARACTER_ERR: 5,
      NO_DATA_ALLOWED_ERR: 6,
      NO_MODIFICATION_ALLOWED_ERR: 7,
      NOT_FOUND_ERR: 8,
      NOT_SUPPORTED_ERR: 9,
      INUSE_ATTRIBUTE_ERR: 10,
      INVALID_STATE_ERR: 11,
      SYNTAX_ERR: 12,
      INVALID_MODIFICATION_ERR: 13,
      NAMESPACE_ERR: 14,
      INVALID_ACCESS_ERR: 15,
      VALIDATION_ERR: 16,
      TYPE_MISMATCH_ERR: 17,
      SECURITY_ERR: 18,
      NETWORK_ERR: 19,
      ABORT_ERR: 20,
      URL_MISMATCH_ERR: 21,
      QUOTA_EXCEEDED_ERR: 22,
      TIMEOUT_ERR: 23,
      INVALID_NODE_TYPE_ERR: 24,
      DATA_CLONE_ERR: 25,
    })) {
      Object.defineProperty(DOMException, name, {
        value,
        writable: false,
        enumerable: true,
        configurable: false,
      });
    }
    globalThis.DOMException = DOMException;
  }

  const cache = new Map(); // nodeId (number) -> wrapper object
  const ids = new WeakMap(); // wrapper -> nodeId (number)

  function idOf(node) {
    return ids.get(node);
  }

  function assertValidId(id) {
    if (id == null || !__dom_is_valid_node(id)) {
      throw new DOMException("Invalid NodeId", "NotFoundError");
    }
  }

  function wrap(id) {
    if (id == null) return null;
    assertValidId(id);
    const existing = cache.get(id);
    if (existing) return existing;

    const type = __dom_node_type(id);
    let obj;
    if (type === 9) obj = new Document(id);
    else if (type === 1) obj = new Element(id);
    else if (type === 3) obj = new Text(id);
    else if (type === 11) obj = new DocumentFragment(id);
    else obj = new Node(id);
    cache.set(id, obj);
    return obj;
  }

  class Node {
    constructor(id) {
      ids.set(this, id);
    }

    get nodeType() {
      const id = idOf(this);
      assertValidId(id);
      return __dom_node_type(id);
    }
    get nodeName() {
      const id = idOf(this);
      assertValidId(id);
      return __dom_node_name(id);
    }
    get nodeValue() {
      const id = idOf(this);
      assertValidId(id);
      const v = __dom_node_value(id);
      return v == null ? null : v;
    }
    set nodeValue(v) {
      const id = idOf(this);
      assertValidId(id);
      __dom_set_node_value(id, v == null ? "" : String(v));
    }

    get parentNode() {
      const id = idOf(this);
      assertValidId(id);
      return wrap(__dom_parent_node(id));
    }
    get parentElement() {
      const id = idOf(this);
      assertValidId(id);
      return wrap(__dom_parent_element(id));
    }

    get firstChild() {
      const id = idOf(this);
      assertValidId(id);
      return wrap(__dom_first_child(id));
    }
    get lastChild() {
      const id = idOf(this);
      assertValidId(id);
      return wrap(__dom_last_child(id));
    }
    get previousSibling() {
      const id = idOf(this);
      assertValidId(id);
      return wrap(__dom_previous_sibling(id));
    }
    get nextSibling() {
      const id = idOf(this);
      assertValidId(id);
      return wrap(__dom_next_sibling(id));
    }

    contains(other) {
      const id = idOf(this);
      assertValidId(id);
      if (other == null) return false;
      const otherId = idOf(other);
      if (otherId == null) return false;
      if (!__dom_is_valid_node(otherId)) return false;
      return __dom_contains(id, otherId);
    }

    get isConnected() {
      const id = idOf(this);
      assertValidId(id);
      return !!__dom_is_connected(id);
    }

    cloneNode(subtree) {
      const id = idOf(this);
      assertValidId(id);
      return wrap(__dom_clone_node(id, !!subtree));
    }
  }

  class Document extends Node {
    get cookie() {
      return __dom_cookie_get();
    }
    set cookie(v) {
      __dom_cookie_set(String(v));
    }
  }
  class DocumentFragment extends Node {}

  class Element extends Node {
    get tagName() {
      const id = idOf(this);
      assertValidId(id);
      return __dom_tag_name(id);
    }

    get id() {
      const id = idOf(this);
      assertValidId(id);
      return __dom_element_id(id);
    }
    set id(v) {
      const id = idOf(this);
      assertValidId(id);
      __dom_set_element_id(id, v == null ? "" : String(v));
    }

    get className() {
      const id = idOf(this);
      assertValidId(id);
      return __dom_element_class_name(id);
    }
    set className(v) {
      const id = idOf(this);
      assertValidId(id);
      __dom_set_element_class_name(id, v == null ? "" : String(v));
    }

    get innerText() {
      const id = idOf(this);
      assertValidId(id);
      return __dom_text_content(id);
    }
    set innerText(v) {
      const id = idOf(this);
      assertValidId(id);
      __dom_set_inner_text(id, v == null ? "" : String(v));
    }

    get innerHTML() {
      const id = idOf(this);
      assertValidId(id);
      return __dom_inner_html(id);
    }
    set innerHTML(v) {
      const id = idOf(this);
      assertValidId(id);
      __dom_set_inner_html(id, String(v));
    }

    get outerHTML() {
      const id = idOf(this);
      assertValidId(id);
      return __dom_outer_html(id);
    }
    set outerHTML(v) {
      const id = idOf(this);
      assertValidId(id);
      __dom_set_outer_html(id, String(v));
    }

    insertAdjacentHTML(position, text) {
      const id = idOf(this);
      assertValidId(id);
      __dom_insert_adjacent_html(id, String(position), String(text));
    }
  }

  class Text extends Node {}

  globalThis.Node = Node;
  globalThis.Element = Element;
  globalThis.Document = Document;
  globalThis.DocumentFragment = DocumentFragment;
  globalThis.Text = Text;

  // Install the global `document`.
  globalThis.document = wrap(__dom_root());
})();
"#;

/// Install MVP DOM bindings into a QuickJS realm.
///
/// After installation, the realm has:
/// - `document` (a wrapper around the `dom2` root document node)
/// - `Node`, `Element`, `Document`, `Text` constructors with basic prototype properties
pub fn install_dom2_bindings<'js>(ctx: Ctx<'js>, dom: SharedDom2Document) -> rquickjs::Result<()> {
  install_dom2_bindings_internal(ctx, dom, None)
}

/// Like [`install_dom2_bindings`], but wires `document.cookie` to a shared [`ResourceFetcher`]'s cookie store.
///
/// This keeps QuickJS `document.cookie` in sync with cookies set via HTTP `Set-Cookie` headers and makes
/// `document.cookie = ...` affect future HTTP requests made through the same fetcher.
pub fn install_dom2_bindings_with_cookie_fetcher<'js>(
  ctx: Ctx<'js>,
  dom: SharedDom2Document,
  document_url: impl Into<String>,
  fetcher: Arc<dyn ResourceFetcher>,
) -> rquickjs::Result<()> {
  install_dom2_bindings_internal(
    ctx,
    dom,
    Some(CookieEnv {
      document_url: document_url.into(),
      fetcher,
    }),
  )
}

#[derive(Clone)]
struct CookieEnv {
  document_url: String,
  fetcher: Arc<dyn ResourceFetcher>,
}

fn install_dom2_bindings_internal<'js>(
  ctx: Ctx<'js>,
  dom: SharedDom2Document,
  cookie_env: Option<CookieEnv>,
) -> rquickjs::Result<()> {
  let globals = ctx.globals();
  let cookie_jar: Rc<RefCell<CookieJar>> = Rc::new(RefCell::new(CookieJar::new()));

  // -- Cookies ---------------------------------------------------------------
  {
    let cookie_jar = Rc::clone(&cookie_jar);
    let cookie_env = cookie_env.clone();
    let f = Function::new(ctx.clone(), move || {
      let mut jar = cookie_jar.borrow_mut();
      if let Some(env) = &cookie_env {
        if let Some(header) = env.fetcher.cookie_header_value(&env.document_url) {
          jar.replace_from_cookie_header(&header);
        }
      }
      jar.cookie_string()
    })?;
    globals.set("__dom_cookie_get", f)?;
  }
  {
    let cookie_jar = Rc::clone(&cookie_jar);
    let cookie_env = cookie_env.clone();
    let f = Function::new(ctx.clone(), move |value: String| -> rquickjs::Result<()> {
      if let Some(env) = &cookie_env {
        env
          .fetcher
          .store_cookie_from_document(&env.document_url, &value);
      }
      cookie_jar.borrow_mut().set_cookie_string(&value);
      Ok(())
    })?;
    globals.set("__dom_cookie_set", f)?;
  }

  // -- Node identity + shape -------------------------------------------------

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move || dom.borrow().root().index() as u32)?;
    globals.set("__dom_root", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<bool> {
      let dom = dom.borrow();
      Ok(dom.node_id_from_index(raw as usize).is_ok())
    })?;
    globals.set("__dom_is_valid_node", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<i32> {
      let dom = dom.borrow();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(0);
      };
      let Some(node) = dom.nodes().get(id.index()) else {
        return Ok(0);
      };
      Ok(match &node.kind {
        NodeKind::Document { .. } => 9,
        NodeKind::DocumentFragment => 11,
        NodeKind::ShadowRoot { .. } => 11,
        NodeKind::Text { .. } => 3,
        NodeKind::Comment { .. } => 8,
        NodeKind::ProcessingInstruction { .. } => 7,
        NodeKind::Doctype { .. } => 10,
        NodeKind::Element { .. } | NodeKind::Slot { .. } => 1,
      })
    })?;
    globals.set("__dom_node_type", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<String> {
      let dom = dom.borrow();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(String::new());
      };
      let Some(node) = dom.nodes().get(id.index()) else {
        return Ok(String::new());
      };
      Ok(match &node.kind {
        NodeKind::Document { .. } => "#document".to_string(),
        NodeKind::DocumentFragment => "#document-fragment".to_string(),
        NodeKind::ShadowRoot { .. } => "#document-fragment".to_string(),
        NodeKind::Text { .. } => "#text".to_string(),
        NodeKind::Comment { .. } => "#comment".to_string(),
        NodeKind::ProcessingInstruction { target, .. } => target.clone(),
        NodeKind::Doctype { name, .. } => name.clone(),
        NodeKind::Slot { namespace, .. } => {
          if is_html_namespace(namespace) {
            "SLOT".to_string()
          } else {
            "slot".to_string()
          }
        }
        NodeKind::Element {
          tag_name,
          namespace,
          ..
        } => {
          if is_html_namespace(namespace) {
            tag_name.to_ascii_uppercase()
          } else {
            tag_name.to_string()
          }
        }
      })
    })?;
    globals.set("__dom_node_name", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32| -> rquickjs::Result<Option<String>> {
        let dom = dom.borrow();
        let Ok(id) = dom.node_id_from_index(raw as usize) else {
          return Ok(None);
        };
        let Some(node) = dom.nodes().get(id.index()) else {
          return Ok(None);
        };
        match &node.kind {
          NodeKind::Text { content } => Ok(Some(content.clone())),
          NodeKind::Comment { content } => Ok(Some(content.clone())),
          NodeKind::ProcessingInstruction { data, .. } => Ok(Some(data.clone())),
          _ => Ok(None),
        }
      },
    )?;
    globals.set("__dom_node_value", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32, value: String| -> rquickjs::Result<()> {
      let mut dom = dom.borrow_mut();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(());
      };
      let _ = dom.set_character_data(id, &value);
      Ok(())
    })?;
    globals.set("__dom_set_node_value", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |js_ctx: Ctx<'_>, raw: u32, deep: bool| -> rquickjs::Result<u32> {
        let result = {
          let mut dom = dom.borrow_mut();
          let Ok(id) = dom.node_id_from_index(raw as usize) else {
            return throw_dom_error(js_ctx, DomError::NotFoundError);
          };
          dom.clone_node(id, deep)
        };
        match result {
          Ok(cloned) => Ok(cloned.index() as u32),
          Err(err) => throw_dom_error(js_ctx, err),
        }
      },
    )?;
    globals.set("__dom_clone_node", f)?;
  }

  // -- Navigation ------------------------------------------------------------

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32| -> rquickjs::Result<Option<u32>> {
        let dom = dom.borrow();
        let Ok(id) = dom.node_id_from_index(raw as usize) else {
          return Ok(None);
        };
        let Some(node) = dom.nodes().get(id.index()) else {
          return Ok(None);
        };
        let Some(parent) = node.parent else {
          return Ok(None);
        };
        let Some(parent_node) = dom.nodes().get(parent.index()) else {
          return Ok(None);
        };
        // Cut off inert template contents: template children observe a null parent.
        if parent_node.inert_subtree {
          return Ok(None);
        }
        Ok(Some(parent.index() as u32))
      },
    )?;
    globals.set("__dom_parent_node", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32| -> rquickjs::Result<Option<u32>> {
        let dom = dom.borrow();
        let Some(parent) = dom_parent_node_index(&dom, raw)? else {
          return Ok(None);
        };
        let Ok(parent_id) = dom.node_id_from_index(parent as usize) else {
          return Ok(None);
        };
        let Some(parent_node) = dom.nodes().get(parent_id.index()) else {
          return Ok(None);
        };
        match &parent_node.kind {
          NodeKind::Element { .. } | NodeKind::Slot { .. } => Ok(Some(parent)),
          _ => Ok(None),
        }
      },
    )?;
    globals.set("__dom_parent_element", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32| -> rquickjs::Result<Option<u32>> {
        let dom = dom.borrow();
        let Ok(id) = dom.node_id_from_index(raw as usize) else {
          return Ok(None);
        };
        let Some(node) = dom.nodes().get(id.index()) else {
          return Ok(None);
        };
        if node.inert_subtree {
          return Ok(None);
        }
        Ok(
          node
            .children
            .iter()
            .copied()
            .find(|child| dom.nodes().get(child.index()).is_some())
            .map(|child| child.index() as u32),
        )
      },
    )?;
    globals.set("__dom_first_child", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32| -> rquickjs::Result<Option<u32>> {
        let dom = dom.borrow();
        let Ok(id) = dom.node_id_from_index(raw as usize) else {
          return Ok(None);
        };
        let Some(node) = dom.nodes().get(id.index()) else {
          return Ok(None);
        };
        if node.inert_subtree {
          return Ok(None);
        }
        Ok(
          node
            .children
            .iter()
            .rev()
            .copied()
            .find(|child| dom.nodes().get(child.index()).is_some())
            .map(|child| child.index() as u32),
        )
      },
    )?;
    globals.set("__dom_last_child", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32| -> rquickjs::Result<Option<u32>> {
        let dom = dom.borrow();
        sibling_index(&dom, raw, SiblingDir::Previous)
      },
    )?;
    globals.set("__dom_previous_sibling", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32| -> rquickjs::Result<Option<u32>> {
        let dom = dom.borrow();
        sibling_index(&dom, raw, SiblingDir::Next)
      },
    )?;
    globals.set("__dom_next_sibling", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |a: u32, b: u32| -> rquickjs::Result<bool> {
        let dom = dom.borrow();
        contains_node(&dom, a, b)
      },
    )?;
    globals.set("__dom_contains", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<bool> {
      let dom = dom.borrow();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(false);
      };
      Ok(dom.is_connected_for_scripting(id))
    })?;
    globals.set("__dom_is_connected", f)?;
  }

  // -- Element ---------------------------------------------------------------

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<String> {
      let dom = dom.borrow();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(String::new());
      };
      let Some(node) = dom.nodes().get(id.index()) else {
        return Ok(String::new());
      };
      Ok(match &node.kind {
        NodeKind::Element {
          tag_name,
          namespace,
          ..
        } => {
          if is_html_namespace(namespace) {
            tag_name.to_ascii_uppercase()
          } else {
            tag_name.to_string()
          }
        }
        NodeKind::Slot { namespace, .. } => {
          if is_html_namespace(namespace) {
            "SLOT".to_string()
          } else {
            "slot".to_string()
          }
        }
        _ => String::new(),
      })
    })?;
    globals.set("__dom_tag_name", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<String> {
      let dom = dom.borrow();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(String::new());
      };
      Ok(dom.id(id).ok().flatten().unwrap_or("").to_string())
    })?;
    globals.set("__dom_element_id", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32, value: String| -> rquickjs::Result<()> {
        let mut dom = dom.borrow_mut();
        let Ok(id) = dom.node_id_from_index(raw as usize) else {
          return Ok(());
        };
        let _ = dom.set_attribute(id, "id", &value);
        Ok(())
      },
    )?;
    globals.set("__dom_set_element_id", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<String> {
      let dom = dom.borrow();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(String::new());
      };
      Ok(dom.class_name(id).ok().flatten().unwrap_or("").to_string())
    })?;
    globals.set("__dom_element_class_name", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32, value: String| -> rquickjs::Result<()> {
        let mut dom = dom.borrow_mut();
        let Ok(id) = dom.node_id_from_index(raw as usize) else {
          return Ok(());
        };
        let _ = dom.set_attribute(id, "class", &value);
        Ok(())
      },
    )?;
    globals.set("__dom_set_element_class_name", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<String> {
      let dom = dom.borrow();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(String::new());
      };
      Ok(text_content(&dom, id))
    })?;
    globals.set("__dom_text_content", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |raw: u32, value: String| -> rquickjs::Result<()> {
        let mut dom = dom.borrow_mut();
        let Ok(parent) = dom.node_id_from_index(raw as usize) else {
          return Ok(());
        };

        // Remove existing children.
        let old_children = dom
          .nodes()
          .get(parent.index())
          .map(|node| node.children.clone())
          .unwrap_or_default();
        for child in old_children {
          let _ = dom.remove_child(parent, child);
        }

        if !value.is_empty() {
          let text = dom.create_text(&value);
          let _ = dom.append_child(parent, text);
        }

        Ok(())
      },
    )?;
    globals.set("__dom_set_inner_text", f)?;
  }

  // -- HTML serialization/parsing helpers (innerHTML / outerHTML) -------------

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |js_ctx: Ctx<'_>, raw: u32| -> rquickjs::Result<String> {
        let result = {
          let dom = dom.borrow();
          let Ok(id) = dom.node_id_from_index(raw as usize) else {
            return Ok(String::new());
          };
          dom.inner_html(id)
        };
        match result {
          Ok(html) => Ok(html),
          Err(err) => throw_dom_error(js_ctx, err),
        }
      },
    )?;
    globals.set("__dom_inner_html", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |js_ctx: Ctx<'_>, raw: u32, html: String| -> rquickjs::Result<()> {
        let result = {
          let mut dom = dom.borrow_mut();
          let Ok(id) = dom.node_id_from_index(raw as usize) else {
            return Ok(());
          };
          dom.set_inner_html(id, &html)
        };
        match result {
          Ok(()) => Ok(()),
          Err(err) => throw_dom_error(js_ctx, err),
        }
      },
    )?;
    globals.set("__dom_set_inner_html", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |js_ctx: Ctx<'_>, raw: u32| -> rquickjs::Result<String> {
        let result = {
          let dom = dom.borrow();
          let Ok(id) = dom.node_id_from_index(raw as usize) else {
            return Ok(String::new());
          };
          dom.outer_html(id)
        };
        match result {
          Ok(html) => Ok(html),
          Err(err) => throw_dom_error(js_ctx, err),
        }
      },
    )?;
    globals.set("__dom_outer_html", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |js_ctx: Ctx<'_>, raw: u32, html: String| -> rquickjs::Result<()> {
        let result = {
          let mut dom = dom.borrow_mut();
          let Ok(id) = dom.node_id_from_index(raw as usize) else {
            return Ok(());
          };
          dom.set_outer_html(id, &html)
        };
        match result {
          Ok(()) => Ok(()),
          Err(err) => throw_dom_error(js_ctx, err),
        }
      },
    )?;
    globals.set("__dom_set_outer_html", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(
      ctx.clone(),
      move |js_ctx: Ctx<'_>, raw: u32, position: String, html: String| -> rquickjs::Result<()> {
        let result = {
          let mut dom = dom.borrow_mut();
          let Ok(id) = dom.node_id_from_index(raw as usize) else {
            return Ok(());
          };
          dom.insert_adjacent_html(id, &position, &html)
        };
        match result {
          Ok(()) => Ok(()),
          Err(err) => throw_dom_error(js_ctx, err),
        }
      },
    )?;
    globals.set("__dom_insert_adjacent_html", f)?;
  }

  // Define JS wrapper classes + cache.
  ctx.eval::<(), _>(DOM_BOOTSTRAP)?;
  Ok(())
}

fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

fn throw_dom_exception<'js, T>(ctx: Ctx<'js>, name: &str, message: &str) -> rquickjs::Result<T> {
  // rquickjs surfaces thrown values via `Error::Exception`. The easiest way to throw a DOMException
  // without relying on internal constructors is to `eval` a `throw`.
  //
  // This is only used by the QuickJS test harness bindings.
  let msg = serde_json::to_string(message).unwrap_or_else(|_| "\"DOMException\"".to_string());
  let name = serde_json::to_string(name).unwrap_or_else(|_| "\"Error\"".to_string());
  match ctx.eval::<(), _>(format!("throw new DOMException({msg}, {name});")) {
    Ok(()) => Err(rquickjs::Error::Exception),
    Err(err) => Err(err),
  }
}

fn throw_dom_error<'js, T>(ctx: Ctx<'js>, err: DomError) -> rquickjs::Result<T> {
  throw_dom_exception(ctx, err.code(), err.code())
}

fn dom_parent_node_index(dom: &Document, raw: u32) -> rquickjs::Result<Option<u32>> {
  let Ok(id) = dom.node_id_from_index(raw as usize) else {
    return Ok(None);
  };
  let Some(node) = dom.nodes().get(id.index()) else {
    return Ok(None);
  };
  let Some(parent) = node.parent else {
    return Ok(None);
  };
  let Some(parent_node) = dom.nodes().get(parent.index()) else {
    return Ok(None);
  };
  if parent_node.inert_subtree {
    return Ok(None);
  }
  Ok(Some(parent.index() as u32))
}

#[derive(Debug, Clone, Copy)]
enum SiblingDir {
  Previous,
  Next,
}

fn sibling_index(dom: &Document, raw: u32, dir: SiblingDir) -> rquickjs::Result<Option<u32>> {
  let Ok(id) = dom.node_id_from_index(raw as usize) else {
    return Ok(None);
  };
  let Some(parent_raw) = dom_parent_node_index(dom, raw)? else {
    return Ok(None);
  };
  let Ok(parent_id) = dom.node_id_from_index(parent_raw as usize) else {
    return Ok(None);
  };
  let Some(parent_node) = dom.nodes().get(parent_id.index()) else {
    return Ok(None);
  };

  let pos = parent_node.children.iter().position(|&c| c == id);
  let Some(pos) = pos else {
    return Ok(None);
  };

  let iter: Box<dyn Iterator<Item = NodeId>> = match dir {
    SiblingDir::Previous => Box::new(parent_node.children.iter().take(pos).rev().copied()),
    SiblingDir::Next => Box::new(parent_node.children.iter().skip(pos + 1).copied()),
  };

  for sib in iter {
    if dom.nodes().get(sib.index()).is_some() {
      return Ok(Some(sib.index() as u32));
    }
  }
  Ok(None)
}

fn contains_node(dom: &Document, raw_self: u32, raw_other: u32) -> rquickjs::Result<bool> {
  let Ok(self_id) = dom.node_id_from_index(raw_self as usize) else {
    return Ok(false);
  };
  let Ok(other_id) = dom.node_id_from_index(raw_other as usize) else {
    return Ok(false);
  };
  if self_id == other_id {
    return Ok(true);
  }

  // Walk ancestors from `other`, stopping when we cross an inert subtree boundary.
  let mut current = Some(other_id);
  while let Some(id) = current {
    if id == self_id {
      return Ok(true);
    }
    let Some(node) = dom.nodes().get(id.index()) else {
      return Ok(false);
    };
    let Some(parent) = node.parent else {
      return Ok(false);
    };
    let Some(parent_node) = dom.nodes().get(parent.index()) else {
      return Ok(false);
    };
    if parent_node.inert_subtree {
      return Ok(false);
    }
    current = Some(parent);
  }

  Ok(false)
}

fn text_content(dom: &Document, root: NodeId) -> String {
  let mut out = String::new();
  let mut stack: Vec<NodeId> = vec![root];

  while let Some(id) = stack.pop() {
    let Some(node) = dom.nodes().get(id.index()) else {
      continue;
    };
    match &node.kind {
      NodeKind::Text { content } => out.push_str(content),
      _ => {}
    }
    if node.inert_subtree {
      continue;
    }
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  out
}

#[cfg(all(test, feature = "quickjs"))]
mod tests {
  use super::*;

  use crate::dom::parse_html;
  use crate::dom2::{
    LiveMutationEvent, LiveMutationTestRecorder, MutationObserverId, MutationObserverInit,
    MutationRecordType,
  };
  use crate::error::{Error, Result};
  use crate::resource::{FetchedResource, ResourceFetcher};
  use rquickjs::{Context, Runtime};
  use selectors::context::QuirksMode;
  use std::cell::RefCell;
  use std::rc::Rc;
  use std::sync::{Arc, Mutex};

  #[derive(Default)]
  struct CookieRecordingFetcher {
    cookies: Mutex<Vec<(String, String)>>,
  }

  impl CookieRecordingFetcher {
    fn cookie_header(&self) -> String {
      let lock = self
        .cookies
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      lock
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
    }
  }

  impl ResourceFetcher for CookieRecordingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      Err(Error::Other(format!(
        "CookieRecordingFetcher does not support fetch: {url}"
      )))
    }

    fn cookie_header_value(&self, _url: &str) -> Option<String> {
      Some(self.cookie_header())
    }

    fn store_cookie_from_document(&self, _url: &str, cookie_string: &str) {
      let first = cookie_string
        .split_once(';')
        .map(|(a, _)| a)
        .unwrap_or(cookie_string);
      let first = first.trim_matches(|c: char| c.is_ascii_whitespace());
      let Some((name, value)) = first.split_once('=') else {
        return;
      };
      let name = name.trim_matches(|c: char| c.is_ascii_whitespace());
      if name.is_empty() {
        return;
      }

      let mut lock = self
        .cookies
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      if let Some(existing) = lock.iter_mut().find(|(n, _)| n == name) {
        existing.1 = value.to_string();
      } else {
        lock.push((name.to_string(), value.to_string()));
      }
    }
  }

  #[test]
  fn document_cookie_round_trip_is_deterministic() {
    let dom = Document::new(QuirksMode::NoQuirks);
    let dom: SharedDom2Document = Rc::new(RefCell::new(dom));

    let rt = Runtime::new().expect("quickjs runtime");
    let ctx = Context::full(&rt).expect("quickjs context");

    ctx
      .with(|ctx| -> rquickjs::Result<()> {
        install_dom2_bindings(ctx.clone(), Rc::clone(&dom))?;
        ctx.eval::<(), _>("document.cookie = 'b=c; Path=/'; document.cookie = 'a=b';")?;
        let cookie: String = ctx.eval("document.cookie")?;
        assert_eq!(cookie, "a=b; b=c");
        Ok(())
      })
      .expect("js eval");
  }

  #[test]
  fn document_cookie_syncs_with_fetcher_cookie_store() {
    let dom = Document::new(QuirksMode::NoQuirks);
    let dom: SharedDom2Document = Rc::new(RefCell::new(dom));
    let fetcher = Arc::new(CookieRecordingFetcher::default());
    fetcher.store_cookie_from_document("https://example.invalid/", "z=1");

    let rt = Runtime::new().expect("quickjs runtime");
    let ctx = Context::full(&rt).expect("quickjs context");

    ctx
      .with(|ctx| -> rquickjs::Result<()> {
        install_dom2_bindings_with_cookie_fetcher(
          ctx.clone(),
          Rc::clone(&dom),
          "https://example.invalid/",
          fetcher.clone(),
        )?;

        let cookie: String = ctx.eval("document.cookie")?;
        assert_eq!(cookie, "z=1");

        ctx.eval::<(), _>("document.cookie = 'b=c; Path=/'; document.cookie = 'a=b';")?;

        assert_eq!(
          fetcher
            .cookie_header_value("https://example.invalid/")
            .unwrap_or_default(),
          "z=1; b=c; a=b"
        );

        let cookie: String = ctx.eval("document.cookie")?;
        assert_eq!(cookie, "a=b; b=c; z=1");
        Ok(())
      })
      .expect("js eval");
  }

  #[test]
  fn node_value_mutations_go_through_dom2_pipeline() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();
    let text = doc.create_text("a");
    let comment = doc.create_comment("b");

    // Attach nodes directly under the document node so JS can reach them via
    // `document.firstChild` / `nextSibling`. This bypasses hierarchy constraints but is sufficient
    // for exercising the `nodeValue` setter plumbing.
    doc.node_mut(root).children.push(text);
    doc.node_mut(root).children.push(comment);
    doc.node_mut(text).parent = Some(root);
    doc.node_mut(comment).parent = Some(root);

    let observer_id: MutationObserverId = 1;
    let mut options = MutationObserverInit::default();
    options.character_data = true;
    options.character_data_old_value = true;
    options.subtree = true;
    doc
      .mutation_observer_observe(observer_id, root, options)
      .expect("observe should succeed");

    let recorder = LiveMutationTestRecorder::default();
    doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

    let dom: SharedDom2Document = Rc::new(RefCell::new(doc));
    let gen_before = dom.borrow().mutation_generation();

    let rt = Runtime::new().expect("quickjs runtime");
    let ctx = Context::full(&rt).expect("quickjs context");
    ctx
      .with(|ctx| -> rquickjs::Result<()> {
        install_dom2_bindings(ctx.clone(), Rc::clone(&dom))?;
        ctx.eval::<(), _>(
          "document.firstChild.nodeValue = 'hello';\
           document.firstChild.nextSibling.nodeValue = 'world';",
        )?;
        Ok(())
      })
      .expect("js eval");

    {
      let dom_ref = dom.borrow();
      assert_eq!(dom_ref.text_data(text).unwrap(), "hello");
      assert_eq!(dom_ref.comment_data(comment).unwrap(), "world");
      assert_eq!(
        dom_ref.mutation_generation(),
        gen_before + 1,
        "mutation_generation should bump only for Text node replacements"
      );
    }

    let records = dom
      .borrow_mut()
      .mutation_observer_take_records(observer_id);
    assert_eq!(records.len(), 2);

    let text_record = records
      .iter()
      .find(|record| record.target == text)
      .expect("expected characterData record for Text node");
    assert_eq!(text_record.type_, MutationRecordType::CharacterData);
    assert_eq!(text_record.old_value.as_deref(), Some("a"));

    let comment_record = records
      .iter()
      .find(|record| record.target == comment)
      .expect("expected characterData record for Comment node");
    assert_eq!(comment_record.type_, MutationRecordType::CharacterData);
    assert_eq!(comment_record.old_value.as_deref(), Some("b"));

    assert_eq!(
      recorder.take(),
      vec![
        LiveMutationEvent::ReplaceData {
          node: text,
          offset: 0,
          removed_len: 1,
          inserted_len: 5,
        },
        LiveMutationEvent::ReplaceData {
          node: comment,
          offset: 0,
          removed_len: 1,
          inserted_len: 5,
        },
      ]
    );
  }

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
          ctx.eval::<bool, _>("document.firstChild.lastChild.firstChild.previousSibling === null")?,
          true
        );

        // nodeValue behavior.
        assert_eq!(ctx.eval::<bool, _>("document.firstChild.nodeValue === null")?, true);

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
          ctx.eval::<String, _>("document.firstChild.lastChild.firstChild.nextSibling.id")?,
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
          ctx.eval::<String, _>("document.firstChild.lastChild.firstChild.nextSibling.id")?,
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
        assert_eq!(ctx.eval::<String, _>("__savedDiv.firstChild.nodeValue")?, "Hi");
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
        assert_eq!(ctx.eval::<String, _>("div.firstChild.nextSibling.nodeValue")?, "tail");

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
        assert_eq!(
          ctx.eval::<String, _>("div.innerHTML")?,
          "<span id=\"child\">hi</span>tail"
        );

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
        assert_eq!(ctx.eval::<bool, _>("clone.firstChild !== div.firstChild")?, true);
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
        assert_eq!(ctx.eval::<String, _>("docClone.firstChild.nodeName")?, "HTML");
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
}
