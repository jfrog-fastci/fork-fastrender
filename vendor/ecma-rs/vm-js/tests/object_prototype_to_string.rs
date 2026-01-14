use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

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
fn object_prototype_to_string_tags_arguments_objects() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"Object.prototype.toString.call(function() { return arguments; }()) === "[object Arguments]" &&
         Object.prototype.toString.call(function() { "use strict"; return arguments; }()) === "[object Arguments]""#,
    )
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
fn object_prototype_to_string_tags_regexp() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"Object.prototype.toString.call(/./) === "[object RegExp]" &&
         Object.prototype.toString.call(Object(/./)) === "[object RegExp]" &&
         Object.prototype.toString.call(new RegExp()) === "[object RegExp]" &&
         Object.prototype.toString.call(Object(new RegExp())) === "[object RegExp]""#,
    )
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
fn object_prototype_to_string_weak_map_falls_back_to_weak_map_when_to_string_tag_is_deleted() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var toString = Object.prototype.toString;
         var wm = new WeakMap();
         toString.call(wm) === "[object WeakMap]" &&
         (delete WeakMap.prototype[Symbol.toStringTag], true) &&
         // WeakMap is not part of the legacy builtinTag table; removing `@@toStringTag` falls back
         // to "Object".
         toString.call(wm) === "[object Object]""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_weak_set_falls_back_to_weak_set_when_to_string_tag_is_deleted() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var toString = Object.prototype.toString;
         var ws = new WeakSet();
         toString.call(ws) === "[object WeakSet]" &&
         (delete WeakSet.prototype[Symbol.toStringTag], true) &&
         // WeakSet is not part of the legacy builtinTag table; removing `@@toStringTag` falls back
         // to "Object".
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
fn object_prototype_to_string_tags_regexps_and_uses_builtin_tag_when_to_string_tag_is_deleted() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var toString = Object.prototype.toString;
         var r = /a/;
         // RegExp.prototype[@@toStringTag] should tag both real RegExp objects and objects that
         // inherit from RegExp.prototype.
         toString.call(r) === "[object RegExp]" &&
         toString.call(Object.create(RegExp.prototype)) === "[object RegExp]" &&
         // Deleting @@toStringTag should still tag real RegExp objects via builtinTag but fall back
         // to Object for ordinary objects without RegExp internal slots.
         (delete RegExp.prototype[Symbol.toStringTag], true) &&
         toString.call(r) === "[object RegExp]" &&
         toString.call(Object.create(RegExp.prototype)) === "[object Object]""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prototype_to_string_tags_generator_objects() {
  let mut rt = new_runtime();
  let value = rt
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
  let value = rt
    .exec_script(
      r#"var toString = Object.prototype.toString;
             var g = (function*() {})();
             var genProto = Object.getPrototypeOf(g);
             toString.call(g) === "[object Generator]" &&
             (Object.defineProperty(genProto, Symbol.toStringTag, { configurable: true, get() { return {}; } }), true) &&
             // Non-string @@toStringTag is ignored, so we fall back to the built-in tag.
             toString.call(g) === "[object Object]" &&
             (Object.defineProperty(genProto, Symbol.toStringTag, { configurable: true, writable: false, enumerable: false, value: "Generator" }), true) &&
             toString.call(g) === "[object Generator]""#,
      )
      .unwrap();
  assert_eq!(value, Value::Bool(true));
}
