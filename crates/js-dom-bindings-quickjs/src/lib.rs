//! Minimal `dom2` -> QuickJS bindings.
//!
//! This crate exposes a small subset of DOM APIs required for real-world bootstrap scripts
//! (class flips, basic DOM construction, selector queries) on top of FastRender's `dom2` document.

use std::cell::RefCell;
use std::ffi::CString;
use std::rc::Rc;

use fastrender::dom::HTML_NAMESPACE;
use fastrender::dom2::{DomError, Document, NodeId, NodeKind};
use fastrender::web::dom::DomException;
use rquickjs::class::{Trace, Tracer};
use rquickjs::function::{Args, Constructor, Rest};
use rquickjs::{Ctx, Function, JsLifetime, Object, Result as JsResult, Value};

const NODE_CACHE_GLOBAL: &str = "__fastrender_dom_node_cache";
const NODE_CACHE_FINALIZER_REGISTER_GLOBAL: &str = "__fastrender_dom_node_cache_register_finalizer";
const DOM_EXCEPTION_GLOBAL: &str = "__fastrender_throw_dom_exception";
const CHILD_NODES_CACHE_PROP: &str = "__fastrender_childNodes";

// Minimal DOMException polyfill used for spec-shaped error reporting (e.g. HierarchyRequestError).
//
// QuickJS does not ship a DOMException intrinsic; we install a small JS class that matches
// the Error shape used by WPT + real-world scripts.
const DOM_EXCEPTION_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.DOMException !== "function") {
    g.DOMException = class DOMException extends Error {
      constructor(message, name) {
        super(message === undefined ? "" : String(message));
        this.name = name === undefined ? "Error" : String(name);
      }
    };
  }
  g.__fastrender_throw_dom_exception = function (name, message) {
    throw new g.DOMException(message, name);
  };
 })();
 "#;

// rquickjs maps Rust `Option<T>` return values to `undefined` when `None`. Many DOM APIs are
// specified to return `null` (e.g. `querySelector`, `getElementById`, `parentNode`). Patch the
// prototype so callers observe spec-shaped `null` instead of `undefined`.
const DOM_NULLABLE_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  function nullify(v) { return v === undefined ? null : v; }

  function wrapMethod(proto, name) {
    try {
      var orig = proto[name];
      if (typeof orig !== "function") return;
      proto[name] = function () {
        return nullify(orig.apply(this, arguments));
      };
    } catch (_e) {}
  }

  function findProtoGetter(obj, name) {
    var proto = Object.getPrototypeOf(obj);
    while (proto) {
      var desc = Object.getOwnPropertyDescriptor(proto, name);
      if (desc && typeof desc.get === "function") return desc.get;
      proto = Object.getPrototypeOf(proto);
    }
    return null;
  }

  // Patch nullable accessors by defining an own-property getter on the node wrapper object. This
  // avoids relying on the underlying prototype descriptor being configurable.
  g.__fastrender_patch_node_nullables = function (obj) {
    try {
      if (!obj) return;
      function shadowCachedChildNodes() {
        var getter = findProtoGetter(obj, "childNodes");
        if (!getter) return;
        Object.defineProperty(obj, "childNodes", {
          get: function () {
            if (this.__fastrender_childNodes !== undefined) return this.__fastrender_childNodes;
            var v = getter.call(this);
            try {
              Object.defineProperty(this, "__fastrender_childNodes", {
                value: v,
                writable: true,
                enumerable: false,
                configurable: true,
              });
            } catch (_e) {
              this.__fastrender_childNodes = v;
            }
            return v;
          },
          enumerable: true,
          configurable: true,
        });
      }
      function shadowGetter(name) {
        var getter = findProtoGetter(obj, name);
        if (!getter) return;
        Object.defineProperty(obj, name, {
          get: function () { return nullify(getter.call(this)); },
          enumerable: true,
          configurable: true,
        });
      }

      // Live-ish NodeList facade: cache the first `childNodes` array we create so stored references
      // observe updates when DOM mutation methods run.
      shadowCachedChildNodes();

      // Node traversal.
      shadowGetter("parentNode");
      shadowGetter("parentElement");
      shadowGetter("firstChild");
      shadowGetter("lastChild");
      shadowGetter("firstElementChild");
      shadowGetter("lastElementChild");
      shadowGetter("previousSibling");
      shadowGetter("nextSibling");
      shadowGetter("previousElementSibling");
      shadowGetter("nextElementSibling");

      // Document getters.
      shadowGetter("documentElement");
      shadowGetter("head");
      shadowGetter("body");
      shadowGetter("currentScript");
    } catch (_e) {}
  };

  try {
    if (!g.document) return;
    var proto = Object.getPrototypeOf(g.document);
    if (!proto) return;

    // Nullable methods.
    wrapMethod(proto, "getElementById");
    wrapMethod(proto, "getAttribute");
    wrapMethod(proto, "querySelector");
    wrapMethod(proto, "closest");
  } catch (_e) {}
})();
"#;

fn selector_mentions_scope(selectors: &str) -> bool {
  selectors
    .as_bytes()
    .windows(6)
    .any(|w| w.eq_ignore_ascii_case(b":scope"))
}

