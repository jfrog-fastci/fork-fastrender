use fastrender::dom2::NodeId;
use fastrender::js::{EventLoop, RunLimits, RunUntilIdleOutcome};
use fastrender::{BrowserTab, BrowserTabHost, BrowserTabJsExecutor, Error, RenderOptions, Result};

use super::support::rgba_at;

const INSERT_DYNAMIC_INLINE: &str = "const s=document.createElement('script'); s.text='document.documentElement.className=\"x\"'; document.head.appendChild(s);";
const INLINE_DYNAMIC_BODY: &str = "document.documentElement.className=\"x\"";

const EXTERNAL_URL: &str = "https://example.invalid/a.js";
const INSERT_DYNAMIC_EXTERNAL: &str =
  "const s=document.createElement('script'); s.src='https://example.invalid/a.js'; document.head.appendChild(s);";
const EXTERNAL_BODY: &str = "document.documentElement.setAttribute(\"data-ext\",\"1\")";

#[derive(Default)]
struct DynamicScriptInsertionExecutor;

impl BrowserTabJsExecutor for DynamicScriptInsertionExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim();
    match code {
      INSERT_DYNAMIC_INLINE => {
        let dom = document.dom_mut();
        let head = dom
          .head()
          .ok_or_else(|| Error::Other("expected document.head".to_string()))?;
        let script = dom.create_element("script", "");
        let text = dom.create_text(INLINE_DYNAMIC_BODY);
        dom
          .append_child(script, text)
          .map_err(|e| Error::Other(e.to_string()))?;
        dom
          .append_child(head, script)
          .map_err(|e| Error::Other(e.to_string()))?;
      }
      INLINE_DYNAMIC_BODY => {
        let dom = document.dom_mut();
        let html = dom
          .document_element()
          .ok_or_else(|| Error::Other("expected documentElement".to_string()))?;
        dom
          .set_attribute(html, "class", "x")
          .map_err(|e| Error::Other(e.to_string()))?;
      }
      INSERT_DYNAMIC_EXTERNAL => {
        let dom = document.dom_mut();
        let head = dom
          .head()
          .ok_or_else(|| Error::Other("expected document.head".to_string()))?;
        let script = dom.create_element("script", "");
        dom
          .set_attribute(script, "src", EXTERNAL_URL)
          .map_err(|e| Error::Other(e.to_string()))?;
        dom
          .append_child(head, script)
          .map_err(|e| Error::Other(e.to_string()))?;
      }
      EXTERNAL_BODY => {
        let dom = document.dom_mut();
        let html = dom
          .document_element()
          .ok_or_else(|| Error::Other("expected documentElement".to_string()))?;
        dom
          .set_attribute(html, "data-ext", "1")
          .map_err(|e| Error::Other(e.to_string()))?;
      }
      _ => {}
    }
    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    // This integration test suite focuses on dynamic insertion semantics for classic scripts; treat
    // module scripts identically for the purposes of the executor stub.
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

#[test]
fn browser_tab_executes_dynamically_inserted_inline_scripts() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #box {{ width: 64px; height: 64px; background: rgb(255, 0, 0); }}
            html.x #box {{ background: rgb(0, 0, 255); }}
          </style>
        </head>
        <body>
          <div id="box"></div>
          <script>{INSERT_DYNAMIC_INLINE}</script>
        </body>
      </html>"#
  );

  let options = RenderOptions::new().with_viewport(64, 64);
  let mut tab = BrowserTab::from_html(&html, options, DynamicScriptInsertionExecutor::default())?;

  let frame_red = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_red, 32, 32), [255, 0, 0, 255]);
  assert!(tab.render_if_needed()?.is_none());

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let html_id = tab
    .dom()
    .document_element()
    .ok_or_else(|| Error::Other("expected documentElement".to_string()))?;
  assert_eq!(
    tab
      .dom()
      .class_name(html_id)
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("x")
  );

  let frame_blue = tab
    .render_if_needed()?
    .expect("expected a new frame after dynamic script mutation");
  assert_eq!(rgba_at(&frame_blue, 32, 32), [0, 0, 255, 255]);
  assert_ne!(frame_blue.data(), frame_red.data(), "expected pixels to change");
  assert!(tab.render_if_needed()?.is_none());
  Ok(())
}

#[test]
fn browser_tab_executes_dynamically_inserted_external_scripts() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = format!(
    r#"<!doctype html>
      <html>
        <head></head>
        <body>
          <script>{INSERT_DYNAMIC_EXTERNAL}</script>
        </body>
      </html>"#
  );

  let options = RenderOptions::new().with_viewport(1, 1);
  let mut tab = BrowserTab::from_html(&html, options, DynamicScriptInsertionExecutor::default())?;
  tab.register_script_source(EXTERNAL_URL, EXTERNAL_BODY);

  tab.render_frame()?;
  assert!(tab.render_if_needed()?.is_none());

  let html_id = tab
    .dom()
    .document_element()
    .ok_or_else(|| Error::Other("expected documentElement".to_string()))?;
  assert_eq!(
    tab
      .dom()
      .get_attribute(html_id, "data-ext")
      .map_err(|e| Error::Other(e.to_string()))?,
    None
  );

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    tab
      .dom()
      .get_attribute(html_id, "data-ext")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("1")
  );
  tab
    .render_if_needed()?
    .expect("expected a new frame after external script mutation");
  Ok(())
}
