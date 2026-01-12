use vm_js::{Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_prototype_to_string_respects_symbol_to_string_tag() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = { [Symbol.toStringTag]: "X" }; Object.prototype.toString.call(o) === "[object X]""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_tags_arrays() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Object.prototype.toString.call([]) === "[object Array]""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_tags_promises() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Object.prototype.toString.call(Promise.resolve()) === "[object Promise]""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_promise_falls_back_to_object_when_to_string_tag_is_deleted() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var toString = Object.prototype.toString;
         var p = Promise.resolve();
         toString.call(p) === "[object Promise]" &&
         (delete Promise.prototype[Symbol.toStringTag], true) &&
         toString.call(p) === "[object Object]""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_weak_map_falls_back_to_object_when_to_string_tag_is_deleted() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var toString = Object.prototype.toString;
         var wm = new WeakMap();
         toString.call(wm) === "[object WeakMap]" &&
         (delete WeakMap.prototype[Symbol.toStringTag], true) &&
         toString.call(wm) === "[object Object]""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_weak_set_falls_back_to_object_when_to_string_tag_is_deleted() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var toString = Object.prototype.toString;
         var ws = new WeakSet();
         toString.call(ws) === "[object WeakSet]" &&
         (delete WeakSet.prototype[Symbol.toStringTag], true) &&
         toString.call(ws) === "[object Object]""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_tags_promise_prototype_via_symbol_to_string_tag() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"Object.prototype.toString.call(Promise.prototype) === "[object Promise]" &&
         Object.prototype.toString.call(Object.create(Promise.prototype)) === "[object Promise]""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_tags_generator_objects() {
  let mut rt = new_runtime();
  let value = rt
    // `vm-js` does not yet implement generator execution (`(function*() {})()`), but generator
    // functions still create a per-function `.prototype` object that inherits from
    // `%GeneratorPrototype%`, which defines `@@toStringTag = "Generator"`.
    .exec_script(
      r#"var proto = (function*() {}).prototype;
         var o = Object.create(proto);
         Object.prototype.toString.call(o) === "[object Generator]""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_tags_generator_heap_object() {
  let mut rt = new_runtime();

  // Allocate a Generator heap object directly (generator execution is not yet implemented).
  {
    let (_vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let intr = *realm.intrinsics();
    let global = realm.global_object();

    let mut scope = heap.scope();
    let gen = scope
      .alloc_generator_with_prototype(Some(intr.generator_prototype()), Value::Undefined, &[], None)
      .unwrap();
    scope.push_root(Value::Object(gen)).unwrap();

    let key = PropertyKey::from_string(scope.alloc_string("g").unwrap());
    scope
      .define_property(
        global,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(gen),
            writable: true,
          },
        },
      )
      .unwrap();
  }

  let value = rt
    .exec_script(
      r#"var toString = Object.prototype.toString;
         var genProto = Object.getPrototypeOf(g);
         toString.call(g) === "[object Generator]" &&
         (Object.defineProperty(genProto, Symbol.toStringTag, { configurable: true, get() { return {}; } }), true) &&
         toString.call(g) === "[object Object]" &&
         (Object.defineProperty(genProto, Symbol.toStringTag, { configurable: true, writable: false, enumerable: false, value: "Generator" }), true) &&
         toString.call(g) === "[object Generator]""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
