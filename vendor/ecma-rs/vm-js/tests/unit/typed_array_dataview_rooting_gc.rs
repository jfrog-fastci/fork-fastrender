use crate::{builtins, Heap, HeapLimits, JsRuntime, Job, RealmId, Value, Vm, VmError, VmHostHooks, VmOptions};

#[derive(Default)]
struct NoopHostHooks;

impl VmHostHooks for NoopHostHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
    // Unit tests do not run the microtask queue.
  }
}

fn new_runtime_with_tiny_gc() -> Result<JsRuntime, VmError> {
  // Keep the heap small enough that allocations inside `valueOf` reliably trigger GC, but large
  // enough for full realm + intrinsics initialization.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1));
  JsRuntime::new(vm, heap)
}

fn extract_fast_array_elems3(
  rt: &mut JsRuntime,
  array_val: Value,
) -> Result<[Value; 3], VmError> {
  let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut scope = heap.scope();

  // Root the array object while we read its fast elements.
  scope.push_root(array_val)?;
  let Value::Object(array_obj) = array_val else {
    return Err(VmError::TypeError("expected array object"));
  };

  let a = scope
    .heap()
    .array_fast_own_data_element_value(array_obj, 0)?
    .ok_or(VmError::TypeError("missing array[0]"))?;
  let b = scope
    .heap()
    .array_fast_own_data_element_value(array_obj, 1)?
    .ok_or(VmError::TypeError("missing array[1]"))?;
  let c = scope
    .heap()
    .array_fast_own_data_element_value(array_obj, 2)?
    .ok_or(VmError::TypeError("missing array[2]"))?;

  Ok([a, b, c])
}

fn extract_fast_array_elems4(
  rt: &mut JsRuntime,
  array_val: Value,
) -> Result<[Value; 4], VmError> {
  let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut scope = heap.scope();

  // Root the array object while we read its fast elements.
  scope.push_root(array_val)?;
  let Value::Object(array_obj) = array_val else {
    return Err(VmError::TypeError("expected array object"));
  };

  let a = scope
    .heap()
    .array_fast_own_data_element_value(array_obj, 0)?
    .ok_or(VmError::TypeError("missing array[0]"))?;
  let b = scope
    .heap()
    .array_fast_own_data_element_value(array_obj, 1)?
    .ok_or(VmError::TypeError("missing array[1]"))?;
  let c = scope
    .heap()
    .array_fast_own_data_element_value(array_obj, 2)?
    .ok_or(VmError::TypeError("missing array[2]"))?;
  let d = scope
    .heap()
    .array_fast_own_data_element_value(array_obj, 3)?
    .ok_or(VmError::TypeError("missing array[3]"))?;

  Ok([a, b, c, d])
}

#[test]
fn array_buffer_ctor_roots_options_across_gc_in_toindex() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const length = { valueOf() { ({});
        return 8;
      }};
      const options = { get maxByteLength() { return 16; } };
      return [length, options, undefined];
    })()"#,
  )?;

  let [length_val, options_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_buffer();
  let new_target = Value::Object(callee);
  let args = [length_val, options_val];

  let mut scope = heap.scope();
  let out = builtins::array_buffer_constructor_construct(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    &args,
    new_target,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected constructor to trigger GC under tiny heap limits"
  );

  let Value::Object(buf_obj) = out else {
    return Err(VmError::InvariantViolation(
      "ArrayBuffer constructor returned non-object",
    ));
  };

  let byte_length = builtins::array_buffer_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(buf_obj),
    &[],
  )?;
  assert_eq!(byte_length, Value::Number(8.0));

  let max_byte_length = builtins::array_buffer_prototype_max_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(buf_obj),
    &[],
  )?;
  assert_eq!(max_byte_length, Value::Number(16.0));

  Ok(())
}

