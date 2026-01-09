//! QuickJS-backed DOM bindings for FastRender's `dom2`.
//!
//! This crate is intentionally small and focuses on wiring up host-maintained state that is
//! observable from JavaScript, such as `document.currentScript`.

use fastrender::dom::HTML_NAMESPACE;
use fastrender::dom2::{Document as Dom2Document, NodeId, NodeKind};
use fastrender::js::CurrentScriptStateHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
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

pub fn install_dom_bindings<'js>(
  ctx: Ctx<'js>,
  globals: &Object<'js>,
  dom: Rc<Dom2Document>,
  current_script_state: CurrentScriptStateHandle,
) -> JsResult<()> {
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
    if node_id.index() >= dom_for_getter.nodes_len() {
      return Ok(None);
    }

    if !matches!(&dom_for_getter.node(node_id).kind, NodeKind::Element { .. }) {
      return Ok(None);
    }

    let element = Object::new(ctx.clone())?;
    element.set("id", element_id(dom_for_getter.as_ref(), node_id))?;
    Ok(Some(element))
  })?;

  globals.set("__fastrender_get_current_script", getter)?;
  ctx.eval::<(), _>(concat!(
    "Object.defineProperty(document, 'currentScript', {",
    "  get: globalThis.__fastrender_get_current_script,",
    "});"
  ))?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::install_dom_bindings;
  use fastrender::dom2::{Document as Dom2Document, NodeId, NodeKind};
  use fastrender::error::{Error, Result};
  use fastrender::js::{
    CurrentScriptHost, CurrentScriptStateHandle, ScriptBlockExecutor, ScriptOrchestrator, ScriptType,
  };
  use rquickjs::{Context, Runtime};
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

  fn init_ctx(dom: Rc<Dom2Document>, script_state: CurrentScriptStateHandle) -> (Runtime, Context) {
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
    let dom = Rc::new(Dom2Document::from_renderer_dom(&renderer_dom));
    let script_a = find_script_by_id(dom.as_ref(), "a");
    let script_b = find_script_by_id(dom.as_ref(), "b");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = JsObservingExecutor { ctx };

    orchestrator.execute_script_element(
      &mut host,
      dom.as_ref(),
      script_a,
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      dom.as_ref(),
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
    let dom = Rc::new(Dom2Document::from_renderer_dom(&renderer_dom));
    let script_a = find_script_by_id(dom.as_ref(), "a");
    let script_b = find_script_by_id(dom.as_ref(), "b");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = NestedJsExecutor::new(ctx, script_a, script_b);

    orchestrator.execute_script_element(
      &mut host,
      dom.as_ref(),
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
    let dom = Rc::new(Dom2Document::from_renderer_dom(&renderer_dom));

    let shadow_script = find_script_by_id(dom.as_ref(), "shadow");
    let module_script = find_script_by_id(dom.as_ref(), "module");

    let script_state = CurrentScriptStateHandle::default();
    let mut host = Host {
      script_state: script_state.clone(),
    };

    let (_rt, ctx) = init_ctx(Rc::clone(&dom), script_state);
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = JsObservingExecutor { ctx };

    orchestrator.execute_script_element(
      &mut host,
      dom.as_ref(),
      shadow_script,
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      dom.as_ref(),
      module_script,
      ScriptType::Module,
      &mut executor,
    )?;

    assert_eq!(read_obs(&executor.ctx), vec![None, None]);
    Ok(())
  }
}
