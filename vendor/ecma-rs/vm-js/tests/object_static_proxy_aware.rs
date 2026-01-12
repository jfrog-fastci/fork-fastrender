use vm_js::{Heap, HeapLimits, JsRuntime, RootId, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Keep a somewhat-larger heap than many of the other unit tests:
  // these tests execute Proxy-heavy scripts that build intermediate arrays/strings for log/order
  // assertions (e.g. `Array.prototype.join`), and minor engine changes can otherwise make the tests
  // flaky due to `OutOfMemory` during `exec_script`.
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_throws_type_error(rt: &mut JsRuntime, script: &str) {
  let err = rt.exec_script(script).unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));

  // Root the thrown value across any subsequent allocations / script runs.
  let root: RootId = rt.heap_mut().add_root(thrown).expect("root thrown value");

  let Value::Object(thrown_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let type_error_proto = rt
    .exec_script("globalThis.TypeError.prototype")
    .expect("evaluate TypeError.prototype");
  let Value::Object(type_error_proto) = type_error_proto else {
    panic!("expected TypeError.prototype to be an object");
  };

  let thrown_proto = rt
    .heap()
    .object_prototype(thrown_obj)
    .expect("get thrown prototype");
  assert_eq!(thrown_proto, Some(type_error_proto));

  rt.heap_mut().remove_root(root);
}

#[test]
fn object_get_own_property_names_observes_proxy_own_keys_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const sym = Symbol("s");
        const target = { a: 1, b: 2, [sym]: 3 };
        const proxy = new Proxy(target, {
          ownKeys(t) { log.push("ownKeys"); return ["b", sym, "a"]; }
        });
        const names = Object.getOwnPropertyNames(proxy);
        names.join("|") === "b|a" && log.join("|") === "ownKeys"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_get_own_property_names_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, { ownKeys() { return []; } });
      r.revoke();
      Object.getOwnPropertyNames(r.proxy);
    "#,
  );
}

#[test]
fn object_get_own_property_symbols_observes_proxy_own_keys_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const s1 = Symbol("s1");
        const s2 = Symbol("s2");
        const proxy = new Proxy({}, {
          ownKeys(t) { log.push("ownKeys"); return ["a", s1, s2]; }
        });
        const syms = Object.getOwnPropertySymbols(proxy);
        syms.length === 2 && syms[0] === s1 && syms[1] === s2 && log.join("|") === "ownKeys"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_get_own_property_symbols_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, { ownKeys() { return []; } });
      r.revoke();
      Object.getOwnPropertySymbols(r.proxy);
    "#,
  );
}

#[test]
fn object_keys_observes_proxy_own_keys_and_gopd_traps() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const sym = Symbol("s");
        const target = { a: 1, b: 2, [sym]: 3 };
        const proxy = new Proxy(target, {
          ownKeys(t) { log.push("ownKeys"); return [sym, "a", "b"]; },
          getOwnPropertyDescriptor(t, p) {
            log.push("gopd:" + String(p));
            return { enumerable: p !== "b", configurable: true };
          }
        });
        const keys = Object.keys(proxy);
        keys.length === 1 && keys[0] === "a" && log.join("|") === "ownKeys|gopd:a|gopd:b"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_keys_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, {
        ownKeys() { return ["a"]; },
        getOwnPropertyDescriptor() { return { enumerable: true, configurable: true }; }
      });
      r.revoke();
      Object.keys(r.proxy);
    "#,
  );
}

#[test]
fn object_values_observes_proxy_own_keys_gopd_and_get_traps() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const sym = Symbol("s");
        const target = { a: 1, b: 2, [sym]: 3 };
        const proxy = new Proxy(target, {
          ownKeys(t) { log.push("ownKeys"); return [sym, "a", "b"]; },
          getOwnPropertyDescriptor(t, p) {
            log.push("gopd:" + String(p));
            return { enumerable: p !== "b", configurable: true };
          },
          get(t, p, r) {
            log.push("get:" + String(p));
            return Reflect.get(t, p, r);
          }
        });
        const values = Object.values(proxy);
        values.length === 1 && values[0] === 1 && log.join("|") === "ownKeys|gopd:a|get:a|gopd:b"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_values_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, {
        ownKeys() { return ["a"]; },
        getOwnPropertyDescriptor() { return { enumerable: true, configurable: true }; },
        get() { return 1; }
      });
      r.revoke();
      Object.values(r.proxy);
    "#,
  );
}

#[test]
fn object_entries_observes_proxy_own_keys_gopd_and_get_traps() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const target = { a: 1, b: 2 };
        const proxy = new Proxy(target, {
          ownKeys(t) { log.push("ownKeys"); return ["a", "b"]; },
          getOwnPropertyDescriptor(t, p) {
            log.push("gopd:" + p);
            return { enumerable: true, configurable: true };
          },
          get(t, p, r) {
            log.push("get:" + p);
            return Reflect.get(t, p, r);
          }
        });
        const entries = Object.entries(proxy);
        entries.length === 2 &&
          entries[0][0] === "a" && entries[0][1] === 1 &&
          entries[1][0] === "b" && entries[1][1] === 2 &&
          log.join("|") === "ownKeys|gopd:a|get:a|gopd:b|get:b"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_entries_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, {
        ownKeys() { return ["a"]; },
        getOwnPropertyDescriptor() { return { enumerable: true, configurable: true }; },
        get() { return 1; }
      });
      r.revoke();
      Object.entries(r.proxy);
    "#,
  );
}

