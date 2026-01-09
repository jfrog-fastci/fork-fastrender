use super::event_loop::EventLoop;
use super::runtime::{JsObject, JsRuntime};
use super::window_timers::JsValue;
use std::time::Duration;

/// Deterministic web time model for JavaScript APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebTime {
  /// The Unix epoch time (ms) that corresponds to `performance.now() == 0`.
  ///
  /// In tests this should default to `0` for determinism. Real hosts may set this to an actual
  /// epoch timestamp.
  pub time_origin_unix_ms: i64,
}

impl Default for WebTime {
  fn default() -> Self {
    Self {
      time_origin_unix_ms: 0,
    }
  }
}

impl WebTime {
  pub fn new(time_origin_unix_ms: i64) -> Self {
    Self { time_origin_unix_ms }
  }

  /// Implementation of `performance.now()`.
  pub fn performance_now<Host>(&self, event_loop: &EventLoop<Host>) -> f64 {
    duration_to_ms_f64(event_loop.now())
  }

  /// Implementation of `Date.now()`.
  pub fn date_now<Host>(&self, event_loop: &EventLoop<Host>) -> i64 {
    self.time_origin_unix_ms.saturating_add(duration_to_millis_i64(event_loop.now()))
  }
}

/// Installs `Date.now()` and `performance.now()` bindings into the JS runtime.
pub fn install_time_bindings<Host: 'static, R: JsRuntime<Host>>(
  runtime: &mut R,
  web_time: WebTime,
) {
  runtime
    .global_object("Date")
    .define_method("now", Box::new(move |_host, event_loop| {
      Ok(JsValue::Number(web_time.date_now(event_loop) as f64))
    }));

  runtime
    .global_object("performance")
    .define_method("now", Box::new(move |_host, event_loop| {
      Ok(JsValue::Number(web_time.performance_now(event_loop)))
    }));
}

fn duration_to_ms_f64(duration: Duration) -> f64 {
  let nanos = duration.as_nanos();
  let millis = nanos / 1_000_000;
  let rem_nanos = nanos % 1_000_000;
  millis as f64 + rem_nanos as f64 / 1_000_000.0
}

fn duration_to_millis_i64(duration: Duration) -> i64 {
  let millis = duration.as_millis();
  if millis > i64::MAX as u128 {
    i64::MAX
  } else {
    millis as i64
  }
}
