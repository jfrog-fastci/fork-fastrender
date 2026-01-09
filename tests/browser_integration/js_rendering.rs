use fastrender::dom::{parse_html_with_options, DomNode, DomNodeType, DomParseOptions, HTML_NAMESPACE};
use fastrender::dom2::Document;
use fastrender::error::{Error, Result};
use fastrender::html::base_url_tracker::BaseUrlTracker;
use fastrender::js::{
  determine_script_type, ClassicScriptScheduler, EventLoop, RunLimits, RunUntilIdleOutcome,
  ScriptElementSpec, ScriptExecutor, ScriptLoader, VirtualClock,
};
use fastrender::text::font_db::FontConfig;
use fastrender::{FastRender, RenderOptions, ResourcePolicy};
use rquickjs::{Context as JsContext, Function as JsFunction, Runtime as JsRuntime};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

fn offline_renderer() -> Result<FastRender> {
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .build()
}

fn fixtures_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/html/js")
}

fn fixture_path(name: &str) -> PathBuf {
  fixtures_dir().join(name)
}

fn read_fixture(name: &str) -> Result<String> {
  std::fs::read_to_string(fixture_path(name))
    .map_err(|err| Error::Other(format!("failed to read fixture {name}: {err}")))
}

fn file_url_for_path(path: &Path) -> Result<String> {
  Url::from_file_path(path)
    .map(|url| url.to_string())
    .map_err(|()| Error::Other(format!("failed to convert path to file:// URL: {path:?}")))
}

fn read_script_source(url: &str) -> String {
  let Ok(parsed) = Url::parse(url) else {
    return String::new();
  };
  if parsed.scheme() != "file" {
    return String::new();
  }
  let Ok(path) = parsed.to_file_path() else {
    return String::new();
  };
  std::fs::read_to_string(path).unwrap_or_default()
}

struct FixtureHost {
  dom: Rc<RefCell<Document>>,
  js_rt: JsRuntime,
  js_ctx: JsContext,
  next_handle: usize,
  handles_by_url: HashMap<String, usize>,
  completed: VecDeque<(usize, String)>,
}

impl FixtureHost {
  fn new(dom: Document) -> Result<Self> {
    let dom = Rc::new(RefCell::new(dom));

    let js_rt = JsRuntime::new().map_err(|err| Error::Other(err.to_string()))?;
    let js_ctx = JsContext::full(&js_rt).map_err(|err| Error::Other(err.to_string()))?;

    js_ctx
      .with(|ctx| -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let dom_for_fn = Rc::clone(&dom);
        let set_class = JsFunction::new(ctx.clone(), move |id: String, class_name: String| {
          let selector = format!("#{id}");
          let mut doc = dom_for_fn.borrow_mut();
          let Ok(Some(node)) = doc.query_selector(&selector, None) else {
            return;
          };
          let _ = doc.set_attribute(node, "class", &class_name);
        })?;
        globals.set("setClass", set_class)?;
        Ok(())
      })
      .map_err(|err| Error::Other(err.to_string()))?;

    Ok(Self {
      dom,
      js_rt,
      js_ctx,
      next_handle: 0,
      handles_by_url: HashMap::new(),
      completed: VecDeque::new(),
    })
  }

  fn dom_snapshot(&self) -> DomNode {
    self
      .dom
      .borrow()
      .to_renderer_dom()
  }

  fn complete_url(&mut self, url: &str) -> Result<()> {
    let Some(handle) = self.handles_by_url.get(url).copied() else {
      return Err(Error::Other(format!(
        "attempted to complete unknown script load url={url}"
      )));
    };
    let source = read_script_source(url);
    self.completed.push_back((handle, source));
    Ok(())
  }
}

impl ScriptLoader for FixtureHost {
  type Handle = usize;

  fn load_blocking(&mut self, url: &str) -> Result<String> {
    Ok(read_script_source(url))
  }

  fn start_load(&mut self, url: &str) -> Result<Self::Handle> {
    let handle = self.next_handle;
    self.next_handle += 1;
    self.handles_by_url.insert(url.to_string(), handle);
    Ok(handle)
  }

  fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
    Ok(self.completed.pop_front())
  }
}

