use fastrender::api::{ConsoleMessageLevel, DiagnosticsLevel};
use fastrender::js::{Clock, EventLoop, JsExecutionOptions, RunLimits, RunUntilIdleOutcome, VirtualClock};
use fastrender::{BrowserTab, RenderOptions, Result, VmJsBrowserTabExecutor};
use std::sync::Arc;

fn console_messages(tab: &BrowserTab, level: ConsoleMessageLevel) -> Vec<String> {
  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  diagnostics
    .console_messages
    .into_iter()
    .filter(|m| m.level == level)
    .map(|m| m.message)
    .collect()
}

fn console_logs(tab: &BrowserTab) -> Vec<String> {
  console_messages(tab, ConsoleMessageLevel::Log)
}

struct Harness {
  tab: BrowserTab,
  document_url: String,
  options: RenderOptions,
}

impl Harness {
  fn new(document_url: &str, js_execution_options: JsExecutionOptions) -> Result<Self> {
    let options = RenderOptions::new()
      .with_viewport(32, 32)
      .with_diagnostics_level(DiagnosticsLevel::Basic);

    let clock_for_loop: Arc<dyn Clock> = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::<fastrender::BrowserTabHost>::with_clock(clock_for_loop);

    let tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      "",
      options.clone(),
      VmJsBrowserTabExecutor::default(),
      event_loop,
      js_execution_options,
    )?;

    Ok(Self {
      tab,
      document_url: document_url.to_string(),
      options,
    })
  }

  fn register_html_source(&mut self, html: &str) {
    self
      .tab
      .register_html_source(self.document_url.clone(), html.to_string());
  }

  fn navigate(&mut self) -> Result<()> {
    self.tab.navigate_to_url(&self.document_url, self.options.clone())
  }

  fn run_until_idle(&mut self) -> Result<()> {
    let outcome = self.tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);
    Ok(())
  }

}

#[test]
fn range_offsets_ignore_shadow_root_pseudo_child_in_js() -> Result<()> {
  let js_options = JsExecutionOptions::default();
  let mut h = Harness::new("https://example.invalid/range_shadow_root_offsets.html", js_options)?;

  h.register_html_source(
    r#"<!doctype html><body>
      <div id="host"><span id="light"></span></div>
      <div id="host2">hello<span id="after"></span></div>
      <script>
        const host = document.getElementById("host");
        const light = document.getElementById("light");
        host.attachShadow({ mode: "open" });

        // Boundary point (host, 1) must be *after* the first light DOM child, even though dom2
        // stores ShadowRoot at raw children[0].
        const rHost = document.createRange();
        rHost.setStart(host, 1);
        rHost.setEnd(host, 1);

        const rLight = document.createRange();
        rLight.setStart(light, 0);
        rLight.setEnd(light, 0);
        console.log("cmp:" + rHost.compareBoundaryPoints(Range.START_TO_START, rLight));

        // Offsets are validated against the light DOM child count, not the raw children list.
        const rIndex = document.createRange();
        rIndex.setStart(host, 1);
        rIndex.setEnd(host, 1);
        try {
          rIndex.setStart(host, 2);
          console.log("idxerr:no");
        } catch (e) {
          console.log("idxerr:" + e.name);
        }

        // Live range maintenance must also use light DOM indices when inserting/removing.
        const rLive = document.createRange();
        rLive.setStart(host, 1);
        rLive.setEnd(host, 1);
        const ins = document.createElement("span");
        host.insertBefore(ins, light);
        console.log("afterInsert:" + rLive.startOffset + "," + rLive.endOffset);
        host.removeChild(ins);
        console.log("afterRemove:" + rLive.startOffset + "," + rLive.endOffset);

        // splitText must treat host child offsets as light DOM indices (ShadowRoot excluded).
        const host2 = document.getElementById("host2");
        host2.attachShadow({ mode: "open" });
        const t = host2.firstChild;
        const rSplit = document.createRange();
        rSplit.setStart(host2, 1);
        rSplit.setEnd(host2, 1);
        t.splitText(2);
        console.log("afterSplit:" + rSplit.startOffset + "," + rSplit.endOffset);
      </script>
    </body>"#,
  );

  h.navigate()?;
  h.run_until_idle()?;

  assert_eq!(
    console_logs(&h.tab),
    vec![
      "cmp:1".to_string(),
      "idxerr:IndexSizeError".to_string(),
      "afterInsert:2,2".to_string(),
      "afterRemove:1,1".to_string(),
      "afterSplit:2,2".to_string(),
    ]
  );

  Ok(())
}