// Best-effort node wrapper cache cleanup: avoid leaving unbounded dead keys in the cache after
// wrapper objects are garbage collected.
//
// We store `NodeId -> WeakRef(wrapper)` in `__fastrender_dom_node_cache`. Without cleanup, hostile
// scripts could create/throw away unbounded numbers of nodes and leave behind dead cache entries.
//
// QuickJS supports `FinalizationRegistry` when the WeakRef intrinsic is installed; if present, we
// register wrappers so the cache can delete keys once the WeakRef is cleared.
//
// Note: the finalizer callback double-checks the cache's current WeakRef for the key and only
// deletes when it derefs to `null`/`undefined`, so it won't race with a newer wrapper for the same
// `NodeId`.
const NODE_CACHE_FINALIZER_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.FinalizationRegistry !== "function") return;
  if (g.__fastrender_dom_node_cache_finalizer) return;
  var cache = g.__fastrender_dom_node_cache;
  if (!cache) return;

  g.__fastrender_dom_node_cache_finalizer = new g.FinalizationRegistry(function (key) {
    try {
      var wr = cache[key];
      if (wr && typeof wr.deref === "function") {
        if (wr.deref() == null) {
          delete cache[key];
        }
      } else {
        delete cache[key];
      }
    } catch (_e) {
      // Ignore.
    }
  });

  g.__fastrender_dom_node_cache_register_finalizer = function (obj, key) {
    try {
      g.__fastrender_dom_node_cache_finalizer.register(obj, key);
    } catch (_e) {
      // Ignore.
    }
  };
})();
"#;

/// Install DOM bindings into a QuickJS context.
///
/// The bindings are intentionally minimal and are designed to support common site bootstraps:
/// element creation, tree mutation, selector queries, and `classList`.
pub fn install_dom_bindings<'js>(
  ctx: Ctx<'js>,
  dom: Rc<RefCell<Document>>,
) -> JsResult<()> {
  ensure_weakref_intrinsic(&ctx)?;
  ctx.eval::<(), _>(DOM_EXCEPTION_SHIM)?;

  if ctx.globals().contains_key("document")? {
    return Err(throw_type_error(&ctx, "DOM bindings already installed"));
  }

  let root = dom.borrow().root();
  let state = Rc::new(DomState { dom });

  // Node wrapper cache (NodeId -> WeakRef(object)). Stored in JS so it is guaranteed to be
  // collected before `Runtime` teardown (avoids `JS_FreeRuntime` assertions).
  ctx
    .globals()
    .set(NODE_CACHE_GLOBAL, Object::new(ctx.clone())?)?;
  // Best-effort: install a `FinalizationRegistry` hook so dead cache keys can be deleted once
  // wrappers are garbage collected.
  ctx.eval::<(), _>(NODE_CACHE_FINALIZER_SHIM)?;

  // Create the `document` global.
  let document_obj = state.wrap_node(ctx.clone(), root)?;
  ctx.globals().set("document", document_obj.clone())?;
  ctx.eval::<(), _>(DOM_NULLABLE_SHIM)?;
  if let Ok(Some(patch_fn)) =
    ctx.globals().get::<_, Option<Function<'js>>>("__fastrender_patch_node_nullables")
  {
    // Best-effort; if the shim fails to patch, callers will still see `undefined` which most
    // scripts treat as nullish.
    let _ = patch_fn.call::<_, ()>((document_obj,));
  }

  Ok(())
}

struct DomState {
  dom: Rc<RefCell<Document>>,
}

impl DomState {
  fn wrap_node<'js>(self: &Rc<Self>, ctx: Ctx<'js>, node_id: NodeId) -> JsResult<Object<'js>> {
    let cache: Option<Object<'js>> = ctx.globals().get(NODE_CACHE_GLOBAL)?;
    let Some(cache) = cache else {
      return Err(throw_type_error(&ctx, "DOM bindings not installed"));
    };
    let key = node_id.index().to_string();

    let cached: Option<Object<'js>> = cache.get(key.as_str())?;
    if let Some(weakref_obj) = cached {
      if let Some(obj) = weakref_deref(&ctx, weakref_obj)? {
        return Ok(obj);
      }
    }

    let inst = rquickjs::Class::instance(
      ctx.clone(),
      Node {
        state: Rc::clone(self),
        node_id,
      },
    )?;
    let obj: Object<'js> = inst.into_inner();
    if let Ok(Some(patch_fn)) =
      ctx.globals().get::<_, Option<Function<'js>>>("__fastrender_patch_node_nullables")
    {
      let _ = patch_fn.call::<_, ()>((obj.clone(),));
    }
    let weakref_obj = weakref_new(&ctx, obj.clone())?;
    cache.set(key.as_str(), weakref_obj)?;
    register_cache_finalizer(&ctx, key.as_str(), &obj)?;
    Ok(obj)
  }

  fn cached_node_wrapper<'js>(
    self: &Rc<Self>,
    ctx: Ctx<'js>,
    node_id: NodeId,
  ) -> JsResult<Option<Object<'js>>> {
    let cache: Option<Object<'js>> = ctx.globals().get(NODE_CACHE_GLOBAL)?;
    let Some(cache) = cache else {
      return Ok(None);
    };
    let key = node_id.index().to_string();
    let cached: Option<Object<'js>> = cache.get(key.as_str())?;
    let Some(weakref_obj) = cached else {
      return Ok(None);
    };
    weakref_deref(&ctx, weakref_obj)
  }

  fn maybe_sync_cached_child_nodes<'js>(
    self: &Rc<Self>,
    ctx: Ctx<'js>,
    node_id: NodeId,
  ) -> JsResult<()> {
    let Some(wrapper) = self.cached_node_wrapper(ctx.clone(), node_id)? else {
      return Ok(());
    };

    let cached_val: Value<'js> = wrapper.get(CHILD_NODES_CACHE_PROP)?;
    let Some(array) = cached_val.into_object() else {
      return Ok(());
    };

    let child_ids = {
      let dom = self.dom.borrow();
      if node_id.index() >= dom.nodes_len() {
        return Ok(());
      }
      direct_child_nodes(&dom, node_id).map_err(|e| dom_error_to_js(&ctx, e))?
    };

    // Update the cached array in place so stored JS references behave like a live NodeList.
    array.set("length", 0u32)?;
    for (idx, child_id) in child_ids.into_iter().enumerate() {
      let child_obj = self.wrap_node(ctx.clone(), child_id)?;
      let key = idx.to_string();
      array.set(key.as_str(), child_obj)?;
    }
    Ok(())
  }
}

