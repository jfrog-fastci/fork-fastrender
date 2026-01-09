use fastrender::dom2::{Document, NodeId};
use fastrender::js::{
  CurrentScriptHost, CurrentScriptStateHandle, EventLoop, RunLimits, ScriptBlockExecutor,
  ScriptOrchestrator, ScriptScheduler, ScriptSchedulerAction, ScriptType, TaskSource,
};
use fastrender::{Error, Result};
use rquickjs::{Context, Ctx, Object, Runtime, Value};
use std::collections::HashMap;

fn find_element_by_id(dom: &Document, id: &str) -> NodeId {
  for node_id in dom.subtree_preorder(dom.root()) {
    if dom.get_attribute(node_id, "id").ok().flatten() == Some(id) {
      return node_id;
    }
  }
  panic!("element id={id} not found");
}

fn element_id_attr(dom: &Document, node_id: NodeId) -> Option<&str> {
  dom.get_attribute(node_id, "id").unwrap_or(None)
}

fn init_js_realm(dom: &Document, script_nodes: &[NodeId]) -> Result<(Runtime, Context)> {
  let rt = Runtime::new().map_err(|e| Error::Other(e.to_string()))?;
  let ctx = Context::full(&rt).map_err(|e| Error::Other(e.to_string()))?;

  ctx.with(|ctx| -> Result<()> {
    let globals = ctx.globals();

    // A minimal `document` object with a `currentScript` Web-compat getter.
    let document = Object::new(ctx.clone()).map_err(|e| Error::Other(e.to_string()))?;
    globals
      .set("document", document.clone())
      .map_err(|e| Error::Other(e.to_string()))?;

    // `document.__currentScript` is the backing slot; `document.currentScript` is a getter.
    //
    // We also maintain a JS-side stack so nested script execution can restore the previous
    // `currentScript` without requiring Rust to hold JS `Value` handles across `Context::with`
    // boundaries (rquickjs values are lifetime-tied to the `Ctx` borrow).
    ctx
      .eval::<(), _>("globalThis.document.__currentScript = null;")
      .map_err(|e| Error::Other(e.to_string()))?;
    ctx
      .eval::<(), _>("globalThis.document.__currentScriptStack = [];")
      .map_err(|e| Error::Other(e.to_string()))?;
    ctx
      .eval::<(), _>(
        r#"
        Object.defineProperty(globalThis.document, "currentScript", {
          get() { return this.__currentScript; },
          configurable: true,
        });

        globalThis.document.__pushCurrentScript = function (v) {
          this.__currentScriptStack.push(this.__currentScript);
          this.__currentScript = v;
        };

        globalThis.document.__popCurrentScript = function () {
          if (this.__currentScriptStack.length === 0) {
            throw new Error("currentScript JS stack underflow");
          }
          this.__currentScript = this.__currentScriptStack.pop();
        };
      "#,
      )
      .map_err(|e| Error::Other(e.to_string()))?;

    // A stable mapping from dom2 NodeId → wrapper object so JS can observe identity.
    let by_node_id = Object::new(ctx.clone()).map_err(|e| Error::Other(e.to_string()))?;
    globals
      .set("__scriptByNodeId", by_node_id.clone())
      .map_err(|e| Error::Other(e.to_string()))?;

    for &node_id in script_nodes {
      let wrapper = Object::new(ctx.clone()).map_err(|e| Error::Other(e.to_string()))?;
      wrapper
        .set("nodeId", node_id.index() as i32)
        .map_err(|e| Error::Other(e.to_string()))?;
      if let Some(id) = element_id_attr(dom, node_id) {
        wrapper
          .set("id", id)
          .map_err(|e| Error::Other(e.to_string()))?;
      }
      by_node_id
        .set(node_id.index().to_string(), wrapper)
        .map_err(|e| Error::Other(e.to_string()))?;
    }

    // Convenience log used by tests.
    ctx
      .eval::<(), _>("globalThis.log = [];")
      .map_err(|e| Error::Other(e.to_string()))?;

    Ok(())
  })?;

  Ok((rt, ctx))
}

