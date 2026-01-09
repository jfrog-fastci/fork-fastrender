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
const DOM_EXCEPTION_GLOBAL: &str = "__fastrender_throw_dom_exception";

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

  // Create the `document` global.
  let document_obj = state.wrap_node(ctx.clone(), root)?;
  ctx.globals().set("document", document_obj)?;

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
    let weakref_obj = weakref_new(&ctx, obj.clone())?;
    cache.set(key.as_str(), weakref_obj)?;
    Ok(obj)
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

  // ===========================================================================
  // Node mutation
  // ===========================================================================

  #[qjs(rename = "appendChild")]
  fn append_child<'js>(&self, ctx: Ctx<'js>, child: Node) -> JsResult<Object<'js>> {
    self
      .state
      .dom
      .borrow_mut()
      .append_child(self.node_id, child.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
    self.state.wrap_node(ctx, child.node_id)
  }

  #[qjs(rename = "insertBefore")]
  fn insert_before<'js>(
    &self,
    ctx: Ctx<'js>,
    new_child: Node,
    reference_child: Option<Node>,
  ) -> JsResult<Object<'js>> {
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
    Ok(())
  }

  #[qjs(rename = "replaceChild")]
  fn replace_child<'js>(
    &self,
    ctx: Ctx<'js>,
    new_child: Node,
    old_child: Node,
  ) -> JsResult<Object<'js>> {
    self
      .state
      .dom
      .borrow_mut()
      .replace_child(self.node_id, new_child.node_id, old_child.node_id)
      .map_err(|e| dom_error_to_js(&ctx, e))?;
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
    self.ensure_document(ctx.clone())?;
    let result = {
      let mut dom = self.state.dom.borrow_mut();
      dom
        .query_selector(&selectors, None)
        .map_err(|e| dom_exception_to_js(&ctx, e))?
    };
    result.map(|id| self.state.wrap_node(ctx, id)).transpose()
  }

  #[qjs(rename = "querySelectorAll")]
  fn query_selector_all<'js>(&self, ctx: Ctx<'js>, selectors: String) -> JsResult<Vec<Object<'js>>> {
    self.ensure_document(ctx.clone())?;
    let ids = {
      let mut dom = self.state.dom.borrow_mut();
      dom
        .query_selector_all(&selectors, None)
        .map_err(|e| dom_exception_to_js(&ctx, e))?
    };
    ids
      .into_iter()
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

fn throw_syntax_error<'js>(ctx: &Ctx<'js>, msg: &str) -> rquickjs::Error {
  let fmt = cstring_for_quickjs("%s");
  let msg = cstring_for_quickjs(msg);
  unsafe {
    rquickjs::qjs::JS_ThrowSyntaxError(ctx.as_raw().as_ptr(), fmt.as_ptr(), msg.as_ptr());
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

fn dom_error_to_js<'js>(ctx: &Ctx<'js>, err: DomError) -> rquickjs::Error {
  match err {
    DomError::HierarchyRequestError | DomError::NotFoundError | DomError::InvalidNodeType => {
      let name = err.code();
      throw_dom_exception(ctx, name, name)
    }
    DomError::SyntaxError => throw_syntax_error(ctx, err.code()),
  }
}

fn dom_exception_to_js<'js>(ctx: &Ctx<'js>, err: DomException) -> rquickjs::Error {
  match err {
    DomException::SyntaxError { message } => throw_syntax_error(ctx, &message),
  }
}

fn ensure_node_exists<'js>(ctx: &Ctx<'js>, dom: &Document, node_id: NodeId) -> JsResult<()> {
  if node_id.index() >= dom.nodes_len() {
    return Err(dom_error_to_js(ctx, DomError::NotFoundError));
  }
  Ok(())
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
    Err(throw_syntax_error(ctx, "InvalidToken"))
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
}
