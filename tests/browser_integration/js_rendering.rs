use fastrender::dom2::Document;
use fastrender::error::{Error, Result};
use fastrender::html::base_url_tracker::BaseUrlTracker;
use fastrender::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use fastrender::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2;
use fastrender::js::{
  ClassicScriptScheduler, EventLoop, RunLimits, RunUntilIdleOutcome, ScriptExecutor, ScriptLoader,
  VirtualClock,
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

fn read_script_source(url: &str) -> Result<String> {
  let parsed =
    Url::parse(url).map_err(|err| Error::Other(format!("invalid script URL {url:?}: {err}")))?;
  if parsed.scheme() != "file" {
    return Err(Error::Other(format!(
      "unexpected non-file script URL (scheme={}): {url:?}",
      parsed.scheme()
    )));
  }
  let path = parsed
    .to_file_path()
    .map_err(|()| Error::Other(format!("failed to convert file:// URL to path: {url:?}")))?;
  std::fs::read_to_string(&path)
    .map_err(|err| Error::Other(format!("failed to read script source {url:?}: {err}")))
}

#[derive(Clone)]
enum DocumentAccess {
  Parsing(Rc<RefCell<StreamingHtmlParser>>),
  Final(Rc<RefCell<Document>>),
}

struct FixtureHost {
  dom: Rc<RefCell<DocumentAccess>>,
  js_rt: JsRuntime,
  js_ctx: JsContext,
  next_handle: usize,
  handles_by_url: HashMap<String, usize>,
  completed: VecDeque<(usize, String)>,
}

impl FixtureHost {
  fn new(dom_access: DocumentAccess) -> Result<Self> {
    let dom = Rc::new(RefCell::new(dom_access));
    let js_rt = JsRuntime::new().map_err(|err| Error::Other(err.to_string()))?;
    let js_ctx = JsContext::full(&js_rt).map_err(|err| Error::Other(err.to_string()))?;

    js_ctx
      .with(|ctx| -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let dom_for_fn = Rc::clone(&dom);
        let set_class = JsFunction::new(ctx.clone(), move |id: String, class_name: String| {
          let target = dom_for_fn.borrow().clone();
          match target {
            DocumentAccess::Parsing(parser) => {
              let parser = parser.borrow();
              let mut doc = parser.document_mut();
              let Some(node) = doc.get_element_by_id(&id) else {
                return;
              };
              let _ = doc.set_attribute(node, "class", &class_name);
            }
            DocumentAccess::Final(doc) => {
              let mut doc = doc.borrow_mut();
              let Some(node) = doc.get_element_by_id(&id) else {
                return;
              };
              let _ = doc.set_attribute(node, "class", &class_name);
            }
          }
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

  fn switch_to_final_document(&mut self, document: Document) {
    let rc = Rc::new(RefCell::new(document));
    *self.dom.borrow_mut() = DocumentAccess::Final(Rc::clone(&rc));
  }

  fn dom_snapshot(&self) -> fastrender::dom::DomNode {
    match &*self.dom.borrow() {
      DocumentAccess::Parsing(parser) => {
        let parser = parser.borrow();
        let doc = parser.document();
        doc.to_renderer_dom()
      }
      DocumentAccess::Final(doc) => doc.borrow().to_renderer_dom(),
    }
  }

  fn root_class(&self) -> Option<String> {
    fn read(doc: &Document) -> Option<String> {
      let root = doc.get_element_by_id("root")?;
      doc
        .get_attribute(root, "class")
        .ok()
        .flatten()
        .map(|s| s.to_string())
    }

    match &*self.dom.borrow() {
      DocumentAccess::Parsing(parser) => {
        let parser = parser.borrow();
        let doc = parser.document();
        read(&doc)
      }
      DocumentAccess::Final(doc) => read(&doc.borrow()),
    }
  }

  fn complete_url(&mut self, url: &str) -> Result<()> {
    let Some(handle) = self.handles_by_url.get(url).copied() else {
      return Err(Error::Other(format!(
        "attempted to complete unknown script load url={url}"
      )));
    };
    let source = read_script_source(url)?;
    self.completed.push_back((handle, source));
    Ok(())
  }
}

impl ScriptLoader for FixtureHost {
  type Handle = usize;

  fn load_blocking(&mut self, url: &str) -> Result<String> {
    read_script_source(url)
  }

  fn start_load(&mut self, url: &str) -> Result<Self::Handle> {
    if self.handles_by_url.contains_key(url) {
      return Err(Error::Other(format!(
        "duplicate start_load call for script URL {url:?}"
      )));
    }
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
    _spec: &fastrender::js::ScriptElementSpec,
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

struct JsFixtureHarness {
  document_url: String,
  parser: Rc<RefCell<StreamingHtmlParser>>,
  host: FixtureHost,
  scheduler: ClassicScriptScheduler<FixtureHost>,
  event_loop: EventLoop<FixtureHost>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpOutcome {
  NeedMoreInput,
  Finished,
}

impl JsFixtureHarness {
  fn from_fixture(name: &str) -> Result<Self> {
    let fixture_url = file_url_for_path(&fixture_path(name))?;
    let parser = Rc::new(RefCell::new(StreamingHtmlParser::new(Some(&fixture_url))));

    let host = FixtureHost::new(DocumentAccess::Parsing(Rc::clone(&parser)))?;
    let scheduler = ClassicScriptScheduler::<FixtureHost>::new();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn fastrender::js::Clock> = clock;
    let event_loop = EventLoop::<FixtureHost>::with_clock(clock_for_loop);

    Ok(Self {
      document_url: fixture_url,
      parser,
      host,
      scheduler,
      event_loop,
    })
  }

  fn push_str(&mut self, chunk: &str) {
    self.parser.borrow_mut().push_str(chunk);
  }

  fn set_eof(&mut self) {
    self.parser.borrow_mut().set_eof();
  }

  fn pump_until_stalled(&mut self) -> Result<PumpOutcome> {
    loop {
      let yield_ = { self.parser.borrow_mut().pump() };
      match yield_ {
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        } => {
          let spec = {
            let parser = self.parser.borrow();
            let doc = parser.document();
            let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
            build_parser_inserted_script_element_spec_dom2(&doc, script, &base)
          };
          self
            .scheduler
            .handle_script(&mut self.host, &mut self.event_loop, spec)?;
          continue;
        }
        StreamingParserYield::NeedMoreInput => return Ok(PumpOutcome::NeedMoreInput),
        StreamingParserYield::Finished { document } => {
          self.host.switch_to_final_document(document);
          return Ok(PumpOutcome::Finished);
        }
      }
    }
  }

  fn pump_to_completion(&mut self) -> Result<()> {
    loop {
      match self.pump_until_stalled()? {
        PumpOutcome::NeedMoreInput => {
          return Err(Error::Other(
            "unexpected NeedMoreInput while pumping with EOF set".to_string(),
          ));
        }
        PumpOutcome::Finished => return Ok(()),
      }
    }
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
  let html = read_fixture("inline_mutation.html")?;
  harness.push_str(&html);
  harness.set_eof();
  harness.pump_to_completion()?;

  harness
    .scheduler
    .finish_parsing(&mut harness.host, &mut harness.event_loop)?;
  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(harness.host.root_class().as_deref(), Some("on"));

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
  let html = read_fixture("external_defer.html")?;
  harness.push_str(&html);
  harness.set_eof();
  harness.pump_to_completion()?;

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

  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    harness.host.root_class().as_deref(),
    Some("off"),
    "defer scripts must not execute before parsing is marked finished"
  );

  harness
    .scheduler
    .finish_parsing(&mut harness.host, &mut harness.event_loop)?;
  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(harness.host.root_class().as_deref(), Some("step2"));

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
  let html = read_fixture("external_async.html")?;
  let marker = "<div style=\"display:none\">padding</div>";
  let (first, second) = html
    .split_once(marker)
    .ok_or_else(|| Error::Other("async fixture missing chunk marker".to_string()))?;

  harness.push_str(first);
  match harness.pump_until_stalled()? {
    PumpOutcome::NeedMoreInput => {}
    PumpOutcome::Finished => {
      return Err(Error::Other(
        "async fixture unexpectedly finished parsing before async load completed".to_string(),
      ));
    }
  }

  let doc_url = Url::parse(&harness.document_url).expect("fixture URL should parse");
  let async_url = doc_url
    .join("assets/async.js")
    .expect("resolve async")
    .to_string();

  harness.host.complete_url(&async_url)?;
  harness
    .scheduler
    .poll(&mut harness.host, &mut harness.event_loop)?;

  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    harness.host.root_class().as_deref(),
    Some("on"),
    "async scripts should be able to mutate the document before parsing completes"
  );

  harness.push_str(marker);
  harness.push_str(second);
  harness.set_eof();
  harness.pump_to_completion()?;

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
  let html = read_fixture("base_url_timing.html")?;
  harness.push_str(&html);
  harness.set_eof();
  harness.pump_to_completion()?;
  harness
    .scheduler
    .finish_parsing(&mut harness.host, &mut harness.event_loop)?;

  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("base_url_timing_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "script before <base href> should resolve against document URL and affect pixels"
  );
  Ok(())
}