#[derive(Clone)]
#[rquickjs::class]
pub struct Node {
  state: Rc<DomState>,
  node_id: NodeId,
}

// This wrapper only stores Rust data (no JS references).
impl<'js> Trace<'js> for Node {
  fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for Node {
  type Changed<'to> = Self;
}

#[rquickjs::methods]
impl Node {
  // ===========================================================================
  // Node traversal
  // ===========================================================================

  #[qjs(get, rename = "nodeType")]
  fn node_type<'js>(&self, ctx: Ctx<'js>) -> JsResult<i32> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    Ok(node_type(&dom, self.node_id))
  }

  #[qjs(get, rename = "nodeName")]
  fn node_name<'js>(&self, ctx: Ctx<'js>) -> JsResult<String> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    Ok(node_name(&dom, self.node_id))
  }

  #[qjs(get, rename = "parentNode")]
  fn parent_node<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let id = self.state.dom.borrow().parent_node(self.node_id);
    id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(get, rename = "firstChild")]
  fn first_child<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let id = self.state.dom.borrow().first_child(self.node_id);
    id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(get, rename = "lastChild")]
  fn last_child<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let id = self.state.dom.borrow().last_child(self.node_id);
    id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(get, rename = "previousSibling")]
  fn previous_sibling<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let id = self.state.dom.borrow().previous_sibling(self.node_id);
    id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(get, rename = "nextSibling")]
  fn next_sibling<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let id = self.state.dom.borrow().next_sibling(self.node_id);
    id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(get, rename = "childNodes")]
  fn child_nodes<'js>(&self, ctx: Ctx<'js>) -> JsResult<Vec<Object<'js>>> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    let children: Vec<NodeId> =
      direct_child_nodes(&dom, self.node_id).map_err(|e| dom_error_to_js(&ctx, e))?;
    drop(dom);

    children
      .into_iter()
      .map(|id| self.state.wrap_node(ctx.clone(), id))
      .collect()
  }

  #[qjs(get, rename = "parentElement")]
  fn parent_element<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    let parent = dom.parent_node(self.node_id).filter(|&parent_id| {
      matches!(
        dom.node(parent_id).kind,
        NodeKind::Element { .. } | NodeKind::Slot { .. }
      )
    });
    drop(dom);
    parent
      .map(|id| self.state.wrap_node(ctx, id))
      .transpose()
  }

  #[qjs(get, rename = "children")]
  fn element_children<'js>(&self, ctx: Ctx<'js>) -> JsResult<Vec<Object<'js>>> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    let child_ids: Vec<NodeId> = direct_child_nodes(&dom, self.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?
      .into_iter()
      .filter(|&id| {
        matches!(
          dom.node(id).kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
      })
      .collect();
    drop(dom);
    child_ids
      .into_iter()
      .map(|id| self.state.wrap_node(ctx.clone(), id))
      .collect()
  }

  #[qjs(get, rename = "childElementCount")]
  fn child_element_count<'js>(&self, ctx: Ctx<'js>) -> JsResult<i32> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    let count = direct_child_nodes(&dom, self.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?
      .into_iter()
      .filter(|&id| {
        matches!(
          dom.node(id).kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
      })
      .count();
    Ok(count as i32)
  }

  #[qjs(get, rename = "firstElementChild")]
  fn first_element_child<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    let found = direct_child_nodes(&dom, self.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?
      .into_iter()
      .find(|&id| {
        matches!(
          dom.node(id).kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
      });
    drop(dom);
    found
      .map(|id| self.state.wrap_node(ctx, id))
      .transpose()
  }

  #[qjs(get, rename = "lastElementChild")]
  fn last_element_child<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    let found = direct_child_nodes(&dom, self.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?
      .into_iter()
      .rev()
      .find(|&id| {
        matches!(
          dom.node(id).kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
      });
    drop(dom);
    found
      .map(|id| self.state.wrap_node(ctx, id))
      .transpose()
  }

  #[qjs(get, rename = "previousElementSibling")]
  fn previous_element_sibling<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    let Some(parent) = dom.parent_node(self.node_id) else {
      return Ok(None);
    };
    let siblings = dom.children(parent).map_err(|e| dom_error_to_js(&ctx, e))?;
    let Some(pos) = siblings.iter().position(|&c| c == self.node_id) else {
      return Ok(None);
    };
    let mut found: Option<NodeId> = None;
    for &sib in siblings.iter().take(pos).rev() {
      if sib.index() >= dom.nodes_len() {
        continue;
      }
      let node = dom.node(sib);
      if node.parent != Some(parent) {
        continue;
      }
      if matches!(node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }) {
        found = Some(sib);
        break;
      }
    }
    drop(dom);
    found
      .map(|id| self.state.wrap_node(ctx, id))
      .transpose()
  }

  #[qjs(get, rename = "nextElementSibling")]
  fn next_element_sibling<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    let Some(parent) = dom.parent_node(self.node_id) else {
      return Ok(None);
    };
    let siblings = dom.children(parent).map_err(|e| dom_error_to_js(&ctx, e))?;
    let Some(pos) = siblings.iter().position(|&c| c == self.node_id) else {
      return Ok(None);
    };
    let mut found: Option<NodeId> = None;
    for &sib in siblings.iter().skip(pos + 1) {
      if sib.index() >= dom.nodes_len() {
        continue;
      }
      let node = dom.node(sib);
      if node.parent != Some(parent) {
        continue;
      }
      if matches!(node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }) {
        found = Some(sib);
        break;
      }
    }
    drop(dom);
    found
      .map(|id| self.state.wrap_node(ctx, id))
      .transpose()
  }

  // ===========================================================================
  // Node mutation
  // ===========================================================================

  #[qjs(rename = "appendChild")]
  fn append_child<'js>(&self, ctx: Ctx<'js>, child: Node) -> JsResult<Object<'js>> {
    let old_parent = self.state.dom.borrow().parent_node(child.node_id);
    self
      .state
      .dom
      .borrow_mut()
      .append_child(self.node_id, child.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    self
      .state
      .maybe_sync_cached_child_nodes(ctx.clone(), self.node_id)?;
    if let Some(old_parent) = old_parent {
      if old_parent != self.node_id {
        self
          .state
          .maybe_sync_cached_child_nodes(ctx.clone(), old_parent)?;
      }
    }
    self.state.wrap_node(ctx, child.node_id)
  }

  #[qjs(rename = "insertBefore")]
  fn insert_before<'js>(
    &self,
    ctx: Ctx<'js>,
    new_child: Node,
    reference_child: Option<Node>,
  ) -> JsResult<Object<'js>> {
    let old_parent = self.state.dom.borrow().parent_node(new_child.node_id);
    self
      .state
      .dom
      .borrow_mut()
      .insert_before(
        self.node_id,
        new_child.node_id,
        reference_child.map(|n| n.node_id),
      )
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    self
      .state
      .maybe_sync_cached_child_nodes(ctx.clone(), self.node_id)?;
    if let Some(old_parent) = old_parent {
      if old_parent != self.node_id {
        self
          .state
          .maybe_sync_cached_child_nodes(ctx.clone(), old_parent)?;
      }
    }
    self.state.wrap_node(ctx, new_child.node_id)
  }

  #[qjs(rename = "removeChild")]
  fn remove_child<'js>(&self, ctx: Ctx<'js>, child: Node) -> JsResult<Object<'js>> {
    self
      .state
      .dom
      .borrow_mut()
      .remove_child(self.node_id, child.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    self
      .state
      .maybe_sync_cached_child_nodes(ctx.clone(), self.node_id)?;
    self.state.wrap_node(ctx, child.node_id)
  }

  #[qjs(rename = "remove")]
  fn remove<'js>(&self, ctx: Ctx<'js>) -> JsResult<()> {
    let mut dom = self.state.dom.borrow_mut();
    let Some(parent) = dom.parent_node(self.node_id) else {
      return Ok(());
    };
    dom
      .remove_child(parent, self.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    drop(dom);
    self
      .state
      .maybe_sync_cached_child_nodes(ctx, parent)?;
    Ok(())
  }

  #[qjs(rename = "replaceChild")]
  fn replace_child<'js>(
    &self,
    ctx: Ctx<'js>,
    new_child: Node,
    old_child: Node,
  ) -> JsResult<Object<'js>> {
    let old_parent = self.state.dom.borrow().parent_node(new_child.node_id);
    self
      .state
      .dom
      .borrow_mut()
      .replace_child(self.node_id, new_child.node_id, old_child.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    self
      .state
      .maybe_sync_cached_child_nodes(ctx.clone(), self.node_id)?;
    if let Some(old_parent) = old_parent {
      if old_parent != self.node_id {
        self
          .state
          .maybe_sync_cached_child_nodes(ctx.clone(), old_parent)?;
      }
    }
    self.state.wrap_node(ctx, old_child.node_id)
  }

  // ===========================================================================
  // textContent
  // ===========================================================================

  #[qjs(get, rename = "textContent")]
  fn text_content_get<'js>(&self, ctx: Ctx<'js>) -> JsResult<String> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    Ok(text_content(&dom, self.node_id))
  }

  #[qjs(set, rename = "textContent")]
  fn text_content_set<'js>(&self, ctx: Ctx<'js>, value: String) -> JsResult<()> {
    let mut dom = self.state.dom.borrow_mut();
    ensure_node_exists(&ctx, &dom, self.node_id)?;

    match &mut dom.node_mut(self.node_id).kind {
      NodeKind::Text { content } | NodeKind::Comment { content } => {
        content.clear();
        content.push_str(&value);
        return Ok(());
      }
      NodeKind::ProcessingInstruction { data, .. } => {
        data.clear();
        data.push_str(&value);
        return Ok(());
      }
      NodeKind::Doctype { .. } => {
        // `DocumentType.textContent` is `null` in the DOM spec; setting it is a no-op.
        return Ok(());
      }
      NodeKind::Document { .. }
      | NodeKind::Element { .. }
      | NodeKind::Slot { .. }
      | NodeKind::ShadowRoot { .. } => {
        // Replace children.
      }
    }

    let children: Vec<NodeId> = dom
      .children(self.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?
      .to_vec();
    for child in children {
      dom
        .remove_child(self.node_id, child)
        .map_err(|e| dom_error_to_js(&ctx, e))?;
    }

    if !value.is_empty() {
      let text = dom.create_text(&value);
      dom
        .append_child(self.node_id, text)
        .map_err(|e| dom_error_to_js(&ctx, e))?;
    }

    drop(dom);
    self
      .state
      .maybe_sync_cached_child_nodes(ctx, self.node_id)?;
    Ok(())
  }

  // ===========================================================================
  // Document methods (only valid on the document node)
  // ===========================================================================

  #[qjs(get, rename = "documentElement")]
  fn document_element<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    self.ensure_document(ctx.clone())?;
    let id = self.state.dom.borrow().document_element();
    id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(get, rename = "head")]
  fn head<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    self.ensure_document(ctx.clone())?;
    let id = self.state.dom.borrow().head();
    id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(get, rename = "body")]
  fn body<'js>(&self, ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    self.ensure_document(ctx.clone())?;
    let id = self.state.dom.borrow().body();
    id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(rename = "createElement")]
  fn create_element<'js>(&self, ctx: Ctx<'js>, tag_name: String) -> JsResult<Object<'js>> {
    self.ensure_document(ctx.clone())?;
    let id = self.state.dom.borrow_mut().create_element(&tag_name, "");
    self.state.wrap_node(ctx, id)
  }

  #[qjs(rename = "createTextNode")]
  fn create_text_node<'js>(&self, ctx: Ctx<'js>, data: String) -> JsResult<Object<'js>> {
    self.ensure_document(ctx.clone())?;
    let id = self.state.dom.borrow_mut().create_text(&data);
    self.state.wrap_node(ctx, id)
  }

  #[qjs(rename = "createComment")]
  fn create_comment<'js>(&self, ctx: Ctx<'js>, data: String) -> JsResult<Object<'js>> {
    self.ensure_document(ctx.clone())?;
    let id = self.state.dom.borrow_mut().create_comment(&data);
    self.state.wrap_node(ctx, id)
  }

  #[qjs(rename = "getElementById")]
  fn get_element_by_id<'js>(&self, ctx: Ctx<'js>, id: String) -> JsResult<Option<Object<'js>>> {
    self.ensure_document(ctx.clone())?;
    let node_id = self.state.dom.borrow().get_element_by_id(&id);
    node_id.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(rename = "querySelector")]
  fn query_selector<'js>(
    &self,
    ctx: Ctx<'js>,
    selectors: String,
  ) -> JsResult<Option<Object<'js>>> {
    let allow_scope = selector_mentions_scope(&selectors);
    let (scope, filter_self) = {
      let dom = self.state.dom.borrow();
      match &dom.node(self.node_id).kind {
        NodeKind::Document { .. } => (None, false),
        NodeKind::Element { .. } | NodeKind::Slot { .. } => (Some(self.node_id), !allow_scope),
        _ => return Err(dom_error_to_js(&ctx, DomError::InvalidNodeType)),
      }
    };

    // `dom2` selector engines treat the scope element itself as a candidate; `Element#querySelector`
    // must only consider descendants for non-`:scope` selectors. Filter `self.node_id` out of
    // results when scoping to an element-like node unless the selector references `:scope`.
    if filter_self {
      let ids = {
        let mut dom = self.state.dom.borrow_mut();
        dom
          .query_selector_all(&selectors, scope)
          .map_err(|e| dom_exception_to_js(&ctx, e))?
      };
      let found = ids.into_iter().find(|id| *id != self.node_id);
      return found.map(|id| self.state.wrap_node(ctx, id)).transpose();
    }

    let found = {
      let mut dom = self.state.dom.borrow_mut();
      dom
        .query_selector(&selectors, scope)
        .map_err(|e| dom_exception_to_js(&ctx, e))?
    };
    found.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(rename = "querySelectorAll")]
  fn query_selector_all<'js>(
    &self,
    ctx: Ctx<'js>,
    selectors: String,
  ) -> JsResult<Vec<Object<'js>>> {
    let allow_scope = selector_mentions_scope(&selectors);
    let (scope, filter_self) = {
      let dom = self.state.dom.borrow();
      match &dom.node(self.node_id).kind {
        NodeKind::Document { .. } => (None, false),
        NodeKind::Element { .. } | NodeKind::Slot { .. } => (Some(self.node_id), !allow_scope),
        _ => return Err(dom_error_to_js(&ctx, DomError::InvalidNodeType)),
      }
    };
    let ids = {
      let mut dom = self.state.dom.borrow_mut();
      dom
        .query_selector_all(&selectors, scope)
        .map_err(|e| dom_exception_to_js(&ctx, e))?
    };
    ids
      .into_iter()
      .filter(|id| !filter_self || *id != self.node_id)
      .map(|id| self.state.wrap_node(ctx.clone(), id))
      .collect()
  }

  #[qjs(get, rename = "currentScript")]
  fn current_script<'js>(&self, _ctx: Ctx<'js>) -> JsResult<Option<Object<'js>>> {
    // Stub for now (HTML script processing wiring is handled by a separate task).
    Ok(None)
  }

  // ===========================================================================
  // Element methods/properties (only valid on element-like nodes)
  // ===========================================================================

  #[qjs(get, rename = "tagName")]
  fn tag_name<'js>(&self, ctx: Ctx<'js>) -> JsResult<String> {
    self.ensure_element(ctx.clone())?;
    let dom = self.state.dom.borrow();
    match &dom.node(self.node_id).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => Ok(if namespace.is_empty() || namespace == HTML_NAMESPACE {
        tag_name.to_ascii_uppercase()
      } else {
        tag_name.clone()
      }),
      NodeKind::Slot { .. } => Ok("SLOT".to_string()),
      _ => Err(dom_error_to_js(&ctx, DomError::InvalidNodeType)),
    }
  }

  #[qjs(rename = "getAttribute")]
  fn get_attribute<'js>(&self, ctx: Ctx<'js>, name: String) -> JsResult<Option<String>> {
    self.ensure_element(ctx.clone())?;
    let dom = self.state.dom.borrow();
    let value = dom
      .get_attribute(self.node_id, &name)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    Ok(value.map(|v| v.to_string()))
  }

  #[qjs(rename = "setAttribute")]
  fn set_attribute<'js>(&self, ctx: Ctx<'js>, name: String, value: String) -> JsResult<()> {
    self.ensure_element(ctx.clone())?;
    self
      .state
      .dom
      .borrow_mut()
      .set_attribute(self.node_id, &name, &value)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    Ok(())
  }

  #[qjs(rename = "removeAttribute")]
  fn remove_attribute<'js>(&self, ctx: Ctx<'js>, name: String) -> JsResult<()> {
    self.ensure_element(ctx.clone())?;
    self
      .state
      .dom
      .borrow_mut()
      .remove_attribute(self.node_id, &name)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    Ok(())
  }

  #[qjs(get, rename = "id")]
  fn id_get<'js>(&self, ctx: Ctx<'js>) -> JsResult<String> {
    self.ensure_element(ctx.clone())?;
    let dom = self.state.dom.borrow();
    let id = dom.id(self.node_id).map_err(|e| dom_error_to_js(&ctx, e))?;
    Ok(id.unwrap_or("").to_string())
  }

  #[qjs(set, rename = "id")]
  fn id_set<'js>(&self, ctx: Ctx<'js>, value: String) -> JsResult<()> {
    self.ensure_element(ctx.clone())?;
    self
      .state
      .dom
      .borrow_mut()
      .set_attribute(self.node_id, "id", &value)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    Ok(())
  }

  #[qjs(get, rename = "className")]
  fn class_name_get<'js>(&self, ctx: Ctx<'js>) -> JsResult<String> {
    self.ensure_element(ctx.clone())?;
    let dom = self.state.dom.borrow();
    let class = dom
      .class_name(self.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    Ok(class.unwrap_or("").to_string())
  }

  #[qjs(set, rename = "className")]
  fn class_name_set<'js>(&self, ctx: Ctx<'js>, value: String) -> JsResult<()> {
    self.ensure_element(ctx.clone())?;
    self
      .state
      .dom
      .borrow_mut()
      .set_attribute(self.node_id, "class", &value)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    Ok(())
  }

  #[qjs(get, rename = "classList")]
  fn class_list<'js>(&self, ctx: Ctx<'js>) -> JsResult<Object<'js>> {
    self.ensure_element(ctx.clone())?;
    let inst = rquickjs::Class::instance(
      ctx.clone(),
      DOMTokenList {
        state: Rc::clone(&self.state),
        element_id: self.node_id,
      },
    )?;
    Ok(inst.into_inner())
  }

  #[qjs(rename = "matches")]
  fn matches_selectors<'js>(&self, ctx: Ctx<'js>, selectors: String) -> JsResult<bool> {
    self.ensure_element(ctx.clone())?;
    let result = {
      let mut dom = self.state.dom.borrow_mut();
      dom
        .matches_selector(self.node_id, &selectors)
        .map_err(|e| dom_exception_to_js(&ctx, e))?
    };
    Ok(result)
  }

  #[qjs(rename = "closest")]
  fn closest<'js>(&self, ctx: Ctx<'js>, selectors: String) -> JsResult<Option<Object<'js>>> {
    self.ensure_element(ctx.clone())?;
    let found = {
      let mut dom = self.state.dom.borrow_mut();
      dom
        .closest(self.node_id, &selectors)
        .map_err(|e| dom_exception_to_js(&ctx, e))?
    };
    found.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  // ===========================================================================
  // Internal helpers
  // ===========================================================================

  fn ensure_document<'js>(&self, ctx: Ctx<'js>) -> JsResult<()> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    if matches!(&dom.node(self.node_id).kind, NodeKind::Document { .. }) {
      return Ok(());
    }
    Err(dom_error_to_js(&ctx, DomError::InvalidNodeType))
  }

  fn ensure_element<'js>(&self, ctx: Ctx<'js>) -> JsResult<()> {
    let dom = self.state.dom.borrow();
    ensure_node_exists(&ctx, &dom, self.node_id)?;
    match &dom.node(self.node_id).kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => Ok(()),
      _ => Err(dom_error_to_js(&ctx, DomError::InvalidNodeType)),
    }
  }
}