#[test]
fn array_buffer_slice_roots_this_and_end_across_gc_in_tonumber() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const buf = new ArrayBuffer(8);
      const begin = { valueOf() { ({});
        return 0;
      }};
      const end = { valueOf() { return 8; } };
      return [buf, begin, end];
    })()"#,
  )?;

  let [buf_val, begin_val, end_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_buffer();
  let args = [begin_val, end_val];

  let mut scope = heap.scope();
  let out = builtins::array_buffer_prototype_slice(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    buf_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected slice to trigger GC under tiny heap limits"
  );

  let Value::Object(slice_obj) = out else {
    return Err(VmError::InvariantViolation(
      "ArrayBuffer.prototype.slice returned non-object",
    ));
  };

  let byte_length = builtins::array_buffer_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(slice_obj),
    &[],
  )?;
  assert_eq!(byte_length, Value::Number(8.0));

  Ok(())
}

#[test]
fn array_buffer_transfer_to_immutable_roots_this_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const buf = new ArrayBuffer(8);
      return [buf, undefined, undefined];
    })()"#,
  )?;

  let [buf_val, _, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_buffer();

  let mut scope = heap.scope();
  let out = builtins::array_buffer_prototype_transfer_to_immutable(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    buf_val,
    &[],
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected transferToImmutable() to trigger GC under tiny heap limits"
  );

  let Value::Object(dst_obj) = out else {
    return Err(VmError::InvariantViolation(
      "ArrayBuffer.prototype.transferToImmutable returned non-object",
    ));
  };

  let dst_len = builtins::array_buffer_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(dst_obj),
    &[],
  )?;
  assert_eq!(dst_len, Value::Number(8.0));

  // Source buffer must be detached.
  let src_len = builtins::array_buffer_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    buf_val,
    &[],
  )?;
  assert_eq!(src_len, Value::Number(0.0));

  let detached = builtins::array_buffer_prototype_detached_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    buf_val,
    &[],
  )?;
  assert_eq!(detached, Value::Bool(true));

  // Immutable buffers are not resizable and have maxByteLength == byteLength.
  let resizable = builtins::array_buffer_prototype_resizable_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(dst_obj),
    &[],
  )?;
  assert_eq!(resizable, Value::Bool(false));

  let max_len = builtins::array_buffer_prototype_max_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(dst_obj),
    &[],
  )?;
  assert_eq!(max_len, Value::Number(8.0));

  Ok(())
}

#[test]
fn typed_array_set_roots_inputs_across_gc_in_offset_coercion() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const target = new Uint8Array(4);
      const source = { length: 3, 0: 1, 1: 2, 2: 3 };
      const offset = { valueOf() { ({});
        return 0;
      }};
      return [target, source, offset];
    })()"#,
  )?;

  let [target_val, source_val, offset_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.uint8_array();
  let args = [source_val, offset_val];

  let mut scope = heap.scope();
  let out = builtins::typed_array_prototype_set(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    target_val,
    &args,
  )?;
  assert_eq!(out, Value::Undefined);

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected set() to trigger GC under tiny heap limits"
  );

  let Value::Object(target_obj) = target_val else {
    return Err(VmError::InvariantViolation(
      "Uint8Array target value is not an object",
    ));
  };

  assert_eq!(
    scope.heap().typed_array_get_element_value(target_obj, 0)?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    scope.heap().typed_array_get_element_value(target_obj, 1)?,
    Some(Value::Number(2.0))
  );
  assert_eq!(
    scope.heap().typed_array_get_element_value(target_obj, 2)?,
    Some(Value::Number(3.0))
  );
  assert_eq!(
    scope.heap().typed_array_get_element_value(target_obj, 3)?,
    Some(Value::Number(0.0))
  );

  Ok(())
}

