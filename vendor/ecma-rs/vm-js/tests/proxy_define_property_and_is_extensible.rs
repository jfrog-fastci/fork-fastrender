use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn proxy_define_property_trap_observes_call_parameters() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var _handler, _target, _prop, _desc;
        var target = {};
        var handler = {
          defineProperty: function (t, prop, desc) {
            _handler = this;
            _target = t;
            _prop = prop;
            _desc = desc;
            return true;
          }
        };
        var p = new Proxy(target, handler);
        Object.defineProperty(p, "attr", {
          configurable: true,
          enumerable: true,
          writable: true,
          value: 1
        });
        _handler === handler &&
          _target === target &&
          _prop === "attr" &&
          Object.keys(_desc).length === 4 &&
          _desc.configurable === true &&
          _desc.enumerable === true &&
          _desc.writable === true &&
          _desc.value === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_define_property_invariants_reject_configurable_false_on_configurable_target() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var target = {};
        Object.defineProperty(target, "foo", { value: 1, configurable: true });
        var p = new Proxy(target, {
          defineProperty: function (t, prop, desc) {
            return true;
          }
        });
        try {
          Object.defineProperty(p, "foo", { value: 1, configurable: false });
          false
        } catch (e) {
          e && e.name === "TypeError"
        }
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_define_property_invariants_reject_incompatible_descriptor() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var target = {};
        Object.defineProperty(target, "foo", { value: 1, configurable: false });
        var p = new Proxy(target, {
          defineProperty: function (t, prop, desc) {
            return true;
          }
        });
        try {
          Object.defineProperty(p, "foo", { value: 1, configurable: true });
          false
        } catch (e) {
          e && e.name === "TypeError"
        }
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn reflect_define_property_uses_proxy_define_property_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var target = {};
        var p = new Proxy(target, {
          defineProperty: function (t, prop, desc) {
            return Object.defineProperty(t, prop, desc);
          }
        });
        var ok = Reflect.defineProperty(p, "attr", {
          configurable: true,
          enumerable: true,
          writable: true,
          value: 1
        });
        var d = Object.getOwnPropertyDescriptor(target, "attr");
        ok === true &&
          d.value === 1 &&
          d.writable === true &&
          d.enumerable === true &&
          d.configurable === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn set_without_set_trap_can_trigger_proxy_define_property_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var log = "";
        var target = {};
        var p = new Proxy(target, {
          defineProperty: function (t, prop, desc) {
            log += "defineProperty:" + prop + "=" + desc.value + "|";
            return Object.defineProperty(t, prop, desc);
          }
        });
        p.x = 1;
        log === "defineProperty:x=1|" && target.x === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_is_extensible_trap_is_observable_and_invariant_checked() {
  let mut rt = new_runtime();

  // Call parameters (this-binding + target arg) and return value.
  let value = rt
    .exec_script(
      r#"
        var seen = false;
        var target = {};
        var handler = {
          isExtensible: function (t) {
            seen = (this === handler) && (t === target);
            return true;
          }
        };
        var p = new Proxy(target, handler);
        Reflect.isExtensible(p) === true && seen === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  // Invariant: trap result must match target's actual extensibility.
  let value = rt
    .exec_script(
      r#"
        var target = {};
        var p = new Proxy(target, {
          isExtensible: function (t) { return false; }
        });
        try {
          Reflect.isExtensible(p);
          false
        } catch (e) {
          e && e.name === "TypeError"
        }
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

