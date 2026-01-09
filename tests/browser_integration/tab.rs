use fastrender::dom2::{Document, NodeId};
use fastrender::js::{Clock, EventLoop, RunLimits, RunUntilIdleOutcome, TaskSource, VirtualClock};
use fastrender::{BrowserTab, BrowserTabHost, BrowserTabJsExecutor, Error, RenderOptions, Result};
use std::sync::Arc;
use std::time::Duration;

fn find_element_by_id(dom: &Document, target: &str) -> Option<NodeId> {
  let mut stack = vec![dom.root()];
  while let Some(id) = stack.pop() {
    if dom.id(id).ok().flatten() == Some(target) {
      return Some(id);
    }
    let node = dom.node(id);
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[derive(Default)]
struct QueuedMutationExecutor;

impl BrowserTabJsExecutor for QueuedMutationExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim();
    if code != "queue-mutation" {
      return Ok(());
    }

    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      let box_id = find_element_by_id(host.dom(), "box")
        .ok_or_else(|| Error::Other("expected #box element".to_string()))?;

      {
        let dom = host.dom_mut();
        dom
          .set_attribute(box_id, "class", "b")
          .map_err(|e| Error::Other(e.to_string()))?;
      }

      event_loop.queue_microtask(move |host, _event_loop| {
        let dom = host.dom_mut();
        dom
          .set_attribute(box_id, "data-microtask", "1")
          .map_err(|e| Error::Other(e.to_string()))?;
        Ok(())
      })?;

      Ok(())
    })?;

    Ok(())
  }
}

#[test]
fn browser_tab_runs_queued_tasks_and_microtasks_and_rerenders() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="a"></div>
        <script>queue-mutation</script>
      </body>
    </html>"#;
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut tab = BrowserTab::from_html(html, options, QueuedMutationExecutor::default())?;
  let frame_a = tab.render_frame()?;
  assert!(tab.render_if_needed()?.is_none());

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let dom = tab.dom();
  let box_id = find_element_by_id(dom, "box").expect("#box id");
  assert_eq!(
    dom
      .class_name(box_id)
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("b")
  );
  assert_eq!(
    dom
      .get_attribute(box_id, "data-microtask")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("1")
  );

  let frame_b = tab
    .render_if_needed()?
    .expect("expected a new frame after task-driven mutation");
  assert_ne!(frame_b.data(), frame_a.data(), "expected pixels to change");
  assert!(tab.render_if_needed()?.is_none());
  Ok(())
}

#[derive(Default)]
struct TimerMutationExecutor;

impl BrowserTabJsExecutor for TimerMutationExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim();
    if code != "queue-timer-mutation" {
      return Ok(());
    }

    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      let box_id = find_element_by_id(host.dom(), "box")
        .ok_or_else(|| Error::Other("expected #box element".to_string()))?;

      host
        .dom_mut()
        .set_attribute(box_id, "data-phase", "task")
        .map_err(|e| Error::Other(e.to_string()))?;

      event_loop.set_timeout(Duration::from_millis(10), move |host, _event_loop| {
        host
          .dom_mut()
          .set_attribute(box_id, "data-phase", "timer")
          .map_err(|e| Error::Other(e.to_string()))?;
        Ok(())
      })?;

      Ok(())
    })?;

    Ok(())
  }
}

#[test]
fn browser_tab_timer_tasks_fire_after_clock_advance_and_rerender() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; background: rgb(0, 0, 0); }
        </style>
      </head>
      <body>
        <div id="box"></div>
        <script>queue-timer-mutation</script>
      </body>
    </html>"#;
  let options = RenderOptions::new().with_viewport(64, 64);

  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock_for_loop);

  let mut tab = BrowserTab::from_html_with_event_loop(html, options, TimerMutationExecutor::default(), event_loop)?;
  tab.render_frame()?;

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    tab.dom()
      .get_attribute(find_element_by_id(tab.dom(), "box").expect("#box id"), "data-phase")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("task")
  );
  tab
    .render_if_needed()?
    .expect("expected render after script task mutation");

  clock.advance(Duration::from_millis(10));
  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    tab.dom()
      .get_attribute(find_element_by_id(tab.dom(), "box").expect("#box id"), "data-phase")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("timer")
  );
  tab
    .render_if_needed()?
    .expect("expected render after timer mutation");
  Ok(())
}