#[derive(Clone)]
#[rquickjs::class]
pub struct DOMTokenList {
  state: Rc<DomState>,
  element_id: NodeId,
}

impl<'js> Trace<'js> for DOMTokenList {
  fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for DOMTokenList {
  type Changed<'to> = Self;
}

#[rquickjs::methods]
impl DOMTokenList {
  #[qjs(rename = "contains")]
  fn contains_token<'js>(&self, ctx: Ctx<'js>, token: String) -> JsResult<bool> {
    validate_token_or_throw(&ctx, &token)?;
    let class = self
      .state
      .dom
      .borrow()
      .class_name(self.element_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?
      .unwrap_or("")
      .to_string();
    let contains = split_classes(&class).any(|t| t == token);
    Ok(contains)
  }

  #[qjs(rename = "add")]
  fn add_tokens<'js>(&self, ctx: Ctx<'js>, tokens: Rest<String>) -> JsResult<()> {
    let ctx2 = ctx.clone();
    let tokens = tokens.0;
    self.update(ctx, move |list| {
      for token in tokens {
        validate_token_or_throw(&ctx2, &token)?;
        if !list.iter().any(|t| t == &token) {
          list.push(token);
        }
      }
      Ok(())
    })
  }

  #[qjs(rename = "remove")]
  fn remove_tokens<'js>(&self, ctx: Ctx<'js>, tokens: Rest<String>) -> JsResult<()> {
    let ctx2 = ctx.clone();
    let tokens = tokens.0;
    self.update(ctx, move |list| {
      for token in tokens {
        validate_token_or_throw(&ctx2, &token)?;
        list.retain(|t| t != &token);
      }
      Ok(())
    })
  }

  #[qjs(rename = "toggle")]
  fn toggle_token<'js>(
    &self,
    ctx: Ctx<'js>,
    token: String,
    force: Rest<bool>,
  ) -> JsResult<bool> {
    validate_token_or_throw(&ctx, &token)?;

    let force = force.0.get(0).copied();
    let mut result = false;
    self.update(ctx, |list| {
      let present = list.iter().any(|t| t == &token);
      match force {
        Some(true) => {
          if !present {
            list.push(token.clone());
          }
          result = true;
        }
        Some(false) => {
          if present {
            list.retain(|t| t != &token);
          }
          result = false;
        }
        None => {
          if present {
            list.retain(|t| t != &token);
            result = false;
          } else {
            list.push(token.clone());
            result = true;
          }
        }
      }
      Ok(())
    })?;

    Ok(result)
  }
}

