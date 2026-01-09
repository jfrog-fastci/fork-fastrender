use fastrender::js::{
  install_time_bindings, EventLoop, JsObject, JsRuntime, JsValue, NativeFunction, VirtualClock,
  WebTime,
};
use fastrender::{Error, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

struct TestRuntime<Host> {
  globals: HashMap<String, TestValue<Host>>,
}

impl<Host> Default for TestRuntime<Host> {
  fn default() -> Self {
    Self {
      globals: HashMap::new(),
    }
  }
}

enum TestValue<Host> {
  Function(NativeFunction<Host>),
  Object(TestObject<Host>),
}

struct TestObject<Host> {
  props: HashMap<String, TestValue<Host>>,
}

impl<Host> Default for TestObject<Host> {
  fn default() -> Self {
    Self {
      props: HashMap::new(),
    }
  }
}

impl<Host> JsObject<Host> for TestObject<Host> {
  fn define_method(&mut self, name: &str, func: NativeFunction<Host>) {
    self.props.insert(name.to_string(), TestValue::Function(func));
  }
}

impl<Host> JsRuntime<Host> for TestRuntime<Host> {
  type Object = TestObject<Host>;

  fn global_object(&mut self, name: &str) -> &mut Self::Object {
    let entry = self
      .globals
      .entry(name.to_string())
      .or_insert_with(|| TestValue::Object(TestObject::default()));
    match entry {
      TestValue::Object(obj) => obj,
      TestValue::Function(_) => panic!("global `{name}` is not an object"),
    }
  }

  fn define_global_function(&mut self, name: &str, func: NativeFunction<Host>) {
    self.globals.insert(name.to_string(), TestValue::Function(func));
  }
}

impl<Host> TestRuntime<Host> {
  fn call_global_method_number(
    &self,
    object: &str,
    method: &str,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<f64> {
    let obj = match self.globals.get(object) {
      Some(TestValue::Object(obj)) => obj,
      Some(TestValue::Function(_)) => {
        return Err(Error::Other(format!("global `{object}` is not an object")));
      }
      None => return Err(Error::Other(format!("missing global `{object}`"))),
    };

    let func = match obj.props.get(method) {
      Some(TestValue::Function(func)) => func,
      Some(TestValue::Object(_)) => {
        return Err(Error::Other(format!(
          "property `{object}.{method}` is not a function"
        )));
      }
      None => return Err(Error::Other(format!("missing property `{object}.{method}`"))),
    };

    let value = func(host, event_loop)?;
    match value {
      JsValue::Number(n) => Ok(n),
    }
  }
}

#[test]
fn date_now_and_performance_now_follow_event_loop_clock() -> Result<()> {
  #[derive(Default)]
  struct Host;

  let mut host = Host::default();
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());
  let mut runtime = TestRuntime::<Host>::default();

  install_time_bindings(
    &mut runtime,
    WebTime {
      time_origin_unix_ms: 1_000,
    },
  );

  assert_eq!(
    runtime.call_global_method_number("performance", "now", &mut host, &mut event_loop)?,
    0.0
  );
  assert_eq!(
    runtime.call_global_method_number("Date", "now", &mut host, &mut event_loop)?,
    1_000.0
  );

  assert_eq!(
    runtime.call_global_method_number("performance", "now", &mut host, &mut event_loop)?,
    0.0
  );

  clock.advance(Duration::from_micros(1500));
  assert_eq!(
    runtime.call_global_method_number("performance", "now", &mut host, &mut event_loop)?,
    1.5
  );
  assert_eq!(
    runtime.call_global_method_number("Date", "now", &mut host, &mut event_loop)?,
    1_001.0
  );

  clock.advance(Duration::from_millis(10));
  assert_eq!(
    runtime.call_global_method_number("performance", "now", &mut host, &mut event_loop)?,
    11.5
  );
  assert_eq!(
    runtime.call_global_method_number("Date", "now", &mut host, &mut event_loop)?,
    1_011.0
  );

  Ok(())
}