struct JsHost {
  dom: Document,
  js_rt: Runtime,
  js_ctx: Context,

  script_state: CurrentScriptStateHandle,
  orchestrator: ScriptOrchestrator,

  // Script source text, keyed by the script element NodeId.
  script_segments: HashMap<NodeId, Vec<String>>,
  // Optional nested execution plan used by the nested-currentScript test.
  nested: Option<(NodeId, NodeId)>,
}

impl JsHost {
  fn new(dom: Document, script_nodes: &[NodeId]) -> Result<Self> {
    let (js_rt, js_ctx) = init_js_realm(&dom, script_nodes)?;
    Ok(Self {
      dom,
      js_rt,
      js_ctx,
      script_state: CurrentScriptStateHandle::default(),
      orchestrator: ScriptOrchestrator::new(),
      script_segments: HashMap::new(),
      nested: None,
    })
  }

  fn set_script_source(&mut self, node_id: NodeId, source: &str) {
    self
      .script_segments
      .insert(node_id, vec![source.to_string()]);
  }

  fn set_script_segments(&mut self, node_id: NodeId, segments: Vec<&str>) {
    self.script_segments.insert(
      node_id,
      segments.into_iter().map(|s| s.to_string()).collect(),
    );
  }

  fn set_nested(&mut self, outer: NodeId, inner: NodeId) {
    self.nested = Some((outer, inner));
  }

  fn eval_bool(&self, expr: &str) -> Result<bool> {
    self
      .js_ctx
      .with(|ctx| ctx.eval::<bool, _>(expr).map_err(|e| Error::Other(e.to_string())))
  }

  fn run_script_element(&mut self, node_id: NodeId, script_type: ScriptType) -> Result<()> {
    let mut exec = JsExecutor::default();
    let mut orchestrator = std::mem::take(&mut self.orchestrator);
    let result = orchestrator.execute_script_element(self, node_id, script_type, &mut exec);
    self.orchestrator = orchestrator;
    result
  }
}

impl CurrentScriptHost for JsHost {
  fn current_script_state(&self) -> &CurrentScriptStateHandle {
    &self.script_state
  }
}

impl fastrender::js::DomHost for JsHost {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R,
  {
    f(&self.dom)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool),
  {
    let (result, _changed) = f(&mut self.dom);
    result
  }
}

#[derive(Default)]
struct JsExecutor {
  did_nested: bool,
}

impl JsExecutor {
  fn js_push_current_script<'js>(ctx: Ctx<'js>, new_current_script: Option<NodeId>) -> Result<()> {
    match new_current_script {
      None => ctx
        .eval::<(), _>("document.__pushCurrentScript(null);")
        .map_err(|e| Error::Other(e.to_string()))?,
      Some(node_id) => ctx
        .eval::<(), _>(format!(
          "document.__pushCurrentScript(__scriptByNodeId[\"{}\"]);",
          node_id.index()
        ))
        .map_err(|e| Error::Other(e.to_string()))?,
    }
    Ok(())
  }

  fn js_pop_current_script<'js>(ctx: Ctx<'js>) -> Result<()> {
    ctx
      .eval::<(), _>("document.__popCurrentScript();")
      .map_err(|e| Error::Other(e.to_string()))?;
    Ok(())
  }
}

impl ScriptBlockExecutor<JsHost> for JsExecutor {
  fn execute_script(
    &mut self,
    host: &mut JsHost,
    orchestrator: &mut ScriptOrchestrator,
    script: NodeId,
    _script_type: ScriptType,
  ) -> Result<()> {
    let Some(segments) = host.script_segments.get(&script).cloned() else {
      return Err(Error::Other(format!(
        "missing script source for node_id={}",
        script.index()
      )));
    };

    // Mirror host currentScript state into the JS `document.currentScript` getter.
    let new_current_script = host.current_script();
    let nested_inner = (!self.did_nested)
      .then(|| host.nested.and_then(|(outer, inner)| (outer == script).then_some(inner)))
      .flatten();

    host
      .js_ctx
      .with(|ctx| Self::js_push_current_script(ctx, new_current_script))?;

    // Always restore `document.currentScript` (via a JS-side stack), even when execution throws.
    let exec_result = (|| -> Result<()> {
      for (idx, seg) in segments.iter().enumerate() {
        host
          .js_ctx
          .with(|ctx| ctx.eval::<(), _>(seg.as_str()).map_err(|e| Error::Other(e.to_string())))?;

        // Simulate a nested script execution boundary between the first and second segments.
        if idx == 0 {
          if let Some(inner) = nested_inner {
            self.did_nested = true;
            orchestrator.execute_script_element(host, inner, ScriptType::Classic, self)?;
          }
        }
      }
      Ok(())
    })();

    let restore_result = host
      .js_ctx
      .with(|ctx| Self::js_pop_current_script(ctx));

    restore_result?;
    exec_result
  }
}

