use fastrender::api::VmJsBrowserTabExecutor;
use fastrender::js::{Clock, EventLoop, RunLimits, RunUntilIdleOutcome, VirtualClock};
use fastrender::{BrowserTab, Error, RenderOptions, Result};
use std::sync::Arc;
use std::time::Duration;

fn attr(
  doc: &fastrender::dom2::Document,
  node: fastrender::dom2::NodeId,
  name: &str,
) -> Result<Option<String>> {
  doc
    .get_attribute(node, name)
    .map(|value| value.map(|s| s.to_string()))
    .map_err(|err| Error::Other(err.to_string()))
}

#[test]
fn media_playback_rate_advances_current_time_at_scaled_rate() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let html = include_str!("../pages/fixtures/media_playback/playback_rate.html");
  let options = RenderOptions::new().with_viewport(64, 64);

  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<fastrender::BrowserTabHost>::with_clock(clock_for_loop);

  let mut tab = BrowserTab::from_html_with_event_loop(
    html,
    options,
    VmJsBrowserTabExecutor::new(),
    event_loop,
  )?;

  // Parse-time script installs the interval + kicks off playback.
  tab.run_until_stable_with_run_limits(RunLimits::unbounded(), 4)?;

  // Drive deterministic time forward in steps so `setInterval` fires twice, then hit the timeout
  // deadline (the fixture only marks failure at 1s, so reaching it ensures we get a deterministic
  // pass/fail result).
  for delta_ms in [250_u64, 250, 500] {
    clock.advance(Duration::from_millis(delta_ms));
    assert_eq!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
  }

  let marker = tab
    .dom()
    .get_element_by_id("marker")
    .ok_or_else(|| Error::Other("missing marker element".to_string()))?;

  assert_eq!(
    attr(tab.dom(), marker, "data-result")?.as_deref(),
    Some("pass")
  );
  Ok(())
}

