use vm_js::{GcObject, Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_as_utf8(rt: &JsRuntime, value: Value) -> String {
  match value {
    Value::String(s) => rt.heap().get_string(s).unwrap().to_utf8_lossy(),
    Value::Number(n) => n.to_string(),
    Value::Bool(b) => b.to_string(),
    other => panic!("expected primitive result, got {other:?}"),
  }
}

fn assert_script_throws_type_error(rt: &mut JsRuntime, source: &str) -> Result<(), VmError> {
  assert_script_throws_error_named(rt, source, "TypeError")
}

fn assert_script_throws_range_error(rt: &mut JsRuntime, source: &str) -> Result<(), VmError> {
  assert_script_throws_error_named(rt, source, "RangeError")
}

fn assert_script_throws_error_named(
  rt: &mut JsRuntime,
  source: &str,
  expected_name: &str,
) -> Result<(), VmError> {
  let err = match rt.exec_script(source) {
    Ok(v) => panic!("expected script to throw, got {v:?}"),
    Err(err) => err,
  };

  let thrown = match err {
    VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. } => v,
    other => return Err(other),
  };

  let Value::Object(err_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  // Check `e.name` / `e.message` via prototype chain without invoking accessors.
  fn get_string_from_chain(
    rt: &JsRuntime,
    start: GcObject,
    key: &PropertyKey,
  ) -> Result<Option<String>, VmError> {
    let heap = rt.heap();
    let mut current = Some(start);
    let mut steps = 0usize;
    while let Some(obj) = current {
      if steps >= vm_js::MAX_PROTOTYPE_CHAIN {
        return Ok(None);
      }
      steps += 1;
      if let Some(desc) = heap.object_get_own_property(obj, key)? {
        return match desc.kind {
          PropertyKind::Data {
            value: Value::String(s),
            ..
          } => Ok(Some(heap.get_string(s)?.to_utf8_lossy())),
          _ => Ok(None),
        };
      }
      current = heap.object_prototype(obj)?;
    }
    Ok(None)
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;
  let name_key_s = scope.alloc_string("name")?;
  scope.push_root(Value::String(name_key_s))?;
  let name_key = PropertyKey::from_string(name_key_s);
  let message_key_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(message_key_s))?;
  let message_key = PropertyKey::from_string(message_key_s);
  drop(scope);

  let name = get_string_from_chain(rt, err_obj, &name_key)?.unwrap_or_default();
  let message = get_string_from_chain(rt, err_obj, &message_key)?.unwrap_or_default();
  assert_eq!(
    name, expected_name,
    "unexpected error name: {name} ({message})"
  );
  Ok(())
}

#[test]
fn set_difference_get_set_record_validation_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_script_throws_type_error(
    &mut rt,
    r#"
      (() => {
        const s1 = new Set([1, 2]);
        const s2 = { size: 1, has: undefined, keys() { return [].values(); } };
        s1.difference(s2);
      })();
    "#,
  )?;

  assert_script_throws_type_error(
    &mut rt,
    r#"
      (() => {
        const s1 = new Set([1, 2]);
        const s2 = { size: 1, has() {}, keys: undefined };
        s1.difference(s2);
      })();
    "#,
  )?;

  assert_script_throws_type_error(
    &mut rt,
    r#"
      (() => {
        const s1 = new Set([1, 2]);
        const s2 = { size: undefined, has() {}, keys() { return [].values(); } };
        s1.difference(s2);
      })();
    "#,
  )?;
  Ok(())
}

#[test]
fn set_union_get_set_record_validation_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_script_throws_type_error(
    &mut rt,
    r#"
      (() => {
        const s1 = new Set([1, 2]);
        const s2 = { size: 1, has: undefined, keys() { return [].values(); } };
        s1.union(s2);
      })();
    "#,
  )?;

  assert_script_throws_type_error(
    &mut rt,
    r#"
      (() => {
        const s1 = new Set([1, 2]);
        const s2 = { size: 1, has() {}, keys: undefined };
        s1.union(s2);
      })();
    "#,
  )?;

  assert_script_throws_type_error(
    &mut rt,
    r#"
      (() => {
        const s1 = new Set([1, 2]);
        const s2 = { size: undefined, has() {}, keys() { return [].values(); } };
        s1.union(s2);
      })();
    "#,
  )?;
  Ok(())
}