fn assert_log_eq_script_sequence(host: &JsHost, expected: &[NodeId]) -> Result<()> {
  host.js_ctx.with(|ctx| -> Result<()> {
    let globals = ctx.globals();
    let log: Vec<Value<'_>> = globals
      .get("log")
      .map_err(|e| Error::Other(e.to_string()))?;
    assert_eq!(
      log.len(),
      expected.len(),
      "log length mismatch: got {} expected {}",
      log.len(),
      expected.len()
    );

    // Compare each log entry to the wrapper object for the expected node id.
    for (idx, &node_id) in expected.iter().enumerate() {
      let expr = format!(
        "log[{idx}] === __scriptByNodeId[\"{}\"]",
        node_id.index()
      );
      let equal = ctx
        .eval::<bool, _>(expr.as_str())
        .map_err(|e| Error::Other(e.to_string()))?;
      assert!(equal, "log[{idx}] did not match node_id={}", node_id.index());
    }
    Ok(())
  })
}

#[test]
fn document_current_script_restores_for_nested_execution() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><script id=a></script><script id=b></script>")?;
  let dom = Document::from_renderer_dom(&renderer_dom);
  let script_a = find_element_by_id(&dom, "a");
  let script_b = find_element_by_id(&dom, "b");

  let mut host = JsHost::new(dom, &[script_a, script_b])?;
  host.set_script_segments(
    script_a,
    vec![
      "log.push(document.currentScript);",
      "log.push(document.currentScript);",
    ],
  );
  host.set_script_source(script_b, "log.push(document.currentScript);");
  host.set_nested(script_a, script_b);

  host.run_script_element(script_a, ScriptType::Classic)?;

  assert_log_eq_script_sequence(&host, &[script_a, script_b, script_a])?;
  assert!(host.eval_bool("document.currentScript === null")?);
  Ok(())
}

