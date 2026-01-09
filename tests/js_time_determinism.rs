use fastrender::js::{install_time_bindings, Clock, EventLoop, VirtualClock, WebTime};
use std::sync::Arc;
use std::time::Duration;
use vm_js::{Heap, PropertyKey, Realm, Value, Vm, VmOptions};

fn get_global_property(heap: &mut Heap, realm: &Realm, name: &str) -> Value {
  let mut scope = heap.scope();
  let key_s = scope.alloc_string(name).expect("alloc key string");
  scope
    .push_root(Value::String(key_s))
    .expect("push_root key string");
  let key = PropertyKey::from_string(key_s);
  let global = realm.global_object();
  scope
    .heap()
    .object_get_own_data_property_value(global, &key)
    .expect("get global property")
    .unwrap_or_else(|| panic!("missing global property {name}"))
}

fn get_object_property(heap: &mut Heap, obj: vm_js::GcObject, name: &str) -> Value {
  let mut scope = heap.scope();
  let key_s = scope.alloc_string(name).expect("alloc key string");
  scope
    .push_root(Value::String(key_s))
    .expect("push_root key string");
  let key = PropertyKey::from_string(key_s);
  scope
    .heap()
    .object_get_own_data_property_value(obj, &key)
    .expect("get object property")
    .unwrap_or_else(|| panic!("missing property {name}"))
}

fn call0(vm: &mut Vm, heap: &mut Heap, callee: Value, this: Value) -> Value {
  let mut scope = heap.scope();
  vm
    .call(&mut scope, callee, this, &[])
    .expect("call should succeed")
}

#[test]
fn date_now_and_performance_now_follow_event_loop_clock() {
  let clock = Arc::new(VirtualClock::new());
  let clock_for_bindings: Arc<dyn Clock> = clock.clone();
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<()>::with_clock(clock_for_loop);

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap).expect("create realm");

  let web_time = WebTime::new(1_000);
  let _bindings = install_time_bindings(&mut vm, &realm, &mut heap, clock_for_bindings, web_time)
    .expect("install time bindings");

  // Start at a deterministic time.
  clock.set_now(Duration::from_millis(0));
  assert_eq!(event_loop.now(), Duration::from_millis(0));

  let date = get_global_property(&mut heap, &realm, "Date");
  let performance = get_global_property(&mut heap, &realm, "performance");

  let Value::Object(date_obj) = date else {
    panic!("Date should be an object");
  };
  let Value::Object(performance_obj) = performance else {
    panic!("performance should be an object");
  };

  let date_now = get_object_property(&mut heap, date_obj, "now");
  let perf_now = get_object_property(&mut heap, performance_obj, "now");
  let v = call0(&mut vm, &mut heap, date_now, Value::Object(date_obj));
  assert_eq!(
    v,
    Value::Number(web_time.date_now(&event_loop) as f64),
    "Date.now should incorporate WebTime origin + EventLoop clock"
  );

  let v = call0(&mut vm, &mut heap, perf_now, Value::Object(performance_obj));
  assert_eq!(
    v,
    Value::Number(web_time.performance_now(&event_loop)),
    "performance.now should track EventLoop clock"
  );

  // Advance the virtual clock to a non-integer millisecond.
  clock.set_now(Duration::from_nanos(1_234_567_890)); // 1234.56789ms
  assert_eq!(event_loop.now(), Duration::from_nanos(1_234_567_890));

  let v = call0(&mut vm, &mut heap, date_now, Value::Object(date_obj));
  // Date.now() is millisecond-granularity.
  assert_eq!(v, Value::Number(web_time.date_now(&event_loop) as f64));

  let v = call0(&mut vm, &mut heap, perf_now, Value::Object(performance_obj));
  let Value::Number(n) = v else {
    panic!("performance.now should return a number");
  };
  assert!((n - web_time.performance_now(&event_loop)).abs() < 1e-9);

  // `vm-js` realms own persistent GC roots that must be explicitly removed.
  realm.teardown(&mut heap);
}
