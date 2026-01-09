//! QuickJS-backed DOM bindings for FastRender's `dom2`.
//!
//! This crate is intentionally small and focuses on wiring up host-maintained state that is
//! observable from JavaScript, such as `document.currentScript`.

use fastrender::dom2::{Document as Dom2Document, DomError, NodeId, NodeKind};
use fastrender::js::DomHost;
use fastrender::web::dom::DomException;
use fastrender::js::CurrentScriptStateHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use std::cell::RefCell;
use std::rc::Rc;

fn node_tag_name(dom: &Dom2Document, node_id: NodeId) -> String {
  match &dom.node(node_id).kind {
    NodeKind::Element { tag_name, .. } => tag_name.to_ascii_uppercase(),
    NodeKind::Slot { .. } => "SLOT".to_string(),
    _ => String::new(),
  }
}

fn get_text_content(dom: &Dom2Document, root: NodeId) -> String {
  match &dom.node(root).kind {
    NodeKind::Text { content } => return content.clone(),
    _ => {}
  }

  let mut out = String::new();
  for id in dom.subtree_preorder(root) {
    if let NodeKind::Text { content } = &dom.node(id).kind {
      out.push_str(content);
    }
  }
  out
}

fn set_text_content(dom: &mut Dom2Document, node: NodeId, value: &str) -> Result<bool, DomError> {
  if node.index() >= dom.nodes_len() {
    return Err(DomError::NotFoundError);
  }

  match &dom.node(node).kind {
    NodeKind::Text { .. } => return dom.set_text_data(node, value),
    _ => {}
  }

  // Replace children.
  let children: Vec<NodeId> = dom.children(node)?.to_vec();
  let mut changed = false;
  for child in children {
    changed |= dom.remove_child(node, child)?;
  }

  if !value.is_empty() {
    let text = dom.create_text(value);
    changed |= dom.append_child(node, text)?;
  }

  Ok(changed)
}

