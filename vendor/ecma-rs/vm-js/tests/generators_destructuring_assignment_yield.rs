use vm_js::{Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmOptions};

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_destructuring_assignment_rhs_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { let a = 0; ({a} = yield 0); return a; }
        var it = g();
        var r1 = it.next();
        var r2 = it.next({a: 123});
        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === 123
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_rhs_from_yield_resumption() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { let a = 0; ([a] = yield 0); return a; }
        var it = g();
        it.next();
        var r = it.next([42]);
        r.done === true && r.value === 42
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_destructuring_assignment_expression_returns_rhs_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { let a = 0; var v = ({a} = yield 0); return v.a === 7 && a === 7; }
        var it = g();
        it.next();
        var r = it.next({a: 7});
        r.done === true && r.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_destructuring_assignment_rhs_from_yield_then_pattern_yields_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var assigned;
          function* g() {
            var a = 0;
            assigned = ({[(yield 1)]: a} = yield 0);
            return a;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;

          var rhs = {x: 5};
          var r2 = it.next(rhs);
          if (r2.done !== false || r2.value !== 1) return false;
          // The assignment expression has not completed yet (it suspended inside the pattern).
          if (typeof assigned !== "undefined") return false;

          var r3 = it.next("x");
          return r3.done === true && r3.value === 5 && assigned === rhs;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_destructuring_assignment_rhs_from_yield_then_pattern_yields_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var assigned;
          function* g() {
            var a = 0;
            assigned = ([a = yield 1] = yield 0);
            return a;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;

          var rhs = [];
          var r2 = it.next(rhs);
          if (r2.done !== false || r2.value !== 1) return false;
          // The assignment expression has not completed yet (it suspended inside the pattern).
          if (typeof assigned !== "undefined") return false;

          var r3 = it.next(7);
          return r3.done === true && r3.value === 7 && assigned === rhs;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_rhs_from_yield_resumption_elision_rest_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var assigned;
          var rest;
          function* g() {
            var a = 0;
            assigned = ([, a = 9, ...rest] = yield 0);
            return a === 9 && rest.length === 2 && rest[0] === 3 && rest[1] === 4;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          var rhs = [1, undefined, 3, 4];
          var r2 = it.next(rhs);
          return r2.done === true && r2.value === true && assigned === rhs;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_rhs_from_yield_resumption_rest_default() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var assigned;
          var rest;
          function* g() {
            var a = 0;
            assigned = ({a = 7, ...rest} = yield 0);
            return a === 7 && rest.b === 2 && !Object.prototype.hasOwnProperty.call(rest, "a");
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          var rhs = {b: 2};
          var r2 = it.next(rhs);
          return r2.done === true && r2.value === true && assigned === rhs;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_destructuring_assignment_rhs_and_pattern_yield_are_gc_safe() {
  let mut rt = new_runtime();

  // Allocate an object in Rust and expose it via a global property (not a var binding) so we can
  // delete the last external reference after passing it into the generator.
  let rhs_obj = {
    let (_vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let mut scope = heap.scope();

    let rhs = scope.alloc_object().unwrap();
    scope.push_root(Value::Object(rhs)).unwrap();

    let key_x = scope.alloc_string("x").unwrap();
    scope
      .define_property(rhs, PropertyKey::from_string(key_x), data_desc(Value::Number(5.0)))
      .unwrap();

    let key_rhs = scope.alloc_string("rhsObj").unwrap();
    scope
      .define_property(
        global,
        PropertyKey::from_string(key_rhs),
        data_desc(Value::Object(rhs)),
      )
      .unwrap();

    rhs
  };

  rt
    .exec_script(
      r#"
        var assigned;
        var a = 0;
        var b = 0;
        function* g() {
          assigned = ({x: b, [(yield 1)]: a} = yield 0);
          return a === 5 && b === 5;
        }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();

  let v = rt.exec_script("r1.done === false && r1.value === 0").unwrap();
  assert_eq!(v, Value::Bool(true));

  // Resume with the Rust-allocated RHS object and immediately delete the global property so the
  // object is only kept alive by the generator continuation frames.
  rt
    .exec_script(
      r#"
        var r2 = it.next(globalThis.rhsObj);
        delete globalThis.rhsObj;
      "#,
    )
    .unwrap();

  let v = rt
    .exec_script("r2.done === false && r2.value === 1 && typeof assigned === \"undefined\"")
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  // Force GC while the generator is suspended inside the destructuring pattern.
  rt.heap.collect_garbage();
  assert!(
    rt.heap.is_valid_object(rhs_obj),
    "RHS object should be kept alive by generator continuation frames"
  );

  let v = rt
    .exec_script(
      r#"
        var r3 = it.next("x");
        r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  // Destructuring assignment expressions evaluate to the RHS value; ensure it is the same object.
  let assigned = rt.exec_script("assigned").unwrap();
  assert_eq!(assigned, Value::Object(rhs_obj));
}

#[test]
fn generator_array_destructuring_assignment_rhs_and_pattern_yield_are_gc_safe() {
  let mut rt = new_runtime();

  // Allocate an empty array and expose it via a global property so we can delete the last external
  // reference after passing it into the generator.
  let rhs_arr = {
    let (_vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let intr = realm.intrinsics();
    let mut scope = heap.scope();

    let rhs = scope.alloc_array(0).unwrap();
    // `alloc_array` initialises `[[Prototype]]` to null; install the realm's %Array.prototype%
    // so `GetIterator` works during destructuring.
    scope
      .heap_mut()
      .object_set_prototype(rhs, Some(intr.array_prototype()))
      .unwrap();

    scope.push_root(Value::Object(rhs)).unwrap();
    let key_rhs = scope.alloc_string("rhsArr").unwrap();
    scope
      .define_property(
        global,
        PropertyKey::from_string(key_rhs),
        data_desc(Value::Object(rhs)),
      )
      .unwrap();
    rhs
  };

  rt
    .exec_script(
      r#"
        var assigned;
        function* g() {
          var a = 0;
          assigned = ([a = yield 1] = yield 0);
          return a === 7;
        }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();

  let v = rt.exec_script("r1.done === false && r1.value === 0").unwrap();
  assert_eq!(v, Value::Bool(true));

  rt
    .exec_script(
      r#"
        var r2 = it.next(globalThis.rhsArr);
        delete globalThis.rhsArr;
      "#,
    )
    .unwrap();

  let v = rt
    .exec_script("r2.done === false && r2.value === 1 && typeof assigned === \"undefined\"")
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  // Force GC while the generator is suspended inside the array pattern (default initializer).
  rt.heap.collect_garbage();
  assert!(
    rt.heap.is_valid_object(rhs_arr),
    "RHS array should be kept alive by generator continuation frames"
  );

  let v = rt
    .exec_script(
      r#"
        var r3 = it.next(7);
        r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  let assigned = rt.exec_script("assigned").unwrap();
  assert_eq!(assigned, Value::Object(rhs_arr));
}

#[test]
fn generator_object_destructuring_assignment_rhs_and_computed_key_yield_with_rest_are_gc_safe() {
  let mut rt = new_runtime();

  let rhs_obj = {
    let (_vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let mut scope = heap.scope();

    let rhs = scope.alloc_object().unwrap();
    scope.push_root(Value::Object(rhs)).unwrap();

    let key_x = scope.alloc_string("x").unwrap();
    let key_y = scope.alloc_string("y").unwrap();
    let key_z = scope.alloc_string("z").unwrap();

    scope
      .define_property(rhs, PropertyKey::from_string(key_x), data_desc(Value::Number(1.0)))
      .unwrap();
    scope
      .define_property(rhs, PropertyKey::from_string(key_y), data_desc(Value::Number(2.0)))
      .unwrap();
    scope
      .define_property(rhs, PropertyKey::from_string(key_z), data_desc(Value::Number(3.0)))
      .unwrap();

    let key_rhs = scope.alloc_string("rhsObjRest").unwrap();
    scope
      .define_property(
        global,
        PropertyKey::from_string(key_rhs),
        data_desc(Value::Object(rhs)),
      )
      .unwrap();
    rhs
  };

  rt
    .exec_script(
      r#"
        var assigned;
        var rest;
        function* g() {
          var a = 0;
          var b = 0;
          assigned = ({x: b, [(yield 1)]: a, ...rest} = yield 0);
          return a === 2
            && b === 1
            && rest.z === 3
            && !Object.prototype.hasOwnProperty.call(rest, "x")
            && !Object.prototype.hasOwnProperty.call(rest, "y");
        }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();

  let v = rt.exec_script("r1.done === false && r1.value === 0").unwrap();
  assert_eq!(v, Value::Bool(true));

  rt
    .exec_script(
      r#"
        var r2 = it.next(globalThis.rhsObjRest);
        delete globalThis.rhsObjRest;
      "#,
    )
    .unwrap();

  let v = rt
    .exec_script("r2.done === false && r2.value === 1 && typeof assigned === \"undefined\"")
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  rt.heap.collect_garbage();
  assert!(
    rt.heap.is_valid_object(rhs_obj),
    "RHS object should be kept alive by generator continuation frames"
  );

  let v = rt
    .exec_script(
      r#"
        var r3 = it.next("y");
        r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  let assigned = rt.exec_script("assigned").unwrap();
  assert_eq!(assigned, Value::Object(rhs_obj));
}

#[test]
fn generator_array_destructuring_assignment_rhs_and_default_yield_with_rest_are_gc_safe() {
  let mut rt = new_runtime();

  let rhs_arr = {
    let (_vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let intr = realm.intrinsics();
    let mut scope = heap.scope();

    // Create a sparse array of length 3 where index 0 is a hole (=> undefined) and indices 1/2
    // are present.
    let rhs = scope.alloc_array(3).unwrap();
    scope
      .heap_mut()
      .object_set_prototype(rhs, Some(intr.array_prototype()))
      .unwrap();
    scope.push_root(Value::Object(rhs)).unwrap();

    let key_1 = scope.alloc_array_index_key(1).unwrap();
    let key_2 = scope.alloc_array_index_key(2).unwrap();
    scope
      .define_property(rhs, key_1, data_desc(Value::Number(2.0)))
      .unwrap();
    scope
      .define_property(rhs, key_2, data_desc(Value::Number(3.0)))
      .unwrap();

    let key_rhs = scope.alloc_string("rhsArrRest").unwrap();
    scope
      .define_property(
        global,
        PropertyKey::from_string(key_rhs),
        data_desc(Value::Object(rhs)),
      )
      .unwrap();
    rhs
  };

  rt
    .exec_script(
      r#"
        var assigned;
        var rest;
        function* g() {
          var a = 0;
          assigned = ([a = yield 1, ...rest] = yield 0);
          return a === 7 && rest.length === 2 && rest[0] === 2 && rest[1] === 3;
        }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();

  let v = rt.exec_script("r1.done === false && r1.value === 0").unwrap();
  assert_eq!(v, Value::Bool(true));

  rt
    .exec_script(
      r#"
        var r2 = it.next(globalThis.rhsArrRest);
        delete globalThis.rhsArrRest;
      "#,
    )
    .unwrap();

  let v = rt
    .exec_script("r2.done === false && r2.value === 1 && typeof assigned === \"undefined\"")
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  rt.heap.collect_garbage();
  assert!(
    rt.heap.is_valid_object(rhs_arr),
    "RHS array should be kept alive by generator continuation frames"
  );

  let v = rt
    .exec_script(
      r#"
        var r3 = it.next(7);
        r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  let assigned = rt.exec_script("assigned").unwrap();
  assert_eq!(assigned, Value::Object(rhs_arr));
}

#[test]
fn generator_object_destructuring_assignment_rhs_from_yield_then_rest_target_yields_is_gc_safe() {
  let mut rt = new_runtime();

  // Allocate the RHS object and the holder object in Rust so we can validate GC safety while the
  // generator is suspended inside the rest target.
  let (rhs_obj, holder_obj) = {
    let (_vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let mut scope = heap.scope();

    let rhs = scope.alloc_object().unwrap();
    let holder = scope.alloc_object().unwrap();
    scope
      .push_roots(&[Value::Object(rhs), Value::Object(holder)])
      .unwrap();

    let key_a = scope.alloc_string("a").unwrap();
    let key_b = scope.alloc_string("b").unwrap();
    scope
      .define_property(rhs, PropertyKey::from_string(key_a), data_desc(Value::Number(1.0)))
      .unwrap();
    scope
      .define_property(rhs, PropertyKey::from_string(key_b), data_desc(Value::Number(2.0)))
      .unwrap();

    let rhs_key = scope.alloc_string("rhsRestTarget").unwrap();
    scope
      .define_property(
        global,
        PropertyKey::from_string(rhs_key),
        data_desc(Value::Object(rhs)),
      )
      .unwrap();

    let holder_key = scope.alloc_string("holderObj").unwrap();
    scope
      .define_property(
        global,
        PropertyKey::from_string(holder_key),
        data_desc(Value::Object(holder)),
      )
      .unwrap();

    (rhs, holder)
  };

  rt
    .exec_script(
      r#"
        var assigned;
        function* g() {
          var a = 0;
          assigned = ({a, ...globalThis.holderObj[(yield 1)]} = yield 0);
          return a === 1;
        }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();
  assert_eq!(
    rt.exec_script("r1.done === false && r1.value === 0").unwrap(),
    Value::Bool(true)
  );

  // Resume with the RHS value. The generator should then suspend inside the rest target's computed
  // member key expression (`yield 1`), after the rest object has been created.
  rt
    .exec_script(
      r#"
        var r2 = it.next(globalThis.rhsRestTarget);
        delete globalThis.rhsRestTarget;
        delete globalThis.holderObj;
      "#,
    )
    .unwrap();
  assert_eq!(
    rt.exec_script("r2.done === false && r2.value === 1 && typeof assigned === \"undefined\"")
      .unwrap(),
    Value::Bool(true)
  );

  // Force GC while suspended inside the rest target, with no external references to either object.
  rt.heap.collect_garbage();
  assert!(
    rt.heap.is_valid_object(rhs_obj),
    "RHS object should be kept alive by generator continuation frames"
  );
  assert!(
    rt.heap.is_valid_object(holder_obj),
    "rest target base object should be kept alive by generator continuation frames"
  );

  // Keep the holder alive for postconditions even if `it.next` triggers allocations/GC after
  // the generator completes.
  let holder_root = rt.heap.add_root(Value::Object(holder_obj)).unwrap();

  assert_eq!(
    rt.exec_script("var r3 = it.next(\"k\"); r3.done === true && r3.value === true")
      .unwrap(),
    Value::Bool(true)
  );

  // The assignment expression should evaluate to the original RHS object.
  assert_eq!(rt.exec_script("assigned").unwrap(), Value::Object(rhs_obj));

  // `holder_obj.k` should contain the rest object, which should only have `b`.
  {
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(holder_obj)).unwrap();

    let key_k = scope.alloc_string("k").unwrap();
    let rest_val = scope
      .heap()
      .get(holder_obj, &PropertyKey::from_string(key_k))
      .unwrap();
    let rest_obj = match rest_val {
      Value::Object(o) => o,
      _ => panic!("expected rest target to receive an object"),
    };

    let key_b = scope.alloc_string("b").unwrap();
    assert_eq!(
      scope
        .heap()
        .get(rest_obj, &PropertyKey::from_string(key_b))
        .unwrap(),
      Value::Number(2.0)
    );

    let key_a = scope.alloc_string("a").unwrap();
    assert!(
      scope
        .heap()
        .object_get_own_property(rest_obj, &PropertyKey::from_string(key_a))
        .unwrap()
        .is_none(),
      "rest object should not include excluded property 'a'"
    );
  }

  rt.heap.remove_root(holder_root);
}