#[test]
fn object_assign_observes_proxy_own_keys_gopd_and_get_traps() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const sym = Symbol("s");
        const sourceTarget = { a: 1, [sym]: 2 };
        const source = new Proxy(sourceTarget, {
          ownKeys(t) { log.push("ownKeys"); return [sym, "a"]; },
          getOwnPropertyDescriptor(t, p) {
            log.push("gopd:" + String(p));
            return { enumerable: true, configurable: true };
          },
          get(t, p, r) {
            log.push("get:" + String(p));
            return Reflect.get(t, p, r);
          }
        });
        const out = Object.assign({}, source);
        out.a === 1 && out[sym] === 2 &&
          log.join("|") === "ownKeys|gopd:Symbol(s)|get:Symbol(s)|gopd:a|get:a"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_assign_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, {
        ownKeys() { return ["a"]; },
        getOwnPropertyDescriptor() { return { enumerable: true, configurable: true }; },
        get() { return 1; }
      });
      r.revoke();
      Object.assign({}, r.proxy);
    "#,
  );
}

#[test]
fn object_get_prototype_of_observes_get_prototype_of_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const proto = {};
        const proxy = new Proxy({}, {
          getPrototypeOf(t) { log.push("getPrototypeOf"); return proto; }
        });
        Object.getPrototypeOf(proxy) === proto && log.join("|") === "getPrototypeOf"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_get_prototype_of_forwards_through_proxy_chain() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const proto = {};
        const inner = new Proxy({}, {
          getPrototypeOf(t) { log.push("inner"); return proto; }
        });
        const outer = new Proxy(inner, {});
        Object.getPrototypeOf(outer) === proto && log.join("|") === "inner"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_get_prototype_of_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, { getPrototypeOf() { return null; } });
      r.revoke();
      Object.getPrototypeOf(r.proxy);
    "#,
  );
}

#[test]
fn object_set_prototype_of_observes_set_prototype_of_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const proto = {};
        const proxy = new Proxy({}, {
          setPrototypeOf(t, p) { log.push(p === proto ? "setPrototypeOf:ok" : "setPrototypeOf:bad"); return true; }
        });
        Object.setPrototypeOf(proxy, proto) === proxy && log.join("|") === "setPrototypeOf:ok"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_set_prototype_of_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, { setPrototypeOf() { return true; } });
      r.revoke();
      Object.setPrototypeOf(r.proxy, {});
    "#,
  );
}

#[test]
fn object_is_extensible_observes_is_extensible_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const proxy = new Proxy({}, {
          isExtensible(t) { log.push("isExtensible"); return true; }
        });
        Object.isExtensible(proxy) === true && log.join("|") === "isExtensible"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_is_extensible_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, { isExtensible() { return true; } });
      r.revoke();
      Object.isExtensible(r.proxy);
    "#,
  );
}

#[test]
fn object_prevent_extensions_observes_prevent_extensions_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const target = {};
        const proxy = new Proxy(target, {
          preventExtensions(t) {
            log.push("preventExtensions");
            Object.preventExtensions(t);
            return true;
          }
        });
        Object.preventExtensions(proxy) === proxy &&
          Object.isExtensible(target) === false &&
          log.join("|") === "preventExtensions"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prevent_extensions_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, { preventExtensions() { return true; } });
      r.revoke();
      Object.preventExtensions(r.proxy);
    "#,
  );
}

#[test]
fn object_define_property_observes_define_property_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const target = {};
        const proxy = new Proxy(target, {
          defineProperty(t, p, desc) {
            log.push("defineProperty:" + p);
            return Reflect.defineProperty(t, p, desc);
          }
        });
        const out = Object.defineProperty(proxy, "a", { value: 1, writable: true, enumerable: true, configurable: true });
        out === proxy && target.a === 1 && log.join("|") === "defineProperty:a"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_define_property_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, { defineProperty() { return true; } });
      r.revoke();
      Object.defineProperty(r.proxy, "a", { value: 1 });
    "#,
  );
}

#[test]
fn object_from_entries_reads_entries_via_proxy_get_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        globalThis.log = [];
        const entryTarget = ["a", 1];
        const entry = new Proxy(entryTarget, {
          get(t, p, r) {
            log.push("get:" + String(p));
            return Reflect.get(t, p, r);
          }
        });
        const out = Object.fromEntries([entry]);
        out.a === 1 && log.join("|") === "get:0|get:1"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_from_entries_throws_type_error_on_revoked_proxy_entry() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable(["a", 1], { get(t, p, r) { return Reflect.get(t, p, r); } });
      r.revoke();
      Object.fromEntries([r.proxy]);
    "#,
  );
}