#[test]
fn typed_array_ctor_roots_array_buffer_across_gc_in_toindex() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  // Construct argument objects in JS so `ToNumber` coercion can invoke user code.
  // The `valueOf` allocates, forcing GC under the tiny heap threshold.
  let args_array = rt.exec_script(
    r#"(() => {
      const buf = new ArrayBuffer(8);
      const byteOffset = { valueOf() { ({});
        return 0;
      }};
      const length = { valueOf() { return 4; } };
      return [buf, byteOffset, length];
    })()"#,
  )?;

  let [buf_val, byte_offset_val, length_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  // Call the builtin constructor directly (host context) without rooting args, to ensure the
  // builtin itself roots the ArrayBuffer across `ToIndex(byteOffset)` coercion + GC.
  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.uint8_array();
  let new_target = Value::Object(callee);
  let args = [buf_val, byte_offset_val, length_val];

  let mut scope = heap.scope();
  let out = builtins::uint8_array_constructor_construct(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    &args,
    new_target,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected constructor to trigger GC under tiny heap limits"
  );

  let Value::Object(view_obj) = out else {
    return Err(VmError::InvariantViolation(
      "Uint8Array constructor returned non-object",
    ));
  };

  // Validate `%TypedArray%.prototype` accessors.
  let byte_length = builtins::typed_array_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(view_obj),
    &[],
  )?;
  assert_eq!(byte_length, Value::Number(4.0));

  let byte_offset = builtins::typed_array_prototype_byte_offset_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(view_obj),
    &[],
  )?;
  assert_eq!(byte_offset, Value::Number(0.0));

  let buffer = builtins::typed_array_prototype_buffer_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(view_obj),
    &[],
  )?;
  let Value::Object(buffer_obj) = buffer else {
    return Err(VmError::InvariantViolation(
      "TypedArray.buffer getter returned non-object",
    ));
  };

  // `buffer.byteLength` should reflect the original backing buffer size.
  let buffer_byte_length = builtins::array_buffer_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(buffer_obj),
    &[],
  )?;
  assert_eq!(buffer_byte_length, Value::Number(8.0));

  Ok(())
}

#[test]
fn typed_array_ctor_iterable_roots_iterator_across_gc_in_push_roots() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const iterable = {
        get [Symbol.iterator]() {
          // Force GC during GetMethod([Symbol.iterator]).
          ({});
          return function () {
            // Also allocate during iterator creation to keep GC pressure high.
            ({});
            let i = 0;
            return {
              next() {
                i++;
                if (i <= 2) return { value: i, done: false };
                return { value: undefined, done: true };
              }
            };
          };
        }
      };
      return [iterable, undefined, undefined];
    })()"#,
  )?;

  let [iterable_val, arg1, arg2] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.uint8_array();
  let new_target = Value::Object(callee);
  let args = [iterable_val, arg1, arg2];

  let mut scope = heap.scope();
  let out = builtins::uint8_array_constructor_construct(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    &args,
    new_target,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected constructor to trigger GC under tiny heap limits"
  );

  let Value::Object(view_obj) = out else {
    return Err(VmError::InvariantViolation(
      "Uint8Array constructor returned non-object",
    ));
  };

  assert_eq!(scope.heap().typed_array_length(view_obj)?, 2);
  assert_eq!(
    scope.heap().typed_array_get_element_value(view_obj, 0)?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    scope.heap().typed_array_get_element_value(view_obj, 1)?,
    Some(Value::Number(2.0))
  );

  Ok(())
}