impl ScriptExecutor for FixtureHost {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    _event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let _ = &self.js_rt;
    self
      .js_ctx
      .with(|ctx| ctx.eval::<(), _>(script_text))
      .map_err(|err| Error::Other(err.to_string()))?;
    Ok(())
  }
}

#[derive(Debug, Clone, Copy)]
struct WalkFlags {
  in_head: bool,
  in_foreign_namespace: bool,
  in_template: bool,
}

impl Default for WalkFlags {
  fn default() -> Self {
    Self {
      in_head: false,
      in_foreign_namespace: false,
      in_template: false,
    }
  }
}

struct JsFixtureHarness {
  traversal_dom: DomNode,
  document_url: String,
  base_tracker: BaseUrlTracker,
  host: FixtureHost,
  scheduler: ClassicScriptScheduler<FixtureHost>,
  event_loop: EventLoop<FixtureHost>,
}

impl JsFixtureHarness {
  fn from_fixture(name: &str) -> Result<Self> {
    let html = read_fixture(name)?;
    let traversal_dom = parse_html_with_options(&html, DomParseOptions::javascript_enabled())?;
    let dom2 = Document::from_renderer_dom(&traversal_dom);

    let fixture_url = file_url_for_path(&fixture_path(name))?;
    let base_tracker = BaseUrlTracker::new(Some(&fixture_url));

    let host = FixtureHost::new(dom2)?;
    let scheduler = ClassicScriptScheduler::<FixtureHost>::new();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn fastrender::js::Clock> = clock;
    let event_loop = EventLoop::<FixtureHost>::with_clock(clock_for_loop);

    Ok(Self {
      traversal_dom,
      document_url: fixture_url,
      base_tracker,
      host,
      scheduler,
      event_loop,
    })
  }

  fn discover_scripts_and_base(&mut self) -> Result<()> {
    let mut stack: Vec<(&DomNode, WalkFlags)> = vec![(&self.traversal_dom, WalkFlags::default())];

    while let Some((node, flags)) = stack.pop() {
      let (tag_name, namespace, attrs) = match &node.node_type {
        DomNodeType::Element {
          tag_name,
          namespace,
          attributes,
        } => (Some(tag_name.as_str()), namespace.as_str(), Some(attributes.as_slice())),
        DomNodeType::Slot {
          namespace,
          attributes,
          ..
        } => (Some("slot"), namespace.as_str(), Some(attributes.as_slice())),
        _ => (None, "", None),
      };

      if let (Some(tag_name), Some(attrs)) = (tag_name, attrs) {
        let is_html_namespace = namespace.is_empty() || namespace == HTML_NAMESPACE;
        let in_foreign_namespace = flags.in_foreign_namespace || !is_html_namespace;

        if tag_name.eq_ignore_ascii_case("base") {
          self.base_tracker.on_element_inserted(
            tag_name,
            namespace,
            attrs,
            flags.in_head,
            in_foreign_namespace,
            flags.in_template,
          );
        }

        if tag_name.eq_ignore_ascii_case("script") {
          let async_attr = attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("async"));
          let defer_attr = attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("defer"));
          let raw_src = attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("src"))
            .map(|(_, v)| v.as_str());
          let src = raw_src.and_then(|raw| self.base_tracker.resolve_script_src(raw));

          let mut inline_text = String::new();
          for child in &node.children {
            if let DomNodeType::Text { content } = &child.node_type {
              inline_text.push_str(content);
            }
          }

          let spec = ScriptElementSpec {
            base_url: self.base_tracker.current_base_url(),
            src,
            inline_text,
            async_attr,
            defer_attr,
            parser_inserted: true,
            script_type: determine_script_type(node),
          };

          self
            .scheduler
            .handle_script(&mut self.host, &mut self.event_loop, spec)?;
        }
      }

      // Compute traversal flags for descendants.
      let mut child_flags = flags;
      if let Some(tag_name) = tag_name {
        if tag_name.eq_ignore_ascii_case("head") {
          child_flags.in_head = true;
        }
        if tag_name.eq_ignore_ascii_case("template") {
          child_flags.in_template = true;
        }
      }

      if let Some(namespace) = node.namespace() {
        if !(namespace.is_empty() || namespace == HTML_NAMESPACE) {
          child_flags.in_foreign_namespace = true;
        }
      }

      for child in node.children.iter().rev() {
        stack.push((child, child_flags));
      }
    }

    Ok(())
  }

  fn run_event_loop_until_idle(&mut self) -> Result<RunUntilIdleOutcome> {
    self.event_loop.run_until_idle(
      &mut self.host,
      RunLimits {
        max_tasks: 128,
        max_microtasks: 1024,
        max_wall_time: None,
      },
    )
  }

  fn render(&self, options: RenderOptions) -> Result<tiny_skia::Pixmap> {
    let mut renderer = offline_renderer()?;
    let dom = self.host.dom_snapshot();
    renderer.render_dom_with_options(&dom, options)
  }
}