impl DOMTokenList {
  fn update<'js, F>(&self, ctx: Ctx<'js>, f: F) -> JsResult<()>
  where
    F: FnOnce(&mut Vec<String>) -> JsResult<()>,
  {
    let current = self
      .state
      .dom
      .borrow()
      .class_name(self.element_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?
      .unwrap_or("")
      .to_string();
    let mut list: Vec<String> = split_classes(&current).map(|s| s.to_string()).collect();
    f(&mut list)?;
    let serialized = list.join(" ");
    self
      .state
      .dom
      .borrow_mut()
      .set_attribute(self.element_id, "class", &serialized)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    Ok(())
  }
}

fn ensure_weakref_intrinsic<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
  // `Context::full` doesn't necessarily include the WeakRef intrinsic; ensure it is present since
  // we use it to implement wrapper identity caching without leaking JS objects.
  if !ctx.globals().contains_key("WeakRef")? {
    unsafe {
      rquickjs::qjs::JS_AddIntrinsicWeakRef(ctx.as_raw().as_ptr());
    }
  }
  Ok(())
}

fn weakref_new<'js>(ctx: &Ctx<'js>, target: Object<'js>) -> JsResult<Object<'js>> {
  let ctor: Constructor<'js> = ctx.globals().get("WeakRef")?;
  ctor.construct((target,))
}

