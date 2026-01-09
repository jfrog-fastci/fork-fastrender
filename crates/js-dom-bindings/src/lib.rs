//! QuickJS-backed DOM bindings for FastRender's `dom2`.
//!
//! This crate is intentionally small and focuses on wiring up host-maintained state that is
//! observable from JavaScript, such as `document.currentScript`.

use fastrender::dom::HTML_NAMESPACE;
use fastrender::dom2::{Document as Dom2Document, DomError, NodeId, NodeKind};
use fastrender::js::DomHost;
use fastrender::web::dom::DomException;
use fastrender::js::CurrentScriptStateHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult, Value};
use std::cell::RefCell;
use std::rc::Rc;

fn element_id(dom: &Dom2Document, node_id: NodeId) -> String {
  let node = dom.node(node_id);
  match &node.kind {
    NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => {
      let is_html = namespace.is_empty() || namespace == HTML_NAMESPACE;
      for (name, value) in attributes {
        if (is_html && name.eq_ignore_ascii_case("id")) || (!is_html && name == "id") {
          return value.clone();
        }
      }
      String::new()
    }
    _ => String::new(),
  }
}

fn element_tag_name(dom: &Dom2Document, node_id: NodeId) -> String {
  match &dom.node(node_id).kind {
    NodeKind::Element { tag_name, .. } => tag_name.to_ascii_uppercase(),
    NodeKind::Slot { .. } => "SLOT".to_string(),
    _ => String::new(),
  }
}

