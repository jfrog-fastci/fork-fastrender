//! QuickJS-backed DOM bindings for `dom2`.
//!
//! These bindings are intentionally MVP-grade and focus on core `Node`/`Element` tree navigation
//! and identity properties so real-world scripts can walk the DOM without crashing.
//!
//! The bindings are implemented as:
//! - A small set of Rust host functions (`__dom_*`) that read/mutate the `dom2::Document`.
//! - A JS bootstrap snippet that defines `Node`/`Element`/`Document`/`Text` wrappers and maintains a
//!   wrapper cache so node identity is preserved (`node.firstChild === node.firstChild`).

use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use rquickjs::{Ctx, Function};
use std::cell::RefCell;
use std::rc::Rc;

/// Shared handle for a mutable `dom2` document.
pub type SharedDom2Document = Rc<RefCell<Document>>;

const DOM_BOOTSTRAP: &str = r#"
(function () {
  // Minimal DOMException so host bindings can throw spec-shaped errors.
  if (typeof globalThis.DOMException !== "function") {
    class DOMException extends Error {
      constructor(message, name) {
        super(message || "");
        this.name = name || "Error";
      }
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
  }

  class Document extends Node {}
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
  let globals = ctx.globals();

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
        NodeKind::Element { tag_name, namespace, .. } => {
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
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<Option<String>> {
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
    })?;
    globals.set("__dom_node_value", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32, value: String| -> rquickjs::Result<()> {
      let mut dom = dom.borrow_mut();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(());
      };
      match &mut dom.node_mut(id).kind {
        NodeKind::Text { content } | NodeKind::Comment { content } => {
          content.clear();
          content.push_str(&value);
        }
        NodeKind::ProcessingInstruction { data, .. } => {
          data.clear();
          data.push_str(&value);
        }
        _ => {}
      }
      Ok(())
    })?;
    globals.set("__dom_set_node_value", f)?;
  }

  // -- Navigation ------------------------------------------------------------

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<Option<u32>> {
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
    })?;
    globals.set("__dom_parent_node", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<Option<u32>> {
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
    })?;
    globals.set("__dom_parent_element", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<Option<u32>> {
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
      Ok(node
        .children
        .iter()
        .copied()
        .find(|child| dom.nodes().get(child.index()).is_some())
        .map(|child| child.index() as u32))
    })?;
    globals.set("__dom_first_child", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<Option<u32>> {
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
      Ok(node
        .children
        .iter()
        .rev()
        .copied()
        .find(|child| dom.nodes().get(child.index()).is_some())
        .map(|child| child.index() as u32))
    })?;
    globals.set("__dom_last_child", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<Option<u32>> {
      let dom = dom.borrow();
      sibling_index(&dom, raw, SiblingDir::Previous)
    })?;
    globals.set("__dom_previous_sibling", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |raw: u32| -> rquickjs::Result<Option<u32>> {
      let dom = dom.borrow();
      sibling_index(&dom, raw, SiblingDir::Next)
    })?;
    globals.set("__dom_next_sibling", f)?;
  }

  {
    let dom = Rc::clone(&dom);
    let f = Function::new(ctx.clone(), move |a: u32, b: u32| -> rquickjs::Result<bool> {
      let dom = dom.borrow();
      contains_node(&dom, a, b)
    })?;
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
        NodeKind::Element { tag_name, namespace, .. } => {
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
    let f = Function::new(ctx.clone(), move |raw: u32, value: String| -> rquickjs::Result<()> {
      let mut dom = dom.borrow_mut();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(());
      };
      let _ = dom.set_attribute(id, "id", &value);
      Ok(())
    })?;
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
    let f = Function::new(ctx.clone(), move |raw: u32, value: String| -> rquickjs::Result<()> {
      let mut dom = dom.borrow_mut();
      let Ok(id) = dom.node_id_from_index(raw as usize) else {
        return Ok(());
      };
      let _ = dom.set_attribute(id, "class", &value);
      Ok(())
    })?;
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
    let f = Function::new(ctx.clone(), move |raw: u32, value: String| -> rquickjs::Result<()> {
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
    })?;
    globals.set("__dom_set_inner_text", f)?;
  }

  // Define JS wrapper classes + cache.
  ctx.eval::<(), _>(DOM_BOOTSTRAP)?;
  Ok(())
}

fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
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
