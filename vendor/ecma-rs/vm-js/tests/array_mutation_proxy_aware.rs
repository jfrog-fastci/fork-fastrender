use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_reverse_is_proxy_trap_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var target = { length: 2, 0: "a", 1: "b" };
      var p = new Proxy(target, {
        has: function (t, k) {
          if (k === "0" || k === "1") log.push("has:" + String(k));
          return k in t;
        },
        get: function (t, k, r) {
          if (k === "length" || k === "0" || k === "1") log.push("get:" + String(k));
          return Reflect.get(t, k, r);
        },
        set: function (t, k, v, r) {
          if (k === "0" || k === "1") log.push("set:" + String(k));
          return Reflect.set(t, k, v, r);
        },
      });

      var ok = true;
      try {
        Array.prototype.reverse.call(p);
      } catch (e) {
        ok = false;
      }

      ok
        && target[0] === "b"
        && target[1] === "a"
        && log.join(",") === "get:length,has:0,has:1,get:0,get:1,set:0,set:1";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn array_sort_is_proxy_trap_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var target = { length: 2, 0: "b", 1: "a" };
      var p = new Proxy(target, {
        has: function (t, k) {
          if (k === "0" || k === "1") log.push("has:" + String(k));
          return k in t;
        },
        get: function (t, k, r) {
          if (k === "length" || k === "0" || k === "1") log.push("get:" + String(k));
          return Reflect.get(t, k, r);
        },
        set: function (t, k, v, r) {
          if (k === "0" || k === "1") log.push("set:" + String(k));
          return Reflect.set(t, k, v, r);
        },
        deleteProperty: function (t, k) {
          if (k === "0" || k === "1") log.push("delete:" + String(k));
          return Reflect.deleteProperty(t, k);
        },
      });

      var ok = true;
      try {
        Array.prototype.sort.call(p);
      } catch (e) {
        ok = false;
      }

      ok
        && target[0] === "a"
        && target[1] === "b"
        && log.join(",") === "get:length,has:0,get:0,has:1,get:1,set:0,set:1";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn array_push_is_proxy_trap_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var target = { length: 0 };
      var p = new Proxy(target, {
        get: function (t, k, r) {
          if (k === "length") log.push("get:" + String(k));
          return Reflect.get(t, k, r);
        },
        set: function (t, k, v, r) {
          if (k === "0" || k === "length") log.push("set:" + String(k));
          return Reflect.set(t, k, v, r);
        },
      });

      var ok = true;
      var out = -1;
      try {
        out = Array.prototype.push.call(p, "x");
      } catch (e) {
        ok = false;
      }

      ok
        && out === 1
        && target.length === 1
        && target[0] === "x"
        && log.join(",") === "get:length,set:0,set:length";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn array_pop_is_proxy_trap_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var target = { length: 1, 0: "x" };
      var p = new Proxy(target, {
        get: function (t, k, r) {
          if (k === "length" || k === "0") log.push("get:" + String(k));
          return Reflect.get(t, k, r);
        },
        set: function (t, k, v, r) {
          if (k === "length") log.push("set:" + String(k));
          return Reflect.set(t, k, v, r);
        },
        deleteProperty: function (t, k) {
          if (k === "0") log.push("delete:" + String(k));
          return Reflect.deleteProperty(t, k);
        },
      });

      var ok = true;
      var out;
      try {
        out = Array.prototype.pop.call(p);
      } catch (e) {
        ok = false;
      }

      ok
        && out === "x"
        && target.length === 0
        && !("0" in target)
        && log.join(",") === "get:length,get:0,delete:0,set:length";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn array_shift_is_proxy_trap_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var target = { length: 2, 0: "a", 1: "b" };
      var p = new Proxy(target, {
        has: function (t, k) {
          if (k === "1") log.push("has:" + String(k));
          return k in t;
        },
        get: function (t, k, r) {
          if (k === "length" || k === "0" || k === "1") log.push("get:" + String(k));
          return Reflect.get(t, k, r);
        },
        set: function (t, k, v, r) {
          if (k === "0" || k === "length") log.push("set:" + String(k));
          return Reflect.set(t, k, v, r);
        },
        deleteProperty: function (t, k) {
          if (k === "1") log.push("delete:" + String(k));
          return Reflect.deleteProperty(t, k);
        },
      });

      var ok = true;
      var out;
      try {
        out = Array.prototype.shift.call(p);
      } catch (e) {
        ok = false;
      }

      ok
        && out === "a"
        && target.length === 1
        && target[0] === "b"
        && !("1" in target)
        && log.join(",") === "get:length,get:0,has:1,get:1,set:0,delete:1,set:length";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn array_unshift_is_proxy_trap_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var target = { length: 1, 0: "b" };
      var p = new Proxy(target, {
        has: function (t, k) {
          if (k === "0") log.push("has:" + String(k));
          return k in t;
        },
        get: function (t, k, r) {
          if (k === "length" || k === "0") log.push("get:" + String(k));
          return Reflect.get(t, k, r);
        },
        set: function (t, k, v, r) {
          if (k === "0" || k === "1" || k === "length") log.push("set:" + String(k));
          return Reflect.set(t, k, v, r);
        },
        deleteProperty: function (t, k) {
          if (k === "0" || k === "1") log.push("delete:" + String(k));
          return Reflect.deleteProperty(t, k);
        },
      });

      var ok = true;
      var out;
      try {
        out = Array.prototype.unshift.call(p, "a");
      } catch (e) {
        ok = false;
      }

      ok
        && out === 2
        && target.length === 2
        && target[0] === "a"
        && target[1] === "b"
        && log.join(",") === "get:length,has:0,get:0,set:1,set:0,set:length";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn array_splice_is_proxy_trap_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var target = { length: 3, 0: "a", 1: "b", 2: "c" };
      var p = new Proxy(target, {
        has: function (t, k) {
          if (k === "0" || k === "1" || k === "2") log.push("has:" + String(k));
          return k in t;
        },
        get: function (t, k, r) {
          if (k === "length" || k === "0" || k === "1" || k === "2") log.push("get:" + String(k));
          return Reflect.get(t, k, r);
        },
        set: function (t, k, v, r) {
          if (k === "1" || k === "length") log.push("set:" + String(k));
          return Reflect.set(t, k, v, r);
        },
        deleteProperty: function (t, k) {
          if (k === "2") log.push("delete:" + String(k));
          return Reflect.deleteProperty(t, k);
        },
      });

      var ok = true;
      var removed;
      try {
        removed = Array.prototype.splice.call(p, 1, 1);
      } catch (e) {
        ok = false;
      }

      ok
        && removed.length === 1
        && removed[0] === "b"
        && target.length === 2
        && target[0] === "a"
        && target[1] === "c"
        && !("2" in target)
        && log.join(",") === "get:length,has:1,get:1,has:2,get:2,set:1,delete:2,set:length";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