fn weakref_deref<'js>(ctx: &Ctx<'js>, weakref: Object<'js>) -> JsResult<Option<Object<'js>>> {
  let deref_fn: Function<'js> = weakref.get("deref")?;
  let mut args = Args::new_unsized(ctx.clone());
  args.this(weakref)?;
  let val: Value<'js> = args.apply(&deref_fn)?;
  Ok(val.into_object())
}

fn cstring_for_quickjs(s: &str) -> CString {
  // QuickJS error helpers use `printf` style formatting; the message is passed as a `%s` arg, so
  // we only need a valid C string.
  match CString::new(s) {
    Ok(s) => s,
    Err(_) => CString::new(s.replace('\0', "\u{FFFD}")).expect("replacement string has no NULs"),
  }
}

fn throw_type_error<'js>(ctx: &Ctx<'js>, msg: &str) -> rquickjs::Error {
  let fmt = cstring_for_quickjs("%s");
  let msg = cstring_for_quickjs(msg);
  unsafe {
    rquickjs::qjs::JS_ThrowTypeError(ctx.as_raw().as_ptr(), fmt.as_ptr(), msg.as_ptr());
  }
  rquickjs::Error::Exception
}

fn throw_dom_exception<'js>(ctx: &Ctx<'js>, name: &str, message: &str) -> rquickjs::Error {
  let globals = ctx.globals();
  let Ok(thrower) = globals.get::<_, Function<'js>>(DOM_EXCEPTION_GLOBAL) else {
    // If the shim was not installed for some reason, fall back to a TypeError so we still throw.
    return throw_type_error(ctx, message);
  };

  match thrower.call::<_, ()>((name, message)) {
    Ok(_) => throw_type_error(ctx, message),
    Err(e) => e,
  }
}