#[test]
fn data_view_ctor_roots_byte_length_across_gc_in_toindex() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const buf = new ArrayBuffer(8);
      const byteOffset = { valueOf() { ({});
        return 0;
      }};
      const byteLength = { valueOf() { return 4; } };
      return [buf, byteOffset, byteLength];
    })()"#,
  )?;

  let [buf_val, byte_offset_val, byte_length_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.data_view();
  let new_target = Value::Object(callee);
  let args = [buf_val, byte_offset_val, byte_length_val];

  let mut scope = heap.scope();
  let out = builtins::data_view_constructor_construct(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    &args,
    new_target,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected constructor to trigger GC under tiny heap limits"
  );

  let Value::Object(view_obj) = out else {
    return Err(VmError::InvariantViolation(
      "DataView constructor returned non-object",
    ));
  };

  let byte_length = builtins::data_view_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(view_obj),
    &[],
  )?;
  assert_eq!(byte_length, Value::Number(4.0));

  let byte_offset = builtins::data_view_prototype_byte_offset_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(view_obj),
    &[],
  )?;
  assert_eq!(byte_offset, Value::Number(0.0));

  let buffer = builtins::data_view_prototype_buffer_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(view_obj),
    &[],
  )?;
  let Value::Object(buffer_obj) = buffer else {
    return Err(VmError::InvariantViolation(
      "DataView.buffer getter returned non-object",
    ));
  };

  let buffer_byte_length = builtins::array_buffer_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(buffer_obj),
    &[],
  )?;
  assert_eq!(buffer_byte_length, Value::Number(8.0));

  Ok(())
}

#[test]
fn typed_array_subarray_roots_end_across_gc_in_tonumber() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const ta = new Uint8Array([1, 2, 3, 4]);
      const begin = { valueOf() { ({});
        return 1;
      }};
      const end = { valueOf() { return 3; } };
      return [ta, begin, end];
    })()"#,
  )?;

  let [ta_val, begin_val, end_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.uint8_array();
  let args = [begin_val, end_val];

  let mut scope = heap.scope();
  let out = builtins::typed_array_prototype_subarray(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    ta_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected subarray() to trigger GC under tiny heap limits"
  );

  let Value::Object(view_obj) = out else {
    return Err(VmError::InvariantViolation(
      "TypedArray.prototype.subarray returned non-object",
    ));
  };

  assert_eq!(scope.heap().typed_array_length(view_obj)?, 2);
  assert_eq!(
    scope.heap().typed_array_get_element_value(view_obj, 0)?,
    Some(Value::Number(2.0))
  );
  assert_eq!(
    scope.heap().typed_array_get_element_value(view_obj, 1)?,
    Some(Value::Number(3.0))
  );

  Ok(())
}

#[test]
fn array_buffer_resize_roots_new_length_across_gc_before_coercion() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const buf = new ArrayBuffer(8, { maxByteLength: 16 });
      const newLength = { valueOf() { ({});
        return 12;
      }};
      return [buf, newLength, undefined];
    })()"#,
  )?;

  let [buf_val, new_len_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  // Force the heap to consider itself "close to the limit" so dropping a large root stack will
  // shrink capacity back to 0, ensuring `push_root` inside the builtin triggers GC.
  let max = heap.limits().max_bytes;
  let min_for_root_stack_shrink = max.saturating_mul(3) / 4 + 1;
  let extra_token = {
    let mut scope = heap.scope();
    // Root the target values while we manipulate the root stack / heap counters.
    scope.push_roots(&[buf_val, new_len_val])?;

    let token = {
      let cur = scope.heap().estimated_total_bytes();
      if cur < min_for_root_stack_shrink {
        Some(scope.heap_mut().charge_external(min_for_root_stack_shrink - cur)?)
      } else {
        None
      }
    };

    // Grow the root stack above the shrink threshold (>256) and then drop the scope.
    let mut junk: Vec<Value> = Vec::new();
    junk.try_reserve_exact(300).map_err(|_| VmError::OutOfMemory)?;
    for i in 0..300 {
      junk.push(Value::Number(i as f64));
    }
    scope.push_roots(&junk)?;
    // Return the external charge token so it remains live until *after* scope drop.
    token
  };
  drop(extra_token);

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_buffer();
  let args = [new_len_val];

  let mut scope = heap.scope();
  let out = builtins::array_buffer_prototype_resize(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    buf_val,
    &args,
  )?;
  assert_eq!(out, Value::Undefined);

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected resize() to trigger GC under tiny heap limits"
  );

  let Value::Object(buf_obj) = buf_val else {
    return Err(VmError::InvariantViolation(
      "ArrayBuffer receiver value is not an object",
    ));
  };

  let byte_length = builtins::array_buffer_prototype_byte_length_get(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(buf_obj),
    &[],
  )?;
  assert_eq!(byte_length, Value::Number(12.0));

  Ok(())
}

