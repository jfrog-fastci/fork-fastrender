use crate::error::{Error, Result};
use crate::js::event_loop::{EventLoop, TimerId};
use std::time::Duration;

/// Minimal JS value representation used by the timer APIs.
///
/// This is intentionally small (it only needs to support additional args passed to timer callbacks
/// in early harness tests). A future `ecma-rs` runtime embedding should replace these with real JS
/// values.
#[derive(Debug, Clone, PartialEq)]
pub enum JsValue {
  Undefined,
  Null,
  Bool(bool),
  Number(f64),
  String(String),
}

/// A timer handler passed to `setTimeout`/`setInterval`/`queueMicrotask`.
///
/// Per the web platform APIs, the handler must be callable. String handlers are allowed by the
/// HTML Standard but are **intentionally rejected** for now (we throw a `TypeError`) to avoid
/// evaluating arbitrary strings as code in the host environment.
pub enum TimerHandler<Host: 'static> {
  Function(Box<dyn FnMut(&mut Host, &mut EventLoop<Host>, &[JsValue]) -> Result<()> + 'static>),
  String(String),
  NotCallable,
}

impl<Host: 'static> TimerHandler<Host> {
  pub fn from_fn<F>(f: F) -> Self
  where
    F: FnMut(&mut Host, &mut EventLoop<Host>, &[JsValue]) -> Result<()> + 'static,
  {
    Self::Function(Box::new(f))
  }
}

fn type_error(message: &str) -> Error {
  Error::Other(format!("TypeError: {message}"))
}

#[allow(non_snake_case)]
pub fn setTimeout<Host: 'static>(
  event_loop: &mut EventLoop<Host>,
  handler: TimerHandler<Host>,
  timeout_ms: i64,
  args: Vec<JsValue>,
) -> Result<TimerId> {
  let delay_ms = timeout_ms.max(0) as u64;
  let delay = Duration::from_millis(delay_ms);
  match handler {
    TimerHandler::Function(mut callback) => event_loop
      .set_timeout(delay, move |host, event_loop| {
        callback(host, event_loop, &args)
      }),
    TimerHandler::String(_) => Err(type_error(
      "setTimeout does not currently support string handlers",
    )),
    TimerHandler::NotCallable => Err(type_error("setTimeout callback is not callable")),
  }
}

#[allow(non_snake_case)]
pub fn clearTimeout<Host: 'static>(event_loop: &mut EventLoop<Host>, id: TimerId) {
  event_loop.clear_timeout(id);
}

#[allow(non_snake_case)]
pub fn setInterval<Host: 'static>(
  event_loop: &mut EventLoop<Host>,
  handler: TimerHandler<Host>,
  timeout_ms: i64,
  args: Vec<JsValue>,
) -> Result<TimerId> {
  let interval_ms = timeout_ms.max(0) as u64;
  let interval = Duration::from_millis(interval_ms);
  match handler {
    TimerHandler::Function(mut callback) => event_loop
      .set_interval(interval, move |host, event_loop| {
        callback(host, event_loop, &args)
      }),
    TimerHandler::String(_) => Err(type_error(
      "setInterval does not currently support string handlers",
    )),
    TimerHandler::NotCallable => Err(type_error("setInterval callback is not callable")),
  }
}

#[allow(non_snake_case)]
pub fn clearInterval<Host: 'static>(event_loop: &mut EventLoop<Host>, id: TimerId) {
  event_loop.clear_interval(id);
}

#[allow(non_snake_case)]
pub fn queueMicrotask<Host: 'static>(
  event_loop: &mut EventLoop<Host>,
  callback: TimerHandler<Host>,
) -> Result<()> {
  match callback {
    TimerHandler::Function(mut callback) => {
      event_loop.queue_microtask(move |host, event_loop| callback(host, event_loop, &[]))?;
      Ok(())
    }
    TimerHandler::String(_) => Err(type_error(
      "queueMicrotask does not currently support string callbacks",
    )),
    TimerHandler::NotCallable => Err(type_error("queueMicrotask callback is not callable")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use crate::js::event_loop::{RunLimits, RunUntilIdleOutcome, TaskSource};
  use std::cell::Cell;
  use std::rc::Rc;
  use std::sync::Arc;

  #[test]
  fn ordering_timeout_after_microtask() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      setTimeout(
        event_loop,
        TimerHandler::<Host>::from_fn(|host, _event_loop, _args| {
          host.log.push("t");
          Ok(())
        }),
        0,
        vec![],
      )?;

      queueMicrotask(
        event_loop,
        TimerHandler::<Host>::from_fn(|host, _event_loop, _args| {
          host.log.push("m");
          Ok(())
        }),
      )?;

      host.log.push("sync");
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.log, vec!["sync", "m", "t"]);
    Ok(())
  }

  #[test]
  fn cancellation_timeout() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    event_loop.queue_task(TaskSource::Script, |_host, event_loop| {
      let id = setTimeout(
        event_loop,
        TimerHandler::<Host>::from_fn(|host, _event_loop, _args| {
          host.log.push("t");
          Ok(())
        }),
        0,
        vec![],
      )?;
      clearTimeout(event_loop, id);
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(host.log.is_empty());
    Ok(())
  }

  #[test]
  fn interval_repeats_and_can_be_cancelled() -> Result<()> {
    #[derive(Default)]
    struct Host {
      count: usize,
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);

    let id_cell: Rc<Cell<TimerId>> = Rc::new(Cell::new(0));
    let id_cell_for_cb = id_cell.clone();

    let id = setInterval(
      &mut event_loop,
      TimerHandler::<Host>::from_fn(move |host, event_loop, _args| {
        host.count += 1;
        if host.count == 3 {
          clearInterval(event_loop, id_cell_for_cb.get());
        }
        Ok(())
      }),
      0,
      vec![],
    )?;
    id_cell.set(id);

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 10,
          max_microtasks: 100,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 3);
    Ok(())
  }

  #[test]
  fn timeout_passes_additional_args_to_callback() -> Result<()> {
    #[derive(Default)]
    struct Host {
      observed: Vec<JsValue>,
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    event_loop.queue_task(TaskSource::Script, |_host, event_loop| {
      setTimeout(
        event_loop,
        TimerHandler::<Host>::from_fn(|host, _event_loop, args| {
          host.observed.extend_from_slice(args);
          Ok(())
        }),
        0,
        vec![JsValue::Number(1.0), JsValue::String("x".to_string())],
      )?;
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(
      host.observed,
      vec![JsValue::Number(1.0), JsValue::String("x".to_string())]
    );
    Ok(())
  }

  #[test]
  fn string_handlers_throw_type_error() {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<()>::with_clock(clock);
    let err = setTimeout(
      &mut event_loop,
      TimerHandler::String("alert(1)".to_string()),
      0,
      vec![],
    )
    .expect_err("string handlers should be rejected");
    assert!(matches!(err, Error::Other(msg) if msg.starts_with("TypeError:")));
  }
}