#[test]
fn document_current_script_is_set_for_parser_blocking_async_and_defer() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    r#"<!doctype html>
      <script id=a src="a.js"></script>
      <script id=b></script>
      <script id=c src="c.js" async></script>
      <script id=d src="d.js" defer></script>
    "#,
  )?;
  let dom = Document::from_renderer_dom(&renderer_dom);

  let a = find_element_by_id(&dom, "a");
  let b = find_element_by_id(&dom, "b");
  let c = find_element_by_id(&dom, "c");
  let d = find_element_by_id(&dom, "d");

  let mut host = JsHost::new(dom, &[a, b, c, d])?;
  let mut event_loop = EventLoop::<JsHost>::new();
  let mut scheduler = ScriptScheduler::<NodeId>::new();

  let mut blocked_parser_on: Option<fastrender::js::ScriptId> = None;

  let mut apply_actions = |blocked_parser_on: &mut Option<fastrender::js::ScriptId>,
                           host: &mut JsHost,
                           event_loop: &mut EventLoop<JsHost>,
                           actions: Vec<ScriptSchedulerAction<NodeId>>|
   -> Result<()> {
    for action in actions {
      match action {
        ScriptSchedulerAction::StartFetch { .. } => {}
        ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
          *blocked_parser_on = Some(script_id);
        }
        ScriptSchedulerAction::ExecuteNow {
          script_id,
          node_id,
          source_text,
        } => {
          host.set_script_source(node_id, &source_text);
          host.run_script_element(node_id, ScriptType::Classic)?;
          event_loop.perform_microtask_checkpoint(host)?;
          if *blocked_parser_on == Some(script_id) {
            *blocked_parser_on = None;
          }
        }
        ScriptSchedulerAction::QueueTask {
          script_id: _,
          node_id,
          source_text,
        } => {
          host.set_script_source(node_id, &source_text);
          event_loop.queue_task(TaskSource::Script, move |host, _event_loop| {
            host.run_script_element(node_id, ScriptType::Classic)
          })?;
        }
      }
    }
    Ok(())
  };

  // Discover + execute a parser-blocking external script (no async/defer).
  let discovered = scheduler.discovered_parser_script(
    fastrender::js::ScriptElementSpec {
      base_url: None,
      src: Some("https://example.com/a.js".to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      node_id: Some(a),
      script_type: ScriptType::Classic,
    },
    a,
    None,
  )?;
  let a_id = discovered.id;
  apply_actions(&mut blocked_parser_on, &mut host, &mut event_loop, discovered.actions)?;
  assert_eq!(blocked_parser_on, Some(a_id));

  // Fetch completes; the blocking script executes synchronously and unblocks the parser.
  let actions = scheduler.fetch_completed(a_id, "log.push(document.currentScript);".to_string())?;
  apply_actions(&mut blocked_parser_on, &mut host, &mut event_loop, actions)?;
  assert_eq!(blocked_parser_on, None);

  // Now the parser can continue and execute an inline script.
  let discovered = scheduler.discovered_parser_script(
    fastrender::js::ScriptElementSpec {
      base_url: None,
      src: None,
      src_attr_present: false,
      inline_text: "log.push(document.currentScript);".to_string(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      node_id: Some(b),
      script_type: ScriptType::Classic,
    },
    b,
    None,
  )?;
  apply_actions(&mut blocked_parser_on, &mut host, &mut event_loop, discovered.actions)?;

  // Discover async + defer external scripts.
  let discovered = scheduler.discovered_parser_script(
    fastrender::js::ScriptElementSpec {
      base_url: None,
      src: Some("https://example.com/c.js".to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr: true,
      defer_attr: false,
      parser_inserted: true,
      node_id: Some(c),
      script_type: ScriptType::Classic,
    },
    c,
    None,
  )?;
  let c_id = discovered.id;
  apply_actions(&mut blocked_parser_on, &mut host, &mut event_loop, discovered.actions)?;

  let discovered = scheduler.discovered_parser_script(
    fastrender::js::ScriptElementSpec {
      base_url: None,
      src: Some("https://example.com/d.js".to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr: false,
      defer_attr: true,
      parser_inserted: true,
      node_id: Some(d),
      script_type: ScriptType::Classic,
    },
    d,
    None,
  )?;
  let d_id = discovered.id;
  apply_actions(&mut blocked_parser_on, &mut host, &mut event_loop, discovered.actions)?;

  // Parsing completes (allows defer scripts to queue once ready).
  apply_actions(
    &mut blocked_parser_on,
    &mut host,
    &mut event_loop,
    scheduler.parsing_completed()?,
  )?;

  // Complete fetches (queue tasks).
  apply_actions(
    &mut blocked_parser_on,
    &mut host,
    &mut event_loop,
    scheduler.fetch_completed(c_id, "log.push(document.currentScript);".to_string())?,
  )?;
  apply_actions(
    &mut blocked_parser_on,
    &mut host,
    &mut event_loop,
    scheduler.fetch_completed(d_id, "log.push(document.currentScript);".to_string())?,
  )?;

  // Drain event loop tasks (async/defer scripts run here).
  event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

  // Expected order: a (blocking external), b (inline), c (async), d (defer).
  assert_log_eq_script_sequence(&host, &[a, b, c, d])?;
  assert!(host.eval_bool("document.currentScript === null")?);
  Ok(())
}

#[test]
fn document_current_script_is_null_for_shadow_root_scripts() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    r#"<!doctype html>
      <div id=host>
        <template shadowroot="open">
          <script id=shadow></script>
        </template>
      </div>
    "#,
  )?;
  let dom = Document::from_renderer_dom(&renderer_dom);
  let shadow_script = find_element_by_id(&dom, "shadow");

  let mut host = JsHost::new(dom, &[shadow_script])?;
  host.set_script_source(shadow_script, "globalThis.shadowObserved = document.currentScript;");

  host.run_script_element(shadow_script, ScriptType::Classic)?;
  assert!(host.eval_bool("shadowObserved === null")?);
  Ok(())
}

#[test]
fn document_current_script_is_null_for_module_scripts() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html("<!doctype html><script id=mod></script>")?;
  let dom = Document::from_renderer_dom(&renderer_dom);
  let module_script = find_element_by_id(&dom, "mod");

  let mut host = JsHost::new(dom, &[module_script])?;
  host.set_script_source(module_script, "globalThis.modObserved = document.currentScript;");

  host.run_script_element(module_script, ScriptType::Module)?;
  assert!(host.eval_bool("modObserved === null")?);
  assert!(host.eval_bool("document.currentScript === null")?);
  Ok(())
}