#[test]
fn data_view_get_roots_optional_little_endian_across_gc_in_toindex() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const buf = new ArrayBuffer(2);
      const u8 = new Uint8Array(buf);
      u8[0] = 0x34;
      u8[1] = 0x12;
      const view = new DataView(buf);
      const offset = { valueOf() { ({});
        return 0;
      }};
      // Use a String so `ToBoolean(littleEndian)` must dereference a `GcString` handle (and will
      // crash on stale handles if it was GC'd during offset coercion).
      const littleEndian = "x";
      return [view, offset, littleEndian];
    })()"#,
  )?;

  let [view_val, offset_val, little_endian_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.data_view();
  let args = [offset_val, little_endian_val];

  let mut scope = heap.scope();
  let out = builtins::data_view_prototype_get_uint16(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    view_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected getUint16() to trigger GC under tiny heap limits"
  );

  assert_eq!(out, Value::Number(0x1234 as f64));
  Ok(())
}

#[test]
fn data_view_set_roots_value_and_little_endian_across_gc_in_toindex() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const buf = new ArrayBuffer(2);
      const view = new DataView(buf);
      const offset = { valueOf() { ({});
        return 0;
      }};
      // Use a String so `ToNumber(value)` must dereference a `GcString` handle.
      const value = "4660"; // 0x1234
      // Use a String so `ToBoolean(littleEndian)` must dereference a `GcString` handle.
      const littleEndian = "x";
      return [view, offset, value, littleEndian];
    })()"#,
  )?;

  let [view_val, offset_val, value_val, little_endian_val] =
    extract_fast_array_elems4(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.data_view();
  let args = [offset_val, value_val, little_endian_val];

  let mut scope = heap.scope();
  let out = builtins::data_view_prototype_set_uint16(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    view_val,
    &args,
  )?;
  assert_eq!(out, Value::Undefined);

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected setUint16() to trigger GC under tiny heap limits"
  );

  let Value::Object(view_obj) = view_val else {
    return Err(VmError::InvariantViolation("DataView receiver is not an object"));
  };
  let buffer = scope.heap().data_view_buffer(view_obj)?;
  scope.push_root(Value::Object(buffer))?;
  let data = scope.heap().array_buffer_data(buffer)?;
  assert_eq!(data.get(0), Some(&0x34));
  assert_eq!(data.get(1), Some(&0x12));

  Ok(())
}

#[test]
fn typed_array_slice_roots_end_across_gc_in_tonumber() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const ta = new Uint8Array([1, 2, 3, 4]);
      const begin = { valueOf() { ({});
        return 1;
      }};
      const end = { valueOf() { return 3; } };
      return [ta, begin, end];
    })()"#,
  )?;

  let [ta_val, begin_val, end_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.uint8_array();
  let args = [begin_val, end_val];

  let mut scope = heap.scope();
  let out = builtins::typed_array_prototype_slice(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    ta_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected slice() to trigger GC under tiny heap limits"
  );

  let Value::Object(slice_obj) = out else {
    return Err(VmError::InvariantViolation(
      "TypedArray.prototype.slice returned non-object",
    ));
  };

  assert_eq!(
    scope.heap().typed_array_get_element_value(slice_obj, 0)?,
    Some(Value::Number(2.0))
  );
  assert_eq!(
    scope.heap().typed_array_get_element_value(slice_obj, 1)?,
    Some(Value::Number(3.0))
  );
  assert_eq!(scope.heap().typed_array_get_element_value(slice_obj, 2)?, None);

  Ok(())
}