#[test]
fn set_difference_and_union_do_not_call_set_prototype_add() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      const s1 = new Set([1, 2]);
      const s2 = new Set([2, 3]);

      const originalAdd = Set.prototype.add;
      let count = 0;
      Set.prototype.add = function(...rest) {
        count++;
        return originalAdd.apply(this, rest);
      };

      const combined1 = s1.difference(s2);
      const combined2 = s1.union(s2);

      // Restore eagerly so failures don't poison later tests.
      Set.prototype.add = originalAdd;

      String(count) + ":" + [...combined1].join(",") + ":" + [...combined2].join(",");
    "#,
  )?;
  assert_eq!(value_as_utf8(&rt, value), "0:1:1,2,3");
  Ok(())
}

#[test]
fn set_union_does_not_invoke_other_has_and_difference_avoids_other_keys_based_on_size() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      // union: uses other.keys, not other.has
      const s1 = new Set([1, 2]);
      const other1 = [5, 6];
      other1.size = 3;
      other1.has = function () { throw new Error("has called"); };
      other1.keys = function () { return [2, 3, 4].values(); };
      const unionOut = [...s1.union(other1)].join(",");

      // difference: when this.size <= other.size, uses other.has and does not call other.keys()
      const other2 = [5, 6];
      other2.size = 3;
      other2.has = function (v) {
        if (v === 1) return false;
        if (v === 2) return true;
        throw new Error("unexpected has arg: " + v);
      };
      other2.keys = function () { throw new Error("keys called"); };
      const diffOut = [...s1.difference(other2)].join(",");

      unionOut + "|" + diffOut;
    "#,
  )?;
  assert_eq!(value_as_utf8(&rt, value), "1,2,3,4|1");
  Ok(())
}

#[test]
fn get_set_record_reads_size_once_and_rejects_negative_sizes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `GetSetRecord` must only coerce `size` once.
  let value = rt.exec_script(
    r#"
      (() => {
        const s1 = new Set([1, 2]);
        let sizeGets = 0;
        const other = {
          get size() { sizeGets++; return 0; },
          has() { return false; },
          keys() { return [].values(); },
        };
        s1.union(other);
        return sizeGets;
      })();
    "#,
  )?;
  assert_eq!(value_as_utf8(&rt, value), "1");

  // Negative sizes must be rejected with a RangeError.
  assert_script_throws_range_error(
    &mut rt,
    r#"
      (() => {
        const s1 = new Set([1, 2]);
        const other = { size: -1, has() { return false; }, keys() { return [].values(); } };
        s1.difference(other);
      })();
    "#,
  )?;

  Ok(())
}

#[test]
fn set_is_superset_of_and_is_disjoint_from_close_iterators_on_early_exit() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      const iter = {
        a: [4, 5, 6],
        nextCalls: 0,
        returnCalls: 0,
        next() {
          const done = this.nextCalls >= this.a.length;
          const value = this.a[this.nextCalls];
          this.nextCalls++;
          return { done, value };
        },
        return() {
          this.returnCalls++;
          return this;
        }
      };

      const setlike = {
        size: iter.a.length,
        has(v) { return iter.a.includes(v); },
        keys() { return iter; },
      };

      const supersetTrue = new Set([4,5,6,7]).isSupersetOf(setlike);
      const supersetNextTrue = iter.nextCalls;
      const supersetReturnTrue = iter.returnCalls;
      iter.nextCalls = iter.returnCalls = 0;

      const supersetFalse = new Set([0,1,2,3]).isSupersetOf(setlike);
      const supersetNextFalse = iter.nextCalls;
      const supersetReturnFalse = iter.returnCalls;
      iter.nextCalls = iter.returnCalls = 0;

      const disjointFalse = new Set([4,5,6,7]).isDisjointFrom(setlike);
      const disjointNextFalse = iter.nextCalls;
      const disjointReturnFalse = iter.returnCalls;
      iter.nextCalls = iter.returnCalls = 0;

      const disjointTrue = new Set([0,1,2,3]).isDisjointFrom(setlike);
      const disjointNextTrue = iter.nextCalls;
      const disjointReturnTrue = iter.returnCalls;

      [
        supersetTrue, supersetNextTrue, supersetReturnTrue,
        supersetFalse, supersetNextFalse, supersetReturnFalse,
        disjointFalse, disjointNextFalse, disjointReturnFalse,
        disjointTrue, disjointNextTrue, disjointReturnTrue,
      ].join(",");
    "#,
  )?;
  // For the `true` cases, the iterator is exhausted (3 values + 1 done) so `nextCalls` is 4 and
  // `returnCalls` is 0. For the `false` cases, we stop early after 1 `next()` and close the iterator
  // via `return()`.
  assert_eq!(
    value_as_utf8(&rt, value),
    "true,4,0,false,1,1,false,1,1,true,4,0"
  );
  Ok(())
}