pub fn install_dom_bindings<'js, Host>(
  ctx: Ctx<'js>,
  globals: &Object<'js>,
  dom: Rc<RefCell<Host>>,
  current_script_state: CurrentScriptStateHandle,
) -> JsResult<()>
where
  Host: DomHost + 'static,
{
  let document = Object::new(ctx.clone())?;
  globals.set("document", document.clone())?;

  // `document.currentScript` is a read-only attribute. We define it as a JS accessor that calls
  // into Rust, so it always reflects the host-maintained orchestrator state.
  let dom_for_getter = Rc::clone(&dom);
  let state_for_getter = current_script_state.clone();
  let getter = Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> JsResult<Option<Object<'js>>> {
    let Some(node_id) = state_for_getter.borrow().current_script else {
      return Ok(None);
    };

    // The orchestrator stores `NodeId` handles. If the node is gone (future DOM delete support),
    // surface `null` rather than crashing.
    let is_valid_script = dom_for_getter.borrow().with_dom(|dom| {
      node_id.index() < dom.nodes_len() && matches!(&dom.node(node_id).kind, NodeKind::Element { .. })
    });
    if !is_valid_script {
      return Ok(None);
    };

    // Create/lookup a JS wrapper so `document.currentScript` can be used like a real element
    // (`.id`, `.getAttribute`, `.dataset`, etc.).
    let globals = ctx.globals();
    let wrap_fn: Function<'js> = globals.get("__fastrender_wrap_node_id")?;
    let el: Object<'js> = wrap_fn.call((node_id.index() as u32, "element"))?;
    Ok(Some(el))
  })?;

  globals.set("__fastrender_get_current_script", getter)?;
  ctx.eval::<(), _>(concat!(
    "Object.defineProperty(document, 'currentScript', {",
    "  get: globalThis.__fastrender_get_current_script,",
     "});"
   ))?;

  install_dom_exceptions_and_minimal_dom(ctx.clone(), globals, Rc::clone(&dom))?;

  // `document.documentElement` / `document.head` / `document.body` are commonly used by bootstrap
  // scripts (e.g. `document.head.appendChild(script)`).
  //
  // This is not a full DOM implementation; we expose stable JS objects with minimal shape needed by
  // real-world scripts.
  let (doc_el_id, head_id, body_id) = dom
    .borrow()
    .with_dom(|dom| (dom.document_element(), dom.head(), dom.body()));
  let wrap_fn: Function<'js> = globals.get("__fastrender_wrap_node_id")?;
  let doc_el = doc_el_id
    .map(|id| wrap_fn.call::<_, Object<'js>>((id.index() as u32, "element")))
    .transpose()?;
  let head = head_id
    .map(|id| wrap_fn.call::<_, Object<'js>>((id.index() as u32, "element")))
    .transpose()?;
  let body = body_id
    .map(|id| wrap_fn.call::<_, Object<'js>>((id.index() as u32, "element")))
    .transpose()?;
  document.set("documentElement", doc_el)?;
  document.set("head", head)?;
  document.set("body", body)?;

  Ok(())
}

fn install_dom_exceptions_and_minimal_dom<'js, Host>(
  ctx: Ctx<'js>,
  globals: &Object<'js>,
  dom: Rc<RefCell<Host>>,
) -> JsResult<()>
where
  Host: DomHost + 'static,
{
  // Host functions used by the JS shim.
  globals.set(
    "__fastrender_dom_create_element",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |tag_name: String| {
        let mut dom = dom.borrow_mut();
        let id = dom.mutate_dom(|dom| {
          let id = dom.create_element(&tag_name, "");
          // Creating a detached node does not affect rendered output.
          (id, false)
        });
        Ok::<u32, rquickjs::Error>(id.index() as u32)
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_create_text_node",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |data: String| {
        let mut dom = dom.borrow_mut();
        let id = dom.mutate_dom(|dom| {
          let id = dom.create_text(&data);
          // Detached nodes do not affect rendered output.
          (id, false)
        });
        Ok::<u32, rquickjs::Error>(id.index() as u32)
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_append_child",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, parent: u32, child: u32| {
        let mut dom = dom.borrow_mut();
        let result = dom.mutate_dom(|dom| {
          let parent = NodeId::from_index(parent as usize);
          let child = NodeId::from_index(child as usize);
          match dom.append_child(parent, child) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_insert_before",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, parent: u32, child: u32, reference: Option<u32>| {
        let mut dom = dom.borrow_mut();
        let reference = reference.map(|id| NodeId::from_index(id as usize));
        let result = dom.mutate_dom(|dom| {
          let parent = NodeId::from_index(parent as usize);
          let child = NodeId::from_index(child as usize);
          match dom.insert_before(parent, child, reference) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_remove_child",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, parent: u32, child: u32| {
        let mut dom = dom.borrow_mut();
        let result = dom.mutate_dom(|dom| {
          let parent = NodeId::from_index(parent as usize);
          let child = NodeId::from_index(child as usize);
          match dom.remove_child(parent, child) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_replace_child",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, parent: u32, new_child: u32, old_child: u32| {
        let mut dom = dom.borrow_mut();
        let result = dom.mutate_dom(|dom| {
          let parent = NodeId::from_index(parent as usize);
          let new_child = NodeId::from_index(new_child as usize);
          let old_child = NodeId::from_index(old_child as usize);
          match dom.replace_child(parent, new_child, old_child) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_get_parent_node",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |node: u32| {
        let node_id = NodeId::from_index(node as usize);
        let parent = dom.borrow().with_dom(|dom| {
          if node_id.index() >= dom.nodes_len() {
            return None;
          }
          dom.parent_node(node_id).map(|id| id.index() as u32)
        });
        Ok::<Option<u32>, rquickjs::Error>(parent)
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_query_selector",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, selectors: String, scope: Option<u32>| {
        let mut dom = dom.borrow_mut();
        let scope = scope.map(|id| NodeId::from_index(id as usize));
        let result = dom.mutate_dom(|dom| {
          match dom.query_selector(&selectors, scope) {
            Ok(found) => (Ok(found), false),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(found) => Ok(found.map(|id| id.index() as u32)),
          Err(DomException::SyntaxError { message }) => throw_syntax_error(ctx, &message),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_query_selector_all",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, selectors: String, scope: Option<u32>| {
        let mut dom = dom.borrow_mut();
        let scope = scope.map(|id| NodeId::from_index(id as usize));
        let result = dom.mutate_dom(|dom| match dom.query_selector_all(&selectors, scope) {
          Ok(found) => (Ok(found), false),
          Err(err) => (Err(err), false),
        });
        match result {
          Ok(found) => Ok(found.into_iter().map(|id| id.index() as u32).collect::<Vec<_>>()),
          Err(DomException::SyntaxError { message }) => throw_syntax_error(ctx, &message),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_matches_selector",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, selectors: String| {
        let mut dom = dom.borrow_mut();
        let node_id = NodeId::from_index(node as usize);
        let result = dom.mutate_dom(|dom| match dom.matches_selector(node_id, &selectors) {
          Ok(matched) => (Ok(matched), false),
          Err(err) => (Err(err), false),
        });
        match result {
          Ok(matched) => Ok(matched),
          Err(DomException::SyntaxError { message }) => throw_syntax_error(ctx, &message),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_get_element_by_id",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |id: String| {
        let found = dom.borrow().with_dom(|dom| dom.get_element_by_id(&id));
        Ok::<Option<u32>, rquickjs::Error>(found.map(|id| id.index() as u32))
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_get_attribute",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, name: String| {
        let node_id = NodeId::from_index(node as usize);
        let result = dom.borrow().with_dom(|dom| {
          dom
            .get_attribute(node_id, &name)
            .map(|v| v.map(|s| s.to_string()))
        });
        match result {
          Ok(v) => Ok(v),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_has_attribute",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, name: String| {
        let node_id = NodeId::from_index(node as usize);
        let result = dom.borrow().with_dom(|dom| dom.has_attribute(node_id, &name));
        match result {
          Ok(v) => Ok(v),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_set_attribute",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, name: String, value: String| {
        let mut host = dom.borrow_mut();
        let result = host.mutate_dom(|dom| {
          let node_id = NodeId::from_index(node as usize);
          match dom.set_attribute(node_id, &name, &value) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_remove_attribute",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, name: String| {
        let mut host = dom.borrow_mut();
        let result = host.mutate_dom(|dom| {
          let node_id = NodeId::from_index(node as usize);
          match dom.remove_attribute(node_id, &name) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_set_bool_attribute",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, name: String, present: bool| {
        let mut host = dom.borrow_mut();
        let result = host.mutate_dom(|dom| {
          let node_id = NodeId::from_index(node as usize);
          match dom.set_bool_attribute(node_id, &name, present) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_dataset_get",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |node: u32, prop: String| {
        let node_id = NodeId::from_index(node as usize);
        let result = dom
          .borrow()
          .with_dom(|dom| dom.dataset_get(node_id, &prop).map(|v| v.to_string()));
        Ok::<Option<String>, rquickjs::Error>(result)
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_dataset_set",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, prop: String, value: String| {
        let mut host = dom.borrow_mut();
        let result = host.mutate_dom(|dom| {
          let node_id = NodeId::from_index(node as usize);
          match dom.dataset_set(node_id, &prop, &value) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_dataset_delete",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, prop: String| {
        let mut host = dom.borrow_mut();
        let result = host.mutate_dom(|dom| {
          let node_id = NodeId::from_index(node as usize);
          match dom.dataset_delete(node_id, &prop) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_style_get_property_value",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |node: u32, name: String| {
        let node_id = NodeId::from_index(node as usize);
        let result = dom.borrow().with_dom(|dom| dom.style_get_property_value(node_id, &name));
        Ok::<String, rquickjs::Error>(result)
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_style_set_property",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, name: String, value: String| {
        let mut host = dom.borrow_mut();
        let result = host.mutate_dom(|dom| {
          let node_id = NodeId::from_index(node as usize);
          match dom.style_set_property(node_id, &name, &value) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_get_text_content",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32| {
        let node_id = NodeId::from_index(node as usize);
        let result = dom.borrow().with_dom(|dom| {
          if node_id.index() >= dom.nodes_len() {
            return Err(DomError::NotFoundError);
          }
          Ok(get_text_content(dom, node_id))
        });
        match result {
          Ok(v) => Ok(v),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_set_text_content",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32, value: String| {
        let mut host = dom.borrow_mut();
        let result = host.mutate_dom(|dom| {
          let node_id = NodeId::from_index(node as usize);
          match set_text_content(dom, node_id, &value) {
            Ok(changed) => (Ok(changed), changed),
            Err(err) => (Err(err), false),
          }
        });
        match result {
          Ok(changed) => Ok(changed),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fastrender_dom_get_tag_name",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, node: u32| {
        let node_id = NodeId::from_index(node as usize);
        let result = dom.borrow().with_dom(|dom| {
          if node_id.index() >= dom.nodes_len() {
            return Err(DomError::NotFoundError);
          }
          Ok(node_tag_name(dom, node_id))
        });
        match result {
          Ok(v) => Ok(v),
          Err(err) => throw_dom_error(ctx, err),
        }
      }
    })?,
  )?;

  ctx.eval::<(), _>(DOM_BINDINGS_SHIM)?;

  Ok(())
}

fn throw_dom_error<'js, T>(ctx: Ctx<'js>, err: DomError) -> JsResult<T> {
  let globals = ctx.globals();
  let thrower: Function<'js> = globals.get("__fastrender_throw_dom_exception")?;
  let name = err.code();
  match thrower.call::<_, ()>((name, name)) {
    Ok(_) => unreachable!("thrower must throw"),
    Err(e) => Err(e),
  }
}

fn throw_syntax_error<'js, T>(ctx: Ctx<'js>, message: &str) -> JsResult<T> {
  let globals = ctx.globals();
  let thrower: Function<'js> = globals.get("__fastrender_throw_syntax_error")?;
  match thrower.call::<_, ()>((message,)) {
    Ok(_) => unreachable!("thrower must throw"),
    Err(e) => Err(e),
  }
}

// Note: use `r##"..."##` (double-hash) so the shim can contain `"#` sequences (e.g. CSS selectors
// like `"#id"`), which would otherwise terminate a `r#"... "#` raw string literal.
const DOM_BINDINGS_SHIM: &str = r##"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (g.__fastrender_dom_bindings_installed) return;
  g.__fastrender_dom_bindings_installed = true;

  // --- DOMException (minimal but spec-shaped enough for WPT + real scripts) ---
  if (typeof g.DOMException !== "function") {
    g.DOMException = class DOMException extends Error {
      constructor(message, name) {
        super(message === undefined ? "" : String(message));
        this.name = name === undefined ? "Error" : String(name);
      }
    };
  }

  // Helpers used by Rust host functions to throw the desired error type.
  g.__fastrender_throw_dom_exception = function (name, message) {
    throw new g.DOMException(message, name);
  };
  g.__fastrender_throw_syntax_error = function (message) {
    throw new SyntaxError(message === undefined ? "" : String(message));
  };

  var doc = g.document;
  if (!doc) return;

  function define(obj, key, value) {
    try {
      Object.defineProperty(obj, key, { value: value, writable: true, configurable: true });
    } catch (_e) {
      obj[key] = value;
    }
  }

  function ensureArrayProp(obj, key) {
    if (!obj) return;
    if (!Array.isArray(obj[key])) {
      define(obj, key, []);
    }
  }

  // Node id -> JS object mapping so selector queries can preserve identity.
  var nodeById = g.__fastrender_node_by_id;
  if (!nodeById) {
    nodeById = new Map();
    g.__fastrender_node_by_id = nodeById;
  }

  function ensureNodeBasics(obj, id) {
    if (!obj) return obj;
    if (obj.__node_id == null) define(obj, "__node_id", id);
    if (!("parentNode" in obj)) define(obj, "parentNode", null);
    ensureArrayProp(obj, "childNodes");
    if (!("ownerDocument" in obj)) define(obj, "ownerDocument", doc);
    nodeById.set(id, obj);
    return obj;
  }

  // --- Prototypes ------------------------------------------------------------

  function Node() {}

  Node.prototype.appendChild = function (child) {
    if (!child || (typeof child !== "object" && typeof child !== "function") || child.__node_id == null) {
      throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
    }

    g.__fastrender_dom_append_child(this.__node_id, child.__node_id);

    ensureArrayProp(this, "childNodes");

    // Maintain a small JS-side view of the tree for bootstrap scripts that inspect `childNodes`.
    // If the child was already attached somewhere else, detach it from the old parent's JS list.
    var oldParent = child.parentNode;
    if (oldParent && oldParent !== this && Array.isArray(oldParent.childNodes)) {
      var oldIdx = oldParent.childNodes.indexOf(child);
      if (oldIdx >= 0) oldParent.childNodes.splice(oldIdx, 1);
    }
    if (Array.isArray(this.childNodes)) {
      var idx = this.childNodes.indexOf(child);
      if (idx >= 0) this.childNodes.splice(idx, 1);
      this.childNodes.push(child);
    }
    child.parentNode = this;
    return child;
  };

  Node.prototype.insertBefore = function (child, referenceChild) {
    if (!child || (typeof child !== "object" && typeof child !== "function") || child.__node_id == null) {
      throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
    }
    if (referenceChild === child) return child;
    if (referenceChild != null && ((typeof referenceChild !== "object" && typeof referenceChild !== "function") || referenceChild.__node_id == null)) {
      throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
    }
    var refId = referenceChild == null ? null : referenceChild.__node_id;

    g.__fastrender_dom_insert_before(this.__node_id, child.__node_id, refId);

    ensureArrayProp(this, "childNodes");
    if (referenceChild != null && Array.isArray(this.childNodes) && this.childNodes.indexOf(referenceChild) < 0) {
      // If the reference child wrapper isn't in our JS-side list yet (e.g. initial DOM nodes), add
      // it so we can maintain relative ordering for nodes inserted via JS.
      this.childNodes.push(referenceChild);
    }

    // Mirror appendChild JS-tree maintenance: detach from old parent's list, then insert.
    var oldParent = child.parentNode;
    if (oldParent && oldParent !== this && Array.isArray(oldParent.childNodes)) {
      var oldIdx = oldParent.childNodes.indexOf(child);
      if (oldIdx >= 0) oldParent.childNodes.splice(oldIdx, 1);
    }
    if (Array.isArray(this.childNodes)) {
      var existingIdx = this.childNodes.indexOf(child);
      if (existingIdx >= 0) this.childNodes.splice(existingIdx, 1);
      if (referenceChild == null) {
        this.childNodes.push(child);
      } else {
        var refIdx = this.childNodes.indexOf(referenceChild);
        if (refIdx < 0) {
          this.childNodes.push(child);
        } else {
          this.childNodes.splice(refIdx, 0, child);
        }
      }
    }
    child.parentNode = this;
    return child;
  };

  Node.prototype.removeChild = function (child) {
    if (!child || (typeof child !== "object" && typeof child !== "function") || child.__node_id == null) {
      throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
    }

    g.__fastrender_dom_remove_child(this.__node_id, child.__node_id);

    if (Array.isArray(this.childNodes)) {
      var idx = this.childNodes.indexOf(child);
      if (idx >= 0) this.childNodes.splice(idx, 1);
    }
    if (child.parentNode === this) child.parentNode = null;
    return child;
  };

  Node.prototype.replaceChild = function (newChild, oldChild) {
    if (!newChild || (typeof newChild !== "object" && typeof newChild !== "function") || newChild.__node_id == null) {
      throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
    }
    if (!oldChild || (typeof oldChild !== "object" && typeof oldChild !== "function") || oldChild.__node_id == null) {
      throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
    }
    if (newChild === oldChild) return oldChild;

    g.__fastrender_dom_replace_child(this.__node_id, newChild.__node_id, oldChild.__node_id);

    ensureArrayProp(this, "childNodes");

    var oldParent = newChild.parentNode;
    if (oldParent && oldParent !== this && Array.isArray(oldParent.childNodes)) {
      var oldIdx = oldParent.childNodes.indexOf(newChild);
      if (oldIdx >= 0) oldParent.childNodes.splice(oldIdx, 1);
    }

    if (Array.isArray(this.childNodes)) {
      var existingIdx = this.childNodes.indexOf(newChild);
      if (existingIdx >= 0) this.childNodes.splice(existingIdx, 1);
      var idx = this.childNodes.indexOf(oldChild);
      if (idx >= 0) {
        this.childNodes[idx] = newChild;
      } else {
        this.childNodes.push(newChild);
      }
    }

    if (oldChild.parentNode === this) oldChild.parentNode = null;
    newChild.parentNode = this;
    return oldChild;
  };

  // DOM `ChildNode.remove()`: detach this node from its parent if connected.
  //
  // This is widely used by real sites. Our minimal binding layer does not maintain parent pointers
  // for nodes that existed in the initial DOM snapshot, so we fall back to querying the host for
  // the current parent when needed.
  Node.prototype.remove = function () {
    if (this.__node_id == null) {
      throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
    }

    // Fast path: if our JS-side parent pointer exists, delegate to `removeChild` so we keep the
    // JS-side `childNodes` cache consistent.
    var jsParent = this.parentNode;
    if (
      jsParent &&
      (typeof jsParent === "object" || typeof jsParent === "function") &&
      jsParent.__node_id != null &&
      typeof jsParent.removeChild === "function"
    ) {
      jsParent.removeChild(this);
      return;
    }

    var parentId = g.__fastrender_dom_get_parent_node(this.__node_id);
    if (parentId == null) {
      // Disconnected; clear any stale JS-side parent pointers.
      if (jsParent && Array.isArray(jsParent.childNodes)) {
        var idx = jsParent.childNodes.indexOf(this);
        if (idx >= 0) jsParent.childNodes.splice(idx, 1);
      }
      this.parentNode = null;
      return;
    }

    g.__fastrender_dom_remove_child(parentId, this.__node_id);

    // Best-effort: update JS-side `childNodes` for the parent wrapper if it exists.
    var parentObj = nodeById.get(parentId);
    if (parentObj && Array.isArray(parentObj.childNodes)) {
      var idx = parentObj.childNodes.indexOf(this);
      if (idx >= 0) parentObj.childNodes.splice(idx, 1);
    }
    this.parentNode = null;
  };

  try {
    Object.defineProperty(Node.prototype, "textContent", {
      get: function () {
        return String(g.__fastrender_dom_get_text_content(this.__node_id));
      },
      set: function (value) {
        var v = value == null ? "" : String(value);
        g.__fastrender_dom_set_text_content(this.__node_id, v);
      },
      enumerable: true,
      configurable: true,
    });
  } catch (_e) {
    // Ignore; scripts can still call the host functions directly if needed.
  }

  function Element() {}
  Element.prototype = Object.create(Node.prototype);
  Element.prototype.constructor = Element;

  try {
    Object.defineProperty(Element.prototype, "tagName", {
      get: function () {
        return String(g.__fastrender_dom_get_tag_name(this.__node_id));
      },
      enumerable: true,
      configurable: true,
    });
  } catch (_e) {
    // Ignore.
  }

  Element.prototype.getAttribute = function (name) {
    var v = g.__fastrender_dom_get_attribute(this.__node_id, String(name));
    return v == null ? null : String(v);
  };

  Element.prototype.setAttribute = function (name, value) {
    g.__fastrender_dom_set_attribute(this.__node_id, String(name), String(value));
  };

  Element.prototype.removeAttribute = function (name) {
    g.__fastrender_dom_remove_attribute(this.__node_id, String(name));
  };

  Element.prototype.hasAttribute = function (name) {
    return !!g.__fastrender_dom_has_attribute(this.__node_id, String(name));
  };

  Element.prototype.querySelector = function (selectors) {
    var id = g.__fastrender_dom_query_selector(String(selectors), this.__node_id);
    if (id == null) return null;
    return g.__fastrender_wrap_node_id(id, "element");
  };

  Element.prototype.querySelectorAll = function (selectors) {
    var ids = g.__fastrender_dom_query_selector_all(String(selectors), this.__node_id);
    var out = [];
    for (var i = 0; i < ids.length; i++) {
      out.push(g.__fastrender_wrap_node_id(ids[i], "element"));
    }
    return out;
  };

  Element.prototype.matches = function (selectors) {
    return !!g.__fastrender_dom_matches_selector(this.__node_id, String(selectors));
  };

  function defineReflectedString(prop, attr) {
    try {
      Object.defineProperty(Element.prototype, prop, {
        get: function () {
          var v = g.__fastrender_dom_get_attribute(this.__node_id, attr);
          return v == null ? "" : String(v);
        },
        set: function (value) {
          g.__fastrender_dom_set_attribute(this.__node_id, attr, String(value));
        },
        enumerable: true,
        configurable: true,
      });
    } catch (_e) {
      // Ignore.
    }
  }

  function defineReflectedBool(prop, attr) {
    try {
      Object.defineProperty(Element.prototype, prop, {
        get: function () {
          return !!g.__fastrender_dom_has_attribute(this.__node_id, attr);
        },
        set: function (value) {
          g.__fastrender_dom_set_bool_attribute(this.__node_id, attr, !!value);
        },
        enumerable: true,
        configurable: true,
      });
    } catch (_e) {
      // Ignore.
    }
  }

  defineReflectedString("id", "id");
  defineReflectedString("className", "class");
  defineReflectedString("src", "src");
  defineReflectedString("srcset", "srcset");
  defineReflectedString("sizes", "sizes");
  defineReflectedString("href", "href");
  defineReflectedString("rel", "rel");
  defineReflectedString("type", "type");
  defineReflectedString("charset", "charset");
  defineReflectedString("crossOrigin", "crossorigin");
  defineReflectedString("height", "height");
  defineReflectedString("width", "width");
  defineReflectedBool("async", "async");
  defineReflectedBool("defer", "defer");

  // --- classList -------------------------------------------------------------
  //
  // Many bootstrap scripts flip classes (`document.documentElement.classList.add(...)`) to enable
  // JS-dependent styling. Implement a minimal `DOMTokenList` surface backed by the `class`
  // attribute, keeping behavior close enough for common use without implementing the full DOM
  // standard.
  var class_list_cache = typeof WeakMap === "function" ? new WeakMap() : null;

  function split_ascii_whitespace(s) {
    s = s == null ? "" : String(s);
    var out = [];
    var cur = "";
    for (var i = 0; i < s.length; i++) {
      var c = s.charCodeAt(i);
      var is_ws = c === 9 || c === 10 || c === 12 || c === 13 || c === 32;
      if (is_ws) {
        if (cur) {
          out.push(cur);
          cur = "";
        }
      } else {
        cur += s.charAt(i);
      }
    }
    if (cur) out.push(cur);
    return out;
  }

  function validate_token(token) {
    token = String(token);
    if (token.length === 0) throw new g.DOMException("SyntaxError", "SyntaxError");
    for (var i = 0; i < token.length; i++) {
      var c = token.charCodeAt(i);
      if (c === 9 || c === 10 || c === 12 || c === 13 || c === 32) {
        throw new g.DOMException("InvalidCharacterError", "InvalidCharacterError");
      }
    }
    return token;
  }

  function get_class_tokens(el) {
    var cls = g.__fastrender_dom_get_attribute(el.__node_id, "class");
    return split_ascii_whitespace(cls == null ? "" : String(cls));
  }

  function set_class_tokens(el, tokens) {
    var serialized = tokens.join(" ");
    if (serialized) {
      g.__fastrender_dom_set_attribute(el.__node_id, "class", serialized);
    } else {
      g.__fastrender_dom_remove_attribute(el.__node_id, "class");
    }
  }

  function classListFor(el) {
    if (class_list_cache) {
      var cached = class_list_cache.get(el);
      if (cached) return cached;
    }

    var api = {
      contains: function (token) {
        token = validate_token(token);
        var list = get_class_tokens(el);
        return list.indexOf(token) >= 0;
      },
      add: function () {
        var list = get_class_tokens(el);
        for (var i = 0; i < arguments.length; i++) {
          var token = validate_token(arguments[i]);
          if (list.indexOf(token) < 0) list.push(token);
        }
        set_class_tokens(el, list);
      },
      remove: function () {
        var list = get_class_tokens(el);
        for (var i = 0; i < arguments.length; i++) {
          var token = validate_token(arguments[i]);
          var idx;
          while ((idx = list.indexOf(token)) >= 0) list.splice(idx, 1);
        }
        set_class_tokens(el, list);
      },
      toggle: function (token, force) {
        token = validate_token(token);
        var list = get_class_tokens(el);
        var present = list.indexOf(token) >= 0;
        var idx;
        if (force === true) {
          if (!present) list.push(token);
          set_class_tokens(el, list);
          return true;
        }
        if (force === false) {
          while ((idx = list.indexOf(token)) >= 0) list.splice(idx, 1);
          set_class_tokens(el, list);
          return false;
        }
        if (present) {
          while ((idx = list.indexOf(token)) >= 0) list.splice(idx, 1);
          set_class_tokens(el, list);
          return false;
        }
        list.push(token);
        set_class_tokens(el, list);
        return true;
      },
    };

    if (class_list_cache) class_list_cache.set(el, api);
    return api;
  }

  try {
    Object.defineProperty(Element.prototype, "classList", {
      get: function () {
        return classListFor(this);
      },
      enumerable: true,
      configurable: true,
    });
  } catch (_e) {
    // Ignore.
  }

  var dataset_cache = typeof WeakMap === "function" ? new WeakMap() : null;
  function datasetProxyFor(el) {
    if (!dataset_cache) return {};
    var cached = dataset_cache.get(el);
    if (cached) return cached;
    var proxy = new Proxy(
      {},
      {
        get: function (_t, prop) {
          if (typeof prop !== "string") return undefined;
          var v = g.__fastrender_dom_dataset_get(el.__node_id, prop);
          return v == null ? undefined : String(v);
        },
        set: function (_t, prop, value) {
          if (typeof prop !== "string") return false;
          g.__fastrender_dom_dataset_set(el.__node_id, prop, String(value));
          return true;
        },
        deleteProperty: function (_t, prop) {
          if (typeof prop !== "string") return true;
          g.__fastrender_dom_dataset_delete(el.__node_id, prop);
          return true;
        },
      }
    );
    dataset_cache.set(el, proxy);
    return proxy;
  }

  try {
    Object.defineProperty(Element.prototype, "dataset", {
      get: function () {
        return datasetProxyFor(this);
      },
      enumerable: true,
      configurable: true,
    });
  } catch (_e) {
    // Ignore.
  }

  var style_cache = typeof WeakMap === "function" ? new WeakMap() : null;
  function styleProxyFor(el) {
    if (!style_cache) return {};
    var cached = style_cache.get(el);
    if (cached) return cached;

    var style = {
      getPropertyValue: function (name) {
        return String(g.__fastrender_dom_style_get_property_value(el.__node_id, String(name)));
      },
      setProperty: function (name, value) {
        g.__fastrender_dom_style_set_property(el.__node_id, String(name), String(value));
      },
    };

    var proxy = new Proxy(style, {
      get: function (target, prop) {
        if (prop in target) return target[prop];
        if (typeof prop !== "string") return undefined;
        return target.getPropertyValue(prop);
      },
      set: function (target, prop, value) {
        if (typeof prop !== "string") {
          target[prop] = value;
          return true;
        }
        g.__fastrender_dom_style_set_property(el.__node_id, prop, String(value));
        return true;
      },
    });

    style_cache.set(el, proxy);
    return proxy;
  }

  try {
    Object.defineProperty(Element.prototype, "style", {
      get: function () {
        return styleProxyFor(this);
      },
      enumerable: true,
      configurable: true,
    });
  } catch (_e) {
    // Ignore.
  }

  function Text() {}
  Text.prototype = Object.create(Node.prototype);
  Text.prototype.constructor = Text;
  try {
    Object.defineProperty(Text.prototype, "data", {
      get: function () {
        return this.textContent;
      },
      set: function (value) {
        this.textContent = value == null ? "" : String(value);
      },
      enumerable: true,
      configurable: true,
    });
  } catch (_e) {
    // Ignore.
  }

  function Document() {}
  Document.prototype = Object.create(Node.prototype);
  Document.prototype.constructor = Document;

  Document.prototype.createElement = function (tagName) {
    var id = g.__fastrender_dom_create_element(String(tagName));
    return g.__fastrender_wrap_node_id(id, "element");
  };
  Document.prototype.createTextNode = function (data) {
    var id = g.__fastrender_dom_create_text_node(String(data));
    return g.__fastrender_wrap_node_id(id, "text");
  };
  Document.prototype.querySelector = function (selectors) {
    // Pass an explicit `null` scope so the host binding can treat it as "no scope" even if the
    // JS engine doesn't map omitted args to `Option<T>` reliably.
    var id = g.__fastrender_dom_query_selector(String(selectors), null);
    if (id == null) return null;
    return g.__fastrender_wrap_node_id(id, "element");
  };
  Document.prototype.querySelectorAll = function (selectors) {
    var ids = g.__fastrender_dom_query_selector_all(String(selectors), null);
    var out = [];
    for (var i = 0; i < ids.length; i++) {
      out.push(g.__fastrender_wrap_node_id(ids[i], "element"));
    }
    return out;
  };
  Document.prototype.getElementById = function (id) {
    var found = g.__fastrender_dom_get_element_by_id(String(id));
    if (found == null) return null;
    return g.__fastrender_wrap_node_id(found, "element");
  };

  // --- Wrapper constructor ---------------------------------------------------

  g.__fastrender_wrap_node_id = function (id, kind) {
    id = Number(id) >>> 0;
    if (id === 0) {
      ensureNodeBasics(doc, 0);
      try {
        Object.setPrototypeOf(doc, Document.prototype);
      } catch (_e) {
        // Ignore.
      }
      return doc;
    }

    var existing = nodeById.get(id);
    if (existing) return existing;

    var obj = {};
    ensureNodeBasics(obj, id);

    // Default to an element wrapper; queries only expose elements today.
    if (kind === "text") {
      try {
        Object.setPrototypeOf(obj, Text.prototype);
      } catch (_e) {
        // Ignore.
      }
    } else {
      try {
        Object.setPrototypeOf(obj, Element.prototype);
      } catch (_e) {
        // Ignore.
      }
    }
    return obj;
  };

  // Register the document root as node id 0 (dom2::Document::root()).
  g.__fastrender_wrap_node_id(0, "document");
})();
"##;

#[cfg(test)]
  mod tests {
  use super::install_dom_bindings;
  use fastrender::dom2::{Document as Dom2Document, NodeId, NodeKind};
  use fastrender::error::{Error, Result};
  use fastrender::js::{
    CurrentScriptHost, CurrentScriptStateHandle, ScriptBlockExecutor, ScriptOrchestrator, ScriptType,
  };
  use fastrender::js::DomHost;
  use rquickjs::{Context, Runtime};
  use std::cell::RefCell;
  use std::rc::Rc;

  struct Host {
    dom: Dom2Document,
    script_state: CurrentScriptStateHandle,
  }

  impl fastrender::js::DomHost for Host {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&Dom2Document) -> R,
    {
      f(&self.dom)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut Dom2Document) -> (R, bool),
    {
      let (result, _changed) = f(&mut self.dom);
      result
    }
  }

  impl CurrentScriptHost for Host {
    fn current_script_state(&self) -> &CurrentScriptStateHandle {
      &self.script_state
    }
  }

  #[derive(Debug)]
  struct TestDomHost {
    dom: Dom2Document,
  }

  impl DomHost for TestDomHost {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&Dom2Document) -> R,
    {
      f(&self.dom)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut Dom2Document) -> (R, bool),
    {
      let (result, _changed) = f(&mut self.dom);
      result
    }
  }

  fn find_script_by_id(dom: &Dom2Document, id: &str) -> NodeId {
    let mut stack = vec![dom.root()];
    while let Some(node_id) = stack.pop() {
      if let NodeKind::Element { tag_name, .. } = &dom.node(node_id).kind {
        if tag_name.eq_ignore_ascii_case("script")
          && dom
            .get_attribute(node_id, "id")
            .ok()
            .flatten()
            .is_some_and(|v| v == id)
        {
          return node_id;
        }
      }
      for &child in dom.node(node_id).children.iter().rev() {
        stack.push(child);
      }
    }
    panic!("script with id={id} not found")
  }

  fn init_ctx(
    dom: Rc<RefCell<TestDomHost>>,
    script_state: CurrentScriptStateHandle,
  ) -> (Runtime, Context) {
    let rt = Runtime::new().expect("create QuickJS runtime");
    let ctx = Context::full(&rt).expect("create QuickJS context");
    ctx.with(|ctx| {
      let globals = ctx.globals();
      install_dom_bindings(ctx.clone(), &globals, dom, script_state).expect("install bindings");
      ctx
        .eval::<(), _>("globalThis.obs = []")
        .expect("init obs array");
    });
    (rt, ctx)
  }

  struct JsObservingExecutor {
    ctx: Context,
  }

  impl ScriptBlockExecutor<Host> for JsObservingExecutor {
    fn execute_script(
      &mut self,
      _host: &mut Host,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      self
        .ctx
        .with(|ctx| {
          ctx.eval::<(), _>("globalThis.obs.push(document.currentScript && document.currentScript.id)")
        })
        .map_err(|e| Error::Other(e.to_string()))?;
      Ok(())
    }
  }

  struct JsCurrentScriptShapeExecutor {
    ctx: Context,
  }

  impl ScriptBlockExecutor<Host> for JsCurrentScriptShapeExecutor {
    fn execute_script(
      &mut self,
      _host: &mut Host,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      self
        .ctx
        .with(|ctx| {
          ctx.eval::<(), _>(concat!(
            "globalThis.obs.push(document.currentScript && document.currentScript.tagName);",
            "globalThis.obs.push(document.currentScript && typeof document.currentScript.appendChild);",
          ))
        })
        .map_err(|e| Error::Other(e.to_string()))?;
      Ok(())
    }
  }

  fn read_obs(ctx: &Context) -> Vec<Option<String>> {
    ctx
      .with(|ctx| ctx.eval::<Vec<Option<String>>, _>("globalThis.obs"))
      .expect("read obs")
  }

  #[test]
  fn document_current_script_tracks_sequential_script_execution() -> Result<()> {
    let renderer_dom = fastrender::dom::parse_html(
      "<!doctype html><script id=a></script><script id=b></script>",
    )?;
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let dom_for_bindings = Rc::new(RefCell::new(TestDomHost { dom: dom.clone() }));
    let script_a = find_script_by_id(&dom, "a");
    let script_b = find_script_by_id(&dom, "b");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      dom,
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom_for_bindings), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = JsObservingExecutor { ctx };

    orchestrator.execute_script_element(
      &mut host,
      script_a,
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      script_b,
      ScriptType::Classic,
      &mut executor,
    )?;

    assert_eq!(
      read_obs(&executor.ctx),
      vec![Some("a".to_string()), Some("b".to_string())]
    );
    Ok(())
  }

  #[test]
  fn document_current_script_is_element_wrapper() -> Result<()> {
    let renderer_dom = fastrender::dom::parse_html("<!doctype html><script id=a></script>")?;
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let dom_for_bindings = Rc::new(RefCell::new(TestDomHost { dom: dom.clone() }));
    let script_a = find_script_by_id(&dom, "a");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      dom,
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom_for_bindings), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = JsCurrentScriptShapeExecutor { ctx };

    orchestrator.execute_script_element(&mut host, script_a, ScriptType::Classic, &mut executor)?;

    assert_eq!(
      read_obs(&executor.ctx),
      vec![Some("SCRIPT".to_string()), Some("function".to_string())]
    );
    Ok(())
  }

  struct NestedJsExecutor {
    ctx: Context,
    script_a: NodeId,
    script_b: NodeId,
    did_nested: bool,
  }

  impl NestedJsExecutor {
    fn new(ctx: Context, script_a: NodeId, script_b: NodeId) -> Self {
      Self {
        ctx,
        script_a,
        script_b,
        did_nested: false,
      }
    }

    fn observe(&self) -> Result<()> {
      self
        .ctx
        .with(|ctx| {
          ctx.eval::<(), _>("globalThis.obs.push(document.currentScript && document.currentScript.id)")
        })
        .map_err(|e| Error::Other(e.to_string()))?;
      Ok(())
    }
  }

  impl ScriptBlockExecutor<Host> for NestedJsExecutor {
    fn execute_script(
      &mut self,
      host: &mut Host,
      orchestrator: &mut ScriptOrchestrator,
      script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      self.observe()?;
      if script == self.script_a {
        assert!(
          !self.did_nested,
          "nested executor should run nested script only once"
        );
        self.did_nested = true;
        orchestrator.execute_script_element(host, self.script_b, ScriptType::Classic, self)?;
        self.observe()?;
      }
      Ok(())
    }
  }

  #[test]
  fn document_current_script_restores_after_nested_execution() -> Result<()> {
    let renderer_dom = fastrender::dom::parse_html(
      "<!doctype html><script id=a></script><script id=b></script>",
    )?;
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let dom_for_bindings = Rc::new(RefCell::new(TestDomHost { dom: dom.clone() }));
    let script_a = find_script_by_id(&dom, "a");
    let script_b = find_script_by_id(&dom, "b");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      dom,
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom_for_bindings), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = NestedJsExecutor::new(ctx, script_a, script_b);

    orchestrator.execute_script_element(
      &mut host,
      script_a,
      ScriptType::Classic,
      &mut executor,
    )?;

    assert_eq!(
      read_obs(&executor.ctx),
      vec![
        Some("a".to_string()),
        Some("b".to_string()),
        Some("a".to_string())
      ]
    );
    Ok(())
  }

  #[test]
  fn document_current_script_is_null_for_shadow_tree_classic_and_module_scripts() -> Result<()> {
    let renderer_dom = fastrender::dom::parse_html(concat!(
      "<!doctype html>",
      "<div id=host><template shadowroot=open><script id=shadow></script></template></div>",
      "<script id=module type=module></script>",
    ))?;
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let dom_for_bindings = Rc::new(RefCell::new(TestDomHost { dom: dom.clone() }));

    let shadow_script = find_script_by_id(&dom, "shadow");
    let module_script = find_script_by_id(&dom, "module");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      dom,
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom_for_bindings), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = JsObservingExecutor { ctx };

    orchestrator.execute_script_element(
      &mut host,
      shadow_script,
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      module_script,
      ScriptType::Module,
      &mut executor,
    )?;

    assert_eq!(read_obs(&executor.ctx), vec![None, None]);
    Ok(())
  }

  fn eval_str(ctx: &rquickjs::Ctx<'_>, src: &str) -> String {
    ctx.eval::<String, _>(src).expect("eval")
  }

  #[test]
  fn maps_dom_errors_to_domexception_and_selector_errors_to_syntaxerror() -> Result<()> {
    let renderer_dom = fastrender::dom::parse_html("<!doctype html>")?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();

    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);
    ctx.with(|ctx| {
      let out = eval_str(
        &ctx,
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
      );
      assert_eq!(out, "HierarchyRequestError|true");

      let out = eval_str(
        &ctx,
        r#"(() => {
          try {
            const parent = document.createElement("div");
            const child = document.createElement("span");
            parent.removeChild(child);
            return "no throw";
          } catch (e) {
            return String(e.name);
          }
        })()"#,
      );
      assert_eq!(out, "NotFoundError");

      let out = eval_str(
        &ctx,
        r#"(() => {
          try {
            document.querySelector("[");
            return "no throw";
          } catch (e) {
            return String(e.name) + "|" + String(e instanceof SyntaxError);
          }
        })()"#,
      );
      assert_eq!(out, "SyntaxError|true");

      let out = eval_str(
        &ctx,
        r#"(() => {
          try {
            const el = document.createElement("div");
            el.appendChild(123);
            return "no throw";
          } catch (e) {
            return String(e.name);
          }
        })()"#,
      );
      assert_eq!(out, "InvalidNodeType");
    });
    Ok(())
  }

  #[test]
  fn document_head_and_body_expose_tagname_and_appendchild() -> Result<()> {
    let renderer_dom =
      fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let ok = ctx
      .with(|ctx| {
        ctx.eval::<bool, _>(
          r#"(function () {
            if (!document.head || !document.body) return false;
            if (String(document.head.tagName).toUpperCase() !== "HEAD") return false;
            if (String(document.body.tagName).toUpperCase() !== "BODY") return false;

            var child = document.createElement("div");
            var returned = document.body.appendChild(child);
            if (returned !== child) return false;
            if (!document.body.childNodes || document.body.childNodes.length !== 1) return false;
            if (document.body.childNodes[0] !== child) return false;
            return true;
          })()"#,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;

    assert!(ok, "expected head/body bindings to behave");
    Ok(())
  }

  #[test]
  fn node_remove_detaches_from_parent() -> Result<()> {
    let renderer_dom =
      fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let ok = ctx
      .with(|ctx| {
        ctx.eval::<bool, _>(
          r#"(function () {
            var el = document.createElement("div");
            el.id = "gone";
            document.body.appendChild(el);
            if (document.getElementById("gone") !== el) return false;
            if (!document.body.childNodes || document.body.childNodes.length !== 1) return false;
            el.remove();
            if (el.parentNode !== null) return false;
            if (document.body.childNodes.length !== 0) return false;
            if (document.getElementById("gone") !== null) return false;
            return true;
          })()"#,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;
    assert!(ok, "expected Node.remove() to detach from DOM");
    assert!(
      dom.borrow().dom.get_element_by_id("gone").is_none(),
      "expected removed element to be detached in dom2"
    );
    Ok(())
  }

  #[test]
  fn node_insert_before_moves_child_before_reference() -> Result<()> {
    let renderer_dom =
      fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let ok = ctx
      .with(|ctx| {
        ctx.eval::<bool, _>(
          r#"(function () {
            var a = document.createElement("div");
            a.id = "a";
            var b = document.createElement("div");
            b.id = "b";

            document.head.appendChild(a);
            document.body.appendChild(b);

            var returned = document.head.insertBefore(b, a);
            if (returned !== b) return false;

            if (document.getElementById("a") !== a) return false;
            if (document.getElementById("b") !== b) return false;

            if (document.body.childNodes.length !== 0) return false;
            if (document.head.childNodes.length !== 2) return false;
            if (document.head.childNodes[0] !== b) return false;
            if (document.head.childNodes[1] !== a) return false;
            if (b.parentNode !== document.head) return false;
            if (a.parentNode !== document.head) return false;
            return true;
          })()"#,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;
    assert!(ok, "expected Node.insertBefore to update child order");

    let dom_ref = &dom.borrow().dom;
    let head = dom_ref.head().expect("expected head");
    let children = dom_ref.children(head).expect("read head children");
    assert_eq!(children.len(), 2);
    assert_eq!(dom_ref.get_attribute(children[0], "id").unwrap(), Some("b"));
    assert_eq!(dom_ref.get_attribute(children[1], "id").unwrap(), Some("a"));

    let body = dom_ref.body().expect("expected body");
    assert!(dom_ref.children(body).unwrap().is_empty());

    Ok(())
  }

  #[test]
  fn node_replace_child_swaps_nodes_and_detaches_old_child() -> Result<()> {
    let renderer_dom =
      fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let ok = ctx
      .with(|ctx| {
        ctx.eval::<bool, _>(
          r#"(function () {
            var a = document.createElement("div");
            a.id = "a";
            var b = document.createElement("div");
            b.id = "b";
            var c = document.createElement("div");
            c.id = "c";

            document.body.appendChild(a);
            document.body.appendChild(b);

            var returned = document.body.replaceChild(c, a);
            if (returned !== a) return false;
            if (document.getElementById("a") !== null) return false;
            if (document.getElementById("c") !== c) return false;

            if (document.body.childNodes.length !== 2) return false;
            if (document.body.childNodes[0] !== c) return false;
            if (document.body.childNodes[1] !== b) return false;
            if (a.parentNode !== null) return false;
            if (c.parentNode !== document.body) return false;
            return true;
          })()"#,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;
    assert!(ok, "expected Node.replaceChild to swap children");

    let dom_ref = &dom.borrow().dom;
    let body = dom_ref.body().expect("expected body");
    let children = dom_ref.children(body).expect("read body children");
    assert_eq!(children.len(), 2);
    assert_eq!(dom_ref.get_attribute(children[0], "id").unwrap(), Some("c"));
    assert_eq!(dom_ref.get_attribute(children[1], "id").unwrap(), Some("b"));
    assert!(dom_ref.get_element_by_id("a").is_none());

    Ok(())
  }

  #[test]
  fn bootstrap_like_scripts_can_configure_elements_without_throwing() -> Result<()> {
    let renderer_dom =
      fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let ok = ctx
      .with(|ctx| {
        ctx.eval::<bool, _>(
          r##"(function () {
            var s = document.createElement("script");
            s.id = "boot";
            s.src = "https://example.invalid/boot.js";
            s.async = true;
            s.defer = false;
            s.crossOrigin = "anonymous";
            s.dataset.fooBar = "baz";
            s.style.display = "none";
            s.textContent = "console.log('boot')";
            document.head.appendChild(s);

            if (document.getElementById("boot") !== s) return false;
            if (document.querySelector("#boot") !== s) return false;
            return true;
          })()"##,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;
    assert!(ok, "expected bootstrap-like script to run without errors");

    let dom_ref = &dom.borrow().dom;
    let script = dom_ref
      .get_element_by_id("boot")
      .expect("script appended to head should be discoverable");
    assert_eq!(
      dom_ref.get_attribute(script, "src").unwrap(),
      Some("https://example.invalid/boot.js")
    );
    assert!(dom_ref.has_attribute(script, "async").unwrap());
    assert!(!dom_ref.has_attribute(script, "defer").unwrap());
    assert_eq!(
      dom_ref.get_attribute(script, "crossorigin").unwrap(),
      Some("anonymous")
    );
    assert_eq!(
      dom_ref.get_attribute(script, "data-foo-bar").unwrap(),
      Some("baz")
    );
    assert_eq!(
      dom_ref.get_attribute(script, "style").unwrap(),
      Some("display: none;")
    );
    // textContent becomes a single text node child.
    let children = dom_ref.children(script).unwrap();
    assert!(
      children.len() == 1
        && matches!(&dom_ref.node(children[0]).kind, NodeKind::Text { content } if content == "console.log('boot')"),
      "expected script.textContent to create a single text child"
    );

    Ok(())
  }

  #[test]
  fn class_list_add_remove_toggle_updates_class_attribute() -> Result<()> {
    let renderer_dom =
      fastrender::dom::parse_html("<!doctype html><html><body><div id=x class='a b'></div></body></html>")?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let outcome = ctx
      .with(|ctx| {
        ctx.eval::<String, _>(
          r#"(function () {
            try {
              var el = document.getElementById("x");
              el.classList.add("c");
              el.classList.remove("a");
              el.classList.toggle("d");
              el.classList.toggle("d");
              return "ok";
            } catch (e) {
              return String(e && e.name ? e.name : e);
            }
          })()"#,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;
    assert_eq!(outcome, "ok");

    let dom_ref = &dom.borrow().dom;
    let x = dom_ref.get_element_by_id("x").expect("element should exist");
    let class_attr = dom_ref
      .get_attribute(x, "class")
      .expect("get_attribute should succeed")
      .unwrap_or("")
      .to_string();
    let tokens: std::collections::HashSet<&str> = class_attr.split_whitespace().collect();
    assert!(tokens.contains("b"));
    assert!(tokens.contains("c"));
    assert!(!tokens.contains("a"));
    assert!(!tokens.contains("d"));
    Ok(())
  }

  #[test]
  fn query_selector_all_returns_wrappers_and_supports_element_scope() -> Result<()> {
    let renderer_dom = fastrender::dom::parse_html(
      "<!doctype html><html><body><div class=b></div><div class=b></div></body></html>",
    )?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let ok = ctx
      .with(|ctx| {
        ctx.eval::<bool, _>(
          r#"(function () {
            var all = document.querySelectorAll("div.b");
            if (!all || all.length !== 2) return false;
            if (typeof all[0].getAttribute !== "function") return false;

            // Identity is cached by node id, so re-querying should return the same wrapper objects.
            var all2 = document.querySelectorAll("div.b");
            if (all2.length !== 2) return false;
            if (all[0] !== all2[0]) return false;

            // Element-scoped queries should work too.
            var scoped = document.body.querySelectorAll("div.b");
            if (!scoped || scoped.length !== 2) return false;
            if (scoped[0] !== all[0]) return false;

            var first = document.body.querySelector("div.b");
            if (first !== all[0]) return false;
            return true;
          })()"#,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;
    assert!(ok);
    Ok(())
  }

  #[test]
  fn node_remove_detaches_from_dom() -> Result<()> {
    let renderer_dom = fastrender::dom::parse_html(
      "<!doctype html><html><body><div id=parent><span id=child></span></div></body></html>",
    )?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let ok = ctx
      .with(|ctx| {
        ctx.eval::<bool, _>(
          r#"(function () {
            const child = document.getElementById("child");
            if (!child) return false;
            child.remove();

            // Removing an already-detached node is a no-op.
            const detached = document.createElement("div");
            detached.remove();

            return document.getElementById("child") === null && detached.parentNode === null;
          })()"#,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;
    assert!(ok, "expected remove() to detach nodes");

    assert!(
      dom.borrow().dom.get_element_by_id("child").is_none(),
      "expected removed element to be absent from the DOM tree"
    );

    Ok(())
  }

  #[test]
  fn element_matches_selector() -> Result<()> {
    let renderer_dom = fastrender::dom::parse_html(
      "<!doctype html><html><body><div id=x class=a><span id=y></span></div></body></html>",
    )?;
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_state = CurrentScriptStateHandle::default();
    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);

    let ok = ctx
      .with(|ctx| {
        ctx.eval::<bool, _>(
          r#"(function () {
            var x = document.getElementById("x");
            var y = document.getElementById("y");
            if (!x.matches("div.a")) return false;
            if (x.matches("span")) return false;
            if (y.matches("span") !== true) return false;
            if (y.matches("div")) return false;
            return true;
          })()"#,
        )
      })
      .map_err(|e| Error::Other(e.to_string()))?;
    assert!(ok);
    Ok(())
  }
}  