fn render_static_fixture(name: &str, options: RenderOptions) -> Result<tiny_skia::Pixmap> {
  let html = read_fixture(name)?;
  let mut renderer = offline_renderer()?;
  renderer.render_html_with_options(&html, options)
}

#[test]
fn js_inline_script_mutation_affects_render() -> Result<()> {
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut harness = JsFixtureHarness::from_fixture("inline_mutation.html")?;
  harness.discover_scripts_and_base()?;
  harness
    .scheduler
    .finish_parsing(&mut harness.host, &mut harness.event_loop)?;

  assert_eq!(harness.run_event_loop_until_idle()?, RunUntilIdleOutcome::Idle);

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("inline_mutation_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "inline script should mutate DOM and affect final pixels"
  );
  Ok(())
}

#[test]
fn js_external_defer_scripts_execute_in_order_after_parsing() -> Result<()> {
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut harness = JsFixtureHarness::from_fixture("external_defer.html")?;
  harness.discover_scripts_and_base()?;

  let doc_url = Url::parse(&harness.document_url).expect("fixture URL should parse");
  let defer1_url = doc_url
    .join("assets/defer1.js")
    .expect("resolve defer1")
    .to_string();
  let defer2_url = doc_url
    .join("assets/defer2.js")
    .expect("resolve defer2")
    .to_string();

  // Complete out of order to ensure `defer` is still executed in document order.
  harness.host.complete_url(&defer2_url)?;
  harness.host.complete_url(&defer1_url)?;
  harness
    .scheduler
    .poll(&mut harness.host, &mut harness.event_loop)?;

  harness
    .scheduler
    .finish_parsing(&mut harness.host, &mut harness.event_loop)?;
  assert_eq!(harness.run_event_loop_until_idle()?, RunUntilIdleOutcome::Idle);

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("external_defer_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "defer scripts should run after parsing and in document order"
  );
  Ok(())
}

#[test]
fn js_external_async_script_runs_without_waiting_for_parsing_complete() -> Result<()> {
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut harness = JsFixtureHarness::from_fixture("external_async.html")?;
  harness.discover_scripts_and_base()?;

  let doc_url = Url::parse(&harness.document_url).expect("fixture URL should parse");
  let async_url = doc_url
    .join("assets/async.js")
    .expect("resolve async")
    .to_string();

  harness.host.complete_url(&async_url)?;
  harness
    .scheduler
    .poll(&mut harness.host, &mut harness.event_loop)?;

  // The critical assertion: we render after driving the event loop, without calling `finish_parsing`
  // yet. If `async` execution were incorrectly gated on parsing completion, this would not match.
  assert_eq!(harness.run_event_loop_until_idle()?, RunUntilIdleOutcome::Idle);

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("external_async_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "async script should mutate DOM even before parsing_completed"
  );

  // Finish parsing (no-op for this fixture, but keeps the scheduler contract explicit).
  harness
    .scheduler
    .finish_parsing(&mut harness.host, &mut harness.event_loop)?;
  Ok(())
}

#[test]
fn js_base_url_timing_script_before_base_href_uses_document_url() -> Result<()> {
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut harness = JsFixtureHarness::from_fixture("base_url_timing.html")?;
  harness.discover_scripts_and_base()?;
  harness
    .scheduler
    .finish_parsing(&mut harness.host, &mut harness.event_loop)?;

  assert_eq!(harness.run_event_loop_until_idle()?, RunUntilIdleOutcome::Idle);

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("base_url_timing_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "script before <base href> should resolve against document URL and affect pixels"
  );
  Ok(())
}