fn throw_syntax_dom_exception<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
  throw_dom_exception(ctx, "SyntaxError", message)
}

fn dom_error_to_js<'js>(ctx: &Ctx<'js>, err: DomError) -> rquickjs::Error {
  match err {
    DomError::HierarchyRequestError | DomError::NotFoundError | DomError::InvalidNodeType => {
      let name = err.code();
      throw_dom_exception(ctx, name, name)
    }
    DomError::SyntaxError => throw_syntax_dom_exception(ctx, err.code()),
  }
}

fn dom_exception_to_js<'js>(ctx: &Ctx<'js>, err: DomException) -> rquickjs::Error {
  match err {
    DomException::SyntaxError { message } => throw_syntax_dom_exception(ctx, &message),
  }
}

fn register_cache_finalizer<'js>(ctx: &Ctx<'js>, key: &str, obj: &Object<'js>) -> JsResult<()> {
  let globals = ctx.globals();
  let register: Option<Function<'js>> = globals.get(NODE_CACHE_FINALIZER_REGISTER_GLOBAL)?;
  let Some(register) = register else {
    return Ok(());
  };

  // Best-effort: if finalization is not supported or fails, we still have WeakRef caching (identity
  // preservation) but may retain dead keys.
  let _ = register.call::<_, ()>((obj.clone(), key.to_string()));
  Ok(())
}