fn make_element_object<'js>(
  ctx: Ctx<'js>,
  dom: &Dom2Document,
  node_id: NodeId,
) -> JsResult<Object<'js>> {
  let element = Object::new(ctx.clone())?;
  element.set("id", element_id(dom, node_id))?;
  element.set("tagName", element_tag_name(dom, node_id))?;
  element.set("__node_id", node_id.index() as u32)?;

  // Minimal DOM mutation hooks frequently used by bootstrap scripts:
  // `document.head.appendChild(...)`, `document.body.appendChild(...)`, etc.
  //
  // This is a very small shim, not a full DOM: we mirror mutations into the Rust `dom2` tree via
  // host functions, and maintain a JS-side `childNodes` array for compatibility with common code.
  let child_nodes: Value<'js> = ctx.eval("[]")?;
  element.set("childNodes", child_nodes)?;
  let append_child: Function<'js> = ctx.eval(
    r#"(function (child) {
      if (!this.childNodes) this.childNodes = [];
      if (!child || (typeof child !== "object" && typeof child !== "function") || child.__node_id == null) {
        throw new DOMException("InvalidNodeType", "InvalidNodeType");
      }
      if (typeof globalThis.__fastrender_dom_append_child === "function" && this.__node_id != null) {
        globalThis.__fastrender_dom_append_child(this.__node_id, child.__node_id);
      }
      this.childNodes.push(child);
      child.parentNode = this;
      return child;
    })"#,
  )?;
  element.set("appendChild", append_child)?;
  let remove_child: Function<'js> = ctx.eval(
    r#"(function (child) {
      if (!child || (typeof child !== "object" && typeof child !== "function") || child.__node_id == null) {
        throw new DOMException("InvalidNodeType", "InvalidNodeType");
      }
      if (typeof globalThis.__fastrender_dom_remove_child === "function" && this.__node_id != null) {
        globalThis.__fastrender_dom_remove_child(this.__node_id, child.__node_id);
      }
      if (this.childNodes && this.childNodes.length) {
        var idx = this.childNodes.indexOf(child);
        if (idx >= 0) this.childNodes.splice(idx, 1);
      }
      if (child && (typeof child === "object" || typeof child === "function")) {
        if (child.parentNode === this) child.parentNode = null;
      }
      return child;
    })"#,
  )?;
  element.set("removeChild", remove_child)?;

  Ok(element)
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

    let maybe_id = dom_for_getter.borrow().with_dom(|dom| {
      // The orchestrator stores `NodeId` handles. If the node is gone (future DOM delete support),
      // surface `null` rather than crashing.
      if node_id.index() >= dom.nodes_len() {
        return None;
      }
      if !matches!(&dom.node(node_id).kind, NodeKind::Element { .. }) {
        return None;
      }
      Some(element_id(dom, node_id))
    });

    let Some(id) = maybe_id else {
      return Ok(None);
    };

    let element = Object::new(ctx.clone())?;
    element.set("id", id)?;
    Ok(Some(element))
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
  let (doc_el, head, body) = dom
    .borrow()
    .with_dom(|dom| -> JsResult<(Option<Object<'js>>, Option<Object<'js>>, Option<Object<'js>>)> {
    let doc_el = dom
      .document_element()
      .map(|node_id| make_element_object(ctx.clone(), dom, node_id))
      .transpose()?;
    let head = dom
      .head()
      .map(|node_id| make_element_object(ctx.clone(), dom, node_id))
      .transpose()?;
    let body = dom
      .body()
      .map(|node_id| make_element_object(ctx.clone(), dom, node_id))
      .transpose()?;
    Ok((doc_el, head, body))
  })?;
  document.set("documentElement", doc_el)?;
  document.set("head", head)?;
  document.set("body", body)?;

  // Ensure these structural nodes are discoverable via `document.querySelector`, which is backed by
  // the `__fastrender_node_by_id` mapping in `DOM_BINDINGS_SHIM`.
  ctx.eval::<(), _>(
    r#"(function () {
      var g = typeof globalThis !== "undefined" ? globalThis : this;
      var map = g.__fastrender_node_by_id;
      if (!map || typeof map.set !== "function") return;
      var doc = g.document;
      if (!doc) return;
      var nodes = [doc.documentElement, doc.head, doc.body];
      for (var i = 0; i < nodes.length; i++) {
        var n = nodes[i];
        if (n && n.__node_id != null) {
          map.set(n.__node_id, n);
        }
      }
    })();"#,
  )?;

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
    "__fastrender_dom_query_selector",
    Function::new(ctx.clone(), {
      let dom = Rc::clone(&dom);
      move |ctx: Ctx<'js>, selectors: String| {
        let mut dom = dom.borrow_mut();
        let result = dom.mutate_dom(|dom| {
          match dom.query_selector(&selectors, None) {
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

const DOM_BINDINGS_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;

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

  // Node id -> JS object mapping so selectors can preserve identity.
  var nodeById = g.__fastrender_node_by_id;
  if (!nodeById) {
    nodeById = new Map();
    g.__fastrender_node_by_id = nodeById;
  }

  function registerNode(obj, id) {
    if (!obj) return obj;
    try {
      Object.defineProperty(obj, "__node_id", {
        value: id,
        writable: true,
        configurable: true,
      });
    } catch (_e) {
      obj.__node_id = id;
    }
    nodeById.set(id, obj);
    return obj;
  }

  function ensureNodeApis(obj) {
    if (!obj) return;
    if (typeof obj.appendChild !== "function") {
      obj.appendChild = function (child) {
        if (!child || (typeof child !== "object" && typeof child !== "function") || child.__node_id == null) {
          throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
        }
        g.__fastrender_dom_append_child(this.__node_id, child.__node_id);
        if (child && (typeof child === "object" || typeof child === "function")) {
          child.parentNode = this;
        }
        return child;
      };
    }
    if (typeof obj.removeChild !== "function") {
      obj.removeChild = function (child) {
        if (!child || (typeof child !== "object" && typeof child !== "function") || child.__node_id == null) {
          throw new g.DOMException("InvalidNodeType", "InvalidNodeType");
        }
        g.__fastrender_dom_remove_child(this.__node_id, child.__node_id);
        if (child && (typeof child === "object" || typeof child === "function")) {
          if (child.parentNode === this) child.parentNode = null;
        }
        return child;
      };
    }
  }

  // Register the document root as node id 0 (dom2::Document::root()).
  if (doc.__node_id == null) {
    registerNode(doc, 0);
  }
  ensureNodeApis(doc);

  doc.createElement = function (tagName) {
    var el = { tagName: String(tagName), parentNode: null };
    var id = g.__fastrender_dom_create_element(String(tagName));
    registerNode(el, id);
    ensureNodeApis(el);
    return el;
  };

  doc.createTextNode = function (data) {
    var text = { data: String(data), parentNode: null };
    var id = g.__fastrender_dom_create_text_node(String(data));
    registerNode(text, id);
    ensureNodeApis(text);
    return text;
  };

  doc.querySelector = function (selectors) {
    var id = g.__fastrender_dom_query_selector(String(selectors));
    if (id == null) return null;
    return nodeById.get(id) || null;
  };
})();
"#;

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

  #[derive(Default)]
  struct Host {
    script_state: CurrentScriptStateHandle,
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
          && super::element_id(dom, node_id).as_str() == id
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
      _dom: &Dom2Document,
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
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_a = find_script_by_id(&dom.borrow().dom, "a");
    let script_b = find_script_by_id(&dom.borrow().dom, "b");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = JsObservingExecutor { ctx };

    orchestrator.execute_script_element(
      &mut host,
      &dom.borrow().dom,
      script_a,
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      &dom.borrow().dom,
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
      dom: &Dom2Document,
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
        orchestrator.execute_script_element(host, dom, self.script_b, ScriptType::Classic, self)?;
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
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));
    let script_a = find_script_by_id(&dom.borrow().dom, "a");
    let script_b = find_script_by_id(&dom.borrow().dom, "b");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = NestedJsExecutor::new(ctx, script_a, script_b);

    orchestrator.execute_script_element(
      &mut host,
      &dom.borrow().dom,
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
    let dom = Rc::new(RefCell::new(TestDomHost {
      dom: Dom2Document::from_renderer_dom(&renderer_dom),
    }));

    let shadow_script = find_script_by_id(&dom.borrow().dom, "shadow");
    let module_script = find_script_by_id(&dom.borrow().dom, "module");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = JsObservingExecutor { ctx };

    orchestrator.execute_script_element(
      &mut host,
      &dom.borrow().dom,
      shadow_script,
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      &dom.borrow().dom,
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
}
