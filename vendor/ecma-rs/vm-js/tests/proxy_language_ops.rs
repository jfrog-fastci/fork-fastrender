use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn proxy_get_trap_is_used_for_property_access() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = "";
      var p = new Proxy({}, {
        get: function(target, prop, receiver) {
          log += "get:" + prop + "|";
          return 123;
        }
      });
      var v = p.foo;
      log === "get:foo|" && v === 123
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_set_trap_is_used_for_property_assignment() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = "";
      var p = new Proxy({}, {
        set: function(target, prop, value, receiver) {
          log += "set:" + prop + "=" + value + "|";
          return true;
        }
      });
      p.foo = 7;
      log === "set:foo=7|"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_set_trap_false_throws_in_strict_mode() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = "";
      var p = new Proxy({}, {
        set: function(target, prop, value, receiver) {
          log += "set:" + prop + "|";
          return false;
        }
      });
      var threw = false;
      try {
        (function() { "use strict"; p.foo = 1; })();
      } catch (e) {
        threw = e && e.name === "TypeError";
      }
      threw && log === "set:foo|"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_delete_and_in_operator_use_traps() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = "";
      var p = new Proxy({x: 1}, {
        deleteProperty: function(target, prop) {
          log += "del:" + prop + "|";
          return true;
        },
        has: function(target, prop) {
          log += "has:" + prop + "|";
          return prop === "x";
        }
      });
      var d = delete p.x;
      var h = ("x" in p);
      d === true && h === true && log === "del:x|has:x|"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_object_spread_uses_copy_data_properties_dispatch() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = "";
      var target = {};
      Object.defineProperty(target, "a", { value: 1, enumerable: true, configurable: true, writable: true });
      Object.defineProperty(target, "b", { value: 2, enumerable: false, configurable: true, writable: true });
      var p = new Proxy(target, {
        ownKeys: function(t) {
          log += "ownKeys|";
          return ["a", "b"];
        },
        getOwnPropertyDescriptor: function(t, prop) {
          log += "gOPD:" + prop + "|";
          if (prop === "a") return { value: 1, enumerable: true, configurable: true, writable: true };
          if (prop === "b") return { value: 2, enumerable: false, configurable: true, writable: true };
          return undefined;
        },
        get: function(t, prop, receiver) {
          log += "get:" + prop + "|";
          return t[prop];
        }
      });
      var o = { ...p };
      log === "ownKeys|gOPD:a|get:a|gOPD:b|" && o.a === 1 && !("b" in o)
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_object_spread_copies_symbol_keys_via_dispatch() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = "";
      var sym = Symbol("s");
      var target = {};
      Object.defineProperty(target, sym, { value: 1, enumerable: true, configurable: true, writable: true });
      Object.defineProperty(target, "a", { value: 2, enumerable: true, configurable: true, writable: true });
      var p = new Proxy(target, {
        ownKeys: function(t) {
          log += "ownKeys|";
          return [sym, "a"];
        },
        getOwnPropertyDescriptor: function(t, prop) {
          log += "gOPD:" + (prop === sym ? "sym" : prop) + "|";
          if (prop === sym) return { value: 1, enumerable: true, configurable: true, writable: true };
          if (prop === "a") return { value: 2, enumerable: true, configurable: true, writable: true };
          return undefined;
        },
        get: function(t, prop, receiver) {
          log += "get:" + (prop === sym ? "sym" : prop) + "|";
          return t[prop];
        }
      });
      var o = { ...p };
      log === "ownKeys|gOPD:sym|get:sym|gOPD:a|get:a|" && o[sym] === 1 && o.a === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_object_rest_destructuring_uses_copy_data_properties_dispatch() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = "";
      var target = {};
      Object.defineProperty(target, "a", { value: 1, enumerable: true, configurable: true, writable: true });
      Object.defineProperty(target, "b", { value: 2, enumerable: true, configurable: true, writable: true });
      Object.defineProperty(target, "c", { value: 3, enumerable: false, configurable: true, writable: true });
      var p = new Proxy(target, {
        ownKeys: function(t) {
          log += "ownKeys|";
          return ["a", "b", "c"];
        },
        getOwnPropertyDescriptor: function(t, prop) {
          log += "gOPD:" + prop + "|";
          if (prop === "a") return { value: 1, enumerable: true, configurable: true, writable: true };
          if (prop === "b") return { value: 2, enumerable: true, configurable: true, writable: true };
          if (prop === "c") return { value: 3, enumerable: false, configurable: true, writable: true };
          return undefined;
        },
        get: function(t, prop, receiver) {
          log += "get:" + prop + "|";
          return t[prop];
        }
      });
      var { a, ...rest } = p;
      log === "get:a|ownKeys|gOPD:b|get:b|gOPD:c|" &&
        a === 1 &&
        rest.b === 2 &&
        !("a" in rest) &&
        !("c" in rest)
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_object_rest_destructuring_excludes_symbol_keys() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = "";
      var sym = Symbol("s");
      var target = {};
      Object.defineProperty(target, sym, { value: 1, enumerable: true, configurable: true, writable: true });
      Object.defineProperty(target, "a", { value: 2, enumerable: true, configurable: true, writable: true });
      var p = new Proxy(target, {
        ownKeys: function(t) {
          log += "ownKeys|";
          return [sym, "a"];
        },
        getOwnPropertyDescriptor: function(t, prop) {
          log += "gOPD:" + (prop === sym ? "sym" : prop) + "|";
          if (prop === sym) return { value: 1, enumerable: true, configurable: true, writable: true };
          if (prop === "a") return { value: 2, enumerable: true, configurable: true, writable: true };
          return undefined;
        },
        get: function(t, prop, receiver) {
          log += "get:" + (prop === sym ? "sym" : prop) + "|";
          return t[prop];
        }
      });
      var { [sym]: x, ...rest } = p;
      log === "get:sym|ownKeys|gOPD:a|get:a|" && x === 1 && rest.a === 2 && !(sym in rest)
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