fn ensure_node_exists<'js>(ctx: &Ctx<'js>, dom: &Document, node_id: NodeId) -> JsResult<()> {
  if node_id.index() >= dom.nodes_len() {
    return Err(dom_error_to_js(ctx, DomError::NotFoundError));
  }
  Ok(())
}

fn direct_child_nodes(dom: &Document, parent: NodeId) -> Result<Vec<NodeId>, DomError> {
  let children = dom.children(parent)?;
  Ok(
    children
      .iter()
      .copied()
      .filter(|&child| child.index() < dom.nodes_len() && dom.node(child).parent == Some(parent))
      .collect(),
  )
}

fn node_type(dom: &Document, node_id: NodeId) -> i32 {
  match &dom.node(node_id).kind {
    NodeKind::Element { .. } | NodeKind::Slot { .. } => 1,
    NodeKind::Text { .. } => 3,
    NodeKind::ProcessingInstruction { .. } => 7,
    NodeKind::Comment { .. } => 8,
    NodeKind::Document { .. } => 9,
    NodeKind::Doctype { .. } => 10,
    NodeKind::ShadowRoot { .. } => 11,
  }
}

fn node_name(dom: &Document, node_id: NodeId) -> String {
  match &dom.node(node_id).kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => {
      if namespace.is_empty() || namespace == HTML_NAMESPACE {
        tag_name.to_ascii_uppercase()
      } else {
        tag_name.clone()
      }
    }
    NodeKind::Slot { .. } => "SLOT".to_string(),
    NodeKind::Text { .. } => "#text".to_string(),
    NodeKind::Comment { .. } => "#comment".to_string(),
    NodeKind::ProcessingInstruction { target, .. } => target.clone(),
    NodeKind::Document { .. } => "#document".to_string(),
    NodeKind::Doctype { name, .. } => name.clone(),
    NodeKind::ShadowRoot { .. } => "#document-fragment".to_string(),
  }
}

fn text_content(dom: &Document, root: NodeId) -> String {
  match &dom.node(root).kind {
    NodeKind::Text { content } => return content.clone(),
    NodeKind::Comment { content } => return content.clone(),
    NodeKind::ProcessingInstruction { data, .. } => return data.clone(),
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

fn split_classes(class_attr: &str) -> impl Iterator<Item = &str> {
  class_attr
    .split(|c: char| c.is_ascii_whitespace())
    .filter(|s| !s.is_empty())
}

fn validate_token(token: &str) -> bool {
  if token.is_empty() || token.chars().any(|c| c.is_ascii_whitespace()) {
    return false;
  }
  true
}

fn validate_token_or_throw<'js>(ctx: &Ctx<'js>, token: &str) -> JsResult<()> {
  if validate_token(token) {
    Ok(())
  } else {
    Err(throw_syntax_dom_exception(ctx, "InvalidToken"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use rquickjs::{Context, Runtime};

  fn make_dom(html: &str) -> Rc<RefCell<Document>> {
    let root = fastrender::dom::parse_html(html).unwrap();
    Rc::new(RefCell::new(Document::from_renderer_dom(&root)))
  }

  #[test]
  fn invalid_node_ids_throw_not_found_domexception_instead_of_panicking() {
    let dom = make_dom(r#"<!doctype html><html><body></body></html>"#);
    let small_len = dom.borrow().nodes_len();

    // `NodeId` values are only meaningful within a single dom2 document. To exercise the bindings'
    // bounds checks, intentionally smuggle a NodeId from a *different* document that has a larger
    // backing node vector.
    let bigger_dom = make_dom(r#"<!doctype html><html><body></body></html>"#);
    let mut node_id = bigger_dom.borrow().root();
    while node_id.index() <= small_len + 4 {
      node_id = bigger_dom.borrow_mut().create_element("div", "");
    }

    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
      // Use the same DOMException surface as `install_dom_bindings`, but without installing the
      // full binding set (this is a unit test for node id validation).
      ctx.eval::<(), _>(DOM_EXCEPTION_SHIM).unwrap();

      let state = Rc::new(DomState { dom });
      let bogus = Node {
        state,
        node_id,
      };
      let inst = rquickjs::Class::instance(ctx.clone(), bogus).unwrap();
      let obj: Object<'_> = inst.into_inner();
      ctx.globals().set("bogus", obj).unwrap();

      let outcome: String = ctx
        .eval(
          r#"(() => {
            try {
              // Any property that consults the underlying dom2 node list should throw a consistent
              // NotFoundError DOMException for invalid ids.
              void bogus.nodeType;
              return "no throw";
            } catch (e) {
              return String(e.name) + "|" + String(e instanceof DOMException);
            }
          })()"#,
        )
        .unwrap();
      assert_eq!(outcome, "NotFoundError|true");
    });
  }

  #[test]
  fn invalid_selectors_throw_syntaxerror_domexception() {
    let dom = make_dom(r#"<!doctype html><html><body><div id="x"></div></body></html>"#);
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
      install_dom_bindings(ctx.clone(), dom).unwrap();

      let outcome: String = ctx
        .eval(
          r#"(() => {
            try {
              document.querySelector("[");
              return "no throw";
            } catch (e) {
              return String(e.name) + "|" + String(e instanceof DOMException) + "|" + String(e instanceof SyntaxError);
            }
          })()"#,
        )
        .unwrap();
      assert_eq!(outcome, "SyntaxError|true|false");
    });
  }
}