#[test]
fn current_script_is_restored_on_js_exception() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><script id=boom></script>")?;
  let dom = Document::from_renderer_dom(&renderer_dom);
  let boom = find_element_by_id(&dom, "boom");

  let mut host = JsHost::new(dom, &[boom])?;
  host.set_script_source(boom, "throw new Error('boom');");

  let err = host
    .run_script_element(boom, ScriptType::Classic)
    .expect_err("expected script execution to throw");
  assert!(matches!(err, Error::Other(_)));
  assert!(host.eval_bool("document.currentScript === null")?);
  Ok(())
}

#[test]
fn disconnected_scripts_do_not_execute_and_do_not_affect_current_script() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><template><script id=inert></script></template><script id=live></script>",
  )?;
  let dom = Document::from_renderer_dom(&renderer_dom);
  let inert = find_element_by_id(&dom, "inert");
  let live = find_element_by_id(&dom, "live");

  let mut host = JsHost::new(dom, &[inert, live])?;
  host.set_script_source(inert, "globalThis.inertRan = true;");
  host.set_script_source(live, "globalThis.liveObserved = document.currentScript;");

  host.run_script_element(inert, ScriptType::Classic)?;
  host.run_script_element(live, ScriptType::Classic)?;

  // The inert script should not have run.
  assert!(host.eval_bool("typeof inertRan === 'undefined'")?);
  // The live script should observe itself as currentScript.
  assert!(host.eval_bool(&format!(
    "liveObserved === __scriptByNodeId[\"{}\"]",
    live.index()
  ))?);
  // And `currentScript` should remain null after execution.
  assert!(host.eval_bool("document.currentScript === null")?);
  Ok(())
}

#[test]
fn disconnected_scripts_do_not_modify_current_script_when_already_set() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><template><script id=inert></script></template><script id=live></script>",
  )?;
  let dom = Document::from_renderer_dom(&renderer_dom);
  let inert = find_element_by_id(&dom, "inert");
  let live = find_element_by_id(&dom, "live");

  let mut host = JsHost::new(dom, &[inert, live])?;

  // Simulate an already-executing script (both host-side and in the JS `document`).
  host.script_state.borrow_mut().current_script = Some(live);
  host.js_ctx.with(|ctx| -> Result<()> {
    ctx
      .eval::<(), _>(format!(
        "document.__currentScript = __scriptByNodeId[\"{}\"]; document.__currentScriptStack = [];",
        live.index()
      ))
      .map_err(|e| Error::Other(e.to_string()))?;
    Ok(())
  })?;

  host.run_script_element(inert, ScriptType::Classic)?;

  // Inert scripts must not execute and must not affect currentScript.
  assert_eq!(host.current_script(), Some(live));
  assert!(host.eval_bool(&format!(
    "document.currentScript === __scriptByNodeId[\"{}\"]",
    live.index()
  ))?);
  assert!(host.eval_bool("document.__currentScriptStack.length === 0")?);
  Ok(())
}
