use crate::{
  builtins, Heap, HeapLimits, JsRuntime, Job, PromiseState, PropertyKey, RealmId, Value, Vm, VmError,
  VmHostHooks, VmOptions,
};

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
fn array_concat_roots_args_across_gc_in_is_concat_spreadable_getter() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const receiver = [];
      const spreadable = {
        get [Symbol.isConcatSpreadable]() {
          // Force a GC during IsConcatSpreadable's `Get(O, @@isConcatSpreadable)` step.
          ({});
          return true;
        },
        length: 0
      };
      const tail = "x";
      return [receiver, spreadable, tail];
    })()"#,
  )?;

  let [receiver_val, spreadable_val, tail_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_constructor();
  let args = [spreadable_val, tail_val];

  let mut scope = heap.scope();
  let out = builtins::array_prototype_concat(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    receiver_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected concat() to trigger GC under tiny heap limits"
  );

  scope.push_root(out)?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::InvariantViolation(
      "Array.prototype.concat returned non-object",
    ));
  };

  assert_eq!(scope.heap().array_length(out_obj)?, 1);
  let elem0 = scope
    .heap()
    .array_fast_own_data_element_value(out_obj, 0)?
    .ok_or(VmError::TypeError("missing out[0]"))?;
  assert_eq!(elem0, tail_val);

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

#[test]
fn array_slice_roots_end_across_gc_in_tonumber() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const arr = [0, 1, 2, 3];
      const begin = { valueOf() { ({});
        return 1;
      }};
      const end = { valueOf() { return 4; } };
      return [arr, begin, end];
    })()"#,
  )?;

  let [arr_val, begin_val, end_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_constructor();
  let args = [begin_val, end_val];

  let mut scope = heap.scope();
  let out = builtins::array_prototype_slice(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    arr_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Array.prototype.slice to trigger GC under tiny heap limits"
  );

  // Verify the slice result contents.
  scope.push_root(out)?;
  let Value::Object(out_arr) = out else {
    return Err(VmError::InvariantViolation(
      "Array.prototype.slice returned non-object",
    ));
  };
  assert_eq!(
    scope
      .heap()
      .array_fast_own_data_element_value(out_arr, 0)?
      .ok_or(VmError::InvariantViolation("missing result[0]"))?,
    Value::Number(1.0)
  );
  assert_eq!(
    scope
      .heap()
      .array_fast_own_data_element_value(out_arr, 1)?
      .ok_or(VmError::InvariantViolation("missing result[1]"))?,
    Value::Number(2.0)
  );
  assert_eq!(
    scope
      .heap()
      .array_fast_own_data_element_value(out_arr, 2)?
      .ok_or(VmError::InvariantViolation("missing result[2]"))?,
    Value::Number(3.0)
  );

  Ok(())
}

#[test]
fn array_join_roots_separator_across_gc_in_length_coercion() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const obj = {
        get length() {
          // Force GC during `ToLength(length)` before `separator` is coerced.
          return { valueOf() { ({}); return 2; } };
        },
        0: "a",
        1: "b",
      };
      const sep = { toString() { return "-"; } };
      return [obj, sep, undefined];
    })()"#,
  )?;

  let [obj_val, sep_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_constructor();
  let args = [sep_val];

  let mut scope = heap.scope();
  let out = builtins::array_prototype_join(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    obj_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Array.prototype.join to trigger GC under tiny heap limits"
  );

  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation(
      "Array.prototype.join returned non-string",
    ));
  };
  let out_utf8 = scope.heap().get_string(out_s)?.to_utf8_lossy();
  assert_eq!(out_utf8, "a-b");

  Ok(())
}

#[test]
fn array_index_of_roots_search_across_gc_in_from_index_coercion() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      // Create two distinct string values with the same contents so the search value can be
      // collected without affecting the array element.
      const a = "xy".slice(0, 1);
      const b = "xy".slice(0, 1);
      const arr = [a];
      const fromIndex = { valueOf() { ({}); return 0; } };
      return [arr, b, fromIndex];
    })()"#,
  )?;

  let [arr_val, search_val, from_index_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_constructor();
  let args = [search_val, from_index_val];

  let mut scope = heap.scope();
  let out = builtins::array_prototype_index_of(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    arr_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Array.prototype.indexOf to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(0.0));

  Ok(())
}

#[test]
fn array_push_roots_later_args_across_gc_in_set() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const arr = [];
      const a = { id: 1 };
      const b = { id: 2 };
      return [arr, a, b];
    })()"#,
  )?;

  let [arr_val, a_val, b_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_constructor();
  let args = [a_val, b_val];

  let mut scope = heap.scope();
  let out = builtins::array_prototype_push(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    arr_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Array.prototype.push to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(2.0));

  // Validate array contents (`{id:1}`, `{id:2}`) to ensure arguments survived GC during insertion.
  scope.push_root(arr_val)?;
  let Value::Object(arr_obj) = arr_val else {
    return Err(VmError::TypeError("expected array object"));
  };
  assert_eq!(scope.heap().array_length(arr_obj)?, 2);

  let id_s = scope.alloc_string("id")?;
  scope.push_root(Value::String(id_s))?;
  let id_key = PropertyKey::from_string(id_s);

  let e0 = scope
    .heap()
    .array_fast_own_data_element_value(arr_obj, 0)?
    .ok_or(VmError::InvariantViolation("missing array[0]"))?;
  let e1 = scope
    .heap()
    .array_fast_own_data_element_value(arr_obj, 1)?
    .ok_or(VmError::InvariantViolation("missing array[1]"))?;

  let Value::Object(o0) = e0 else {
    return Err(VmError::InvariantViolation("expected array[0] to be object"));
  };
  let Value::Object(o1) = e1 else {
    return Err(VmError::InvariantViolation("expected array[1] to be object"));
  };

  assert_eq!(
    scope.heap().object_get_own_data_property_value(o0, &id_key)?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    scope.heap().object_get_own_data_property_value(o1, &id_key)?,
    Some(Value::Number(2.0))
  );

  Ok(())
}

#[test]
fn array_pop_roots_result_across_gc_in_length_set_trap() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const target = { length: 1, 0: { id: 1 } };
      const p = new Proxy(target, {
        set(t, prop, value, recv) {
          if (prop === "length") ({});
          return Reflect.set(t, prop, value, recv);
        },
      });
      return [p, undefined, undefined];
    })()"#,
  )?;

  let [proxy_val, _, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_constructor();

  let mut scope = heap.scope();
  let out = builtins::array_prototype_pop(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    proxy_val,
    &[],
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Array.prototype.pop to trigger GC under tiny heap limits"
  );

  // Root the returned value before allocating keys for inspection.
  scope.push_root(out)?;
  let Value::Object(obj) = out else {
    return Err(VmError::InvariantViolation("expected popped value to be object"));
  };

  let id_s = scope.alloc_string("id")?;
  scope.push_root(Value::String(id_s))?;
  let id_key = PropertyKey::from_string(id_s);

  assert_eq!(
    scope.heap().object_get_own_data_property_value(obj, &id_key)?,
    Some(Value::Number(1.0))
  );

  Ok(())
}

#[test]
fn string_slice_roots_end_across_gc_in_tonumber() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const s = "abcd";
      const begin = { valueOf() { ({});
        return 1;
      }};
      const end = { valueOf() { return 4; } };
      return [s, begin, end];
    })()"#,
  )?;

  let [s_val, begin_val, end_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.string_constructor();
  let args = [begin_val, end_val];

  let mut scope = heap.scope();
  let out = builtins::string_prototype_slice(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    s_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected String.prototype.slice to trigger GC under tiny heap limits"
  );

  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation(
      "String.prototype.slice returned non-string",
    ));
  };
  let out_utf8 = scope.heap().get_string(out_s)?.to_utf8_lossy();
  assert_eq!(out_utf8, "bcd");

  Ok(())
}

#[test]
fn string_concat_roots_later_args_across_gc_in_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const thisVal = "a";
      const b = { toString() { ({});
        return "b";
      }};
      const c = { toString() { return "c"; } };
      return [thisVal, b, c];
    })()"#,
  )?;

  let [this_val, b_val, c_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.string_constructor();
  let args = [b_val, c_val];

  let mut scope = heap.scope();
  let out = builtins::string_prototype_concat(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    this_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected String.prototype.concat to trigger GC under tiny heap limits"
  );

  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation(
      "String.prototype.concat returned non-string",
    ));
  };
  let out_utf8 = scope.heap().get_string(out_s)?.to_utf8_lossy();
  assert_eq!(out_utf8, "abc");

  Ok(())
}

#[test]
fn string_index_of_roots_position_across_gc_in_search_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const s = "abcd";
      const search = { toString() { ({});
        return "bc";
      }};
      const position = { valueOf() { return 0; } };
      return [s, search, position];
    })()"#,
  )?;

  let [s_val, search_val, position_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.string_constructor();
  let args = [search_val, position_val];

  let mut scope = heap.scope();
  let out = builtins::string_prototype_index_of(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    s_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected String.prototype.indexOf to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(1.0));

  Ok(())
}

#[test]
fn string_pad_start_roots_fill_across_gc_in_max_length_coercion() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const s = "a";
      const maxLength = { valueOf() { ({});
        return 3;
      }};
      const fill = { toString() { return "x"; } };
      return [s, maxLength, fill];
    })()"#,
  )?;

  let [s_val, max_len_val, fill_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.string_constructor();
  let args = [max_len_val, fill_val];

  let mut scope = heap.scope();
  let out = builtins::string_prototype_pad_start(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    s_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected String.prototype.padStart to trigger GC under tiny heap limits"
  );

  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation(
      "String.prototype.padStart returned non-string",
    ));
  };
  let out_utf8 = scope.heap().get_string(out_s)?.to_utf8_lossy();
  assert_eq!(out_utf8, "xxa");

  Ok(())
}

#[test]
fn string_substring_roots_end_across_gc_in_start_coercion() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const s = "abcd";
      const start = { valueOf() { ({});
        return 1;
      }};
      const end = { valueOf() { return 3; } };
      return [s, start, end];
    })()"#,
  )?;

  let [s_val, start_val, end_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.string_constructor();
  let args = [start_val, end_val];

  let mut scope = heap.scope();
  let out = builtins::string_prototype_substring(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    s_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected String.prototype.substring to trigger GC under tiny heap limits"
  );

  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation(
      "String.prototype.substring returned non-string",
    ));
  };
  let out_utf8 = scope.heap().get_string(out_s)?.to_utf8_lossy();
  assert_eq!(out_utf8, "bc");

  Ok(())
}

#[test]
fn string_locale_compare_roots_arg_across_gc_in_this_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const thisVal = { toString() { ({});
        return "a";
      }};
      const that = "b";
      return [thisVal, that, undefined];
    })()"#,
  )?;

  let [this_val, that_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.string_constructor();
  let args = [that_val];

  let mut scope = heap.scope();
  let out = builtins::string_prototype_locale_compare(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    this_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected String.prototype.localeCompare to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(-1.0));

  Ok(())
}

#[test]
fn object_define_property_roots_desc_across_gc_in_key_to_property_key() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const target = {};
      const prop = { toString() { ({});
        return "x";
      }};
      const desc = { value: 1, writable: true, enumerable: true, configurable: true };
      return [target, prop, desc];
    })()"#,
  )?;

  let [target_val, prop_val, desc_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.object_constructor();
  let args = [target_val, prop_val, desc_val];

  let mut scope = heap.scope();
  let out = builtins::object_define_property(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Undefined,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Object.defineProperty to trigger GC under tiny heap limits"
  );

  // Validate property installation (`target.x === 1`).
  scope.push_root(out)?;
  let Value::Object(target_obj) = out else {
    return Err(VmError::InvariantViolation(
      "Object.defineProperty returned non-object",
    ));
  };

  let x_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(x_s))?;
  let x_key = PropertyKey::from_string(x_s);
  assert_eq!(
    scope
      .heap()
      .object_get_own_data_property_value(target_obj, &x_key)?,
    Some(Value::Number(1.0))
  );

  Ok(())
}

#[test]
fn object_assign_roots_sources_across_gc_in_target_to_object() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const target = true;
      const source = { x: 1 };
      return [target, source, undefined];
    })()"#,
  )?;

  let [target_val, source_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.object_constructor();
  let args = [target_val, source_val];

  let mut scope = heap.scope();
  let out = builtins::object_assign(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Undefined,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Object.assign to trigger GC under tiny heap limits"
  );

  // Validate that the source object survived GC and was copied into the boxed target.
  scope.push_root(out)?;
  let Value::Object(target_obj) = out else {
    return Err(VmError::InvariantViolation("Object.assign returned non-object"));
  };

  let x_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(x_s))?;
  let x_key = PropertyKey::from_string(x_s);
  assert_eq!(
    scope
      .heap()
      .object_get_own_data_property_value(target_obj, &x_key)?,
    Some(Value::Number(1.0))
  );

  Ok(())
}

#[test]
fn reflect_apply_roots_target_across_gc_in_create_list_from_array_like() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const target = function (x) { return x; };
      const thisArg = undefined;
      const args = {
        get length() { ({});
          return 1;
        },
        0: 42,
      };
      return [target, thisArg, args, undefined];
    })()"#,
  )?;

  let [target_val, this_arg_val, args_val, _] = extract_fast_array_elems4(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.object_constructor();
  let args = [target_val, this_arg_val, args_val];

  let mut scope = heap.scope();
  let out = builtins::reflect_apply(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Undefined,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Reflect.apply to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(42.0));

  Ok(())
}

#[test]
fn global_parse_int_roots_radix_across_gc_in_input_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const input = { toString() { ({});
        return "10";
      }};
      const radix = { valueOf() { return 10; } };
      return [input, radix, undefined];
    })()"#,
  )?;

  let [input_val, radix_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.eval(); // any object; global_parse_int ignores callee
  let args = [input_val, radix_val];

  let mut scope = heap.scope();
  let out = builtins::global_parse_int(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Undefined,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected parseInt to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(10.0));

  Ok(())
}

#[test]
fn math_max_roots_later_args_across_gc_in_to_number() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const a = { valueOf() { ({});
        return 1;
      }};
      const b = { valueOf() { return 2; } };
      return [a, b, undefined];
    })()"#,
  )?;

  let [a_val, b_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.eval(); // unused by Math.max
  let args = [a_val, b_val];

  let mut scope = heap.scope();
  let out = builtins::math_max(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Undefined,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Math.max to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(2.0));

  Ok(())
}

#[test]
fn array_constructor_roots_later_args_across_gc_in_root_stack_growth() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const a = { id: 1 };
      const b = { id: 2 };
      return [a, b, undefined];
    })()"#,
  )?;

  let [a_val, b_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.array_constructor();
  let args = [a_val, b_val];

  let mut scope = heap.scope();
  let out = builtins::array_constructor_call(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Undefined,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Array(...) to trigger GC under tiny heap limits"
  );

  // Validate array contents (`{id:1}`, `{id:2}`) to ensure later args survived GC during element
  // definition.
  scope.push_root(out)?;
  let Value::Object(arr_obj) = out else {
    return Err(VmError::InvariantViolation("Array constructor returned non-object"));
  };
  assert_eq!(scope.heap().array_length(arr_obj)?, 2);

  let id_s = scope.alloc_string("id")?;
  scope.push_root(Value::String(id_s))?;
  let id_key = PropertyKey::from_string(id_s);

  let e0 = scope
    .heap()
    .array_fast_own_data_element_value(arr_obj, 0)?
    .ok_or(VmError::InvariantViolation("missing array[0]"))?;
  let e1 = scope
    .heap()
    .array_fast_own_data_element_value(arr_obj, 1)?
    .ok_or(VmError::InvariantViolation("missing array[1]"))?;

  let Value::Object(o0) = e0 else {
    return Err(VmError::InvariantViolation("expected array[0] to be object"));
  };
  let Value::Object(o1) = e1 else {
    return Err(VmError::InvariantViolation("expected array[1] to be object"));
  };

  assert_eq!(
    scope.heap().object_get_own_data_property_value(o0, &id_key)?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    scope.heap().object_get_own_data_property_value(o1, &id_key)?,
    Some(Value::Number(2.0))
  );

  Ok(())
}

#[test]
fn function_bind_roots_bound_args_across_gc_in_length_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const target = new Proxy(function (a, b) { return [a, b]; }, {
        get(t, p, r) {
          if (p === "length") { ({});
            return 2;
          }
          if (p === "name") return "f";
          return Reflect.get(t, p, r);
        }
      });
      const boundThis = undefined;
      const a = { id: 1 };
      const b = { id: 2 };
      return [target, boundThis, a, b];
    })()"#,
  )?;

  let [target_val, bound_this_val, a_val, b_val] = extract_fast_array_elems4(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.function_constructor(); // unused
  let args = [bound_this_val, a_val, b_val];

  let mut scope = heap.scope();
  let bound = builtins::function_prototype_bind(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    target_val,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Function.prototype.bind to trigger GC under tiny heap limits"
  );

  // Call the bound function and ensure it returns the bound args.
  scope.push_root(bound)?;
  let out = vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, bound, Value::Undefined, &[])?;

  scope.push_root(out)?;
  let Value::Object(arr_obj) = out else {
    return Err(VmError::InvariantViolation("expected bound call result to be array object"));
  };

  let e0 = scope
    .heap()
    .array_fast_own_data_element_value(arr_obj, 0)?
    .ok_or(VmError::InvariantViolation("missing result[0]"))?;
  let e1 = scope
    .heap()
    .array_fast_own_data_element_value(arr_obj, 1)?
    .ok_or(VmError::InvariantViolation("missing result[1]"))?;

  // Root the elements before allocating the `"id"` key string.
  scope.push_roots(&[e0, e1])?;

  let id_s = scope.alloc_string("id")?;
  scope.push_root(Value::String(id_s))?;
  let id_key = PropertyKey::from_string(id_s);

  let Value::Object(o0) = e0 else {
    return Err(VmError::InvariantViolation("expected bound result[0] to be object"));
  };
  let Value::Object(o1) = e1 else {
    return Err(VmError::InvariantViolation("expected bound result[1] to be object"));
  };

  assert_eq!(
    scope.heap().object_get_own_data_property_value(o0, &id_key)?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    scope.heap().object_get_own_data_property_value(o1, &id_key)?,
    Some(Value::Number(2.0))
  );

  Ok(())
}

#[test]
fn string_from_char_code_roots_later_args_across_gc_in_first_tonumber() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const a = { valueOf() { ({});
        return 65;
      }};
      const b = { valueOf() { return 66; } };
      return [a, b, undefined];
    })()"#,
  )?;

  let [a_val, b_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let gc_before = heap.gc_runs();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.string_constructor(); // unused
  let args = [a_val, b_val];

  let mut scope = heap.scope();
  let out = builtins::string_from_char_code(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Undefined,
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected String.fromCharCode to trigger GC under tiny heap limits"
  );

  let Value::String(out_s) = out else {
    return Err(VmError::InvariantViolation(
      "String.fromCharCode returned non-string",
    ));
  };
  let out_utf8 = scope.heap().get_string(out_s)?.to_utf8_lossy();
  assert_eq!(out_utf8, "AB");

  Ok(())
}

#[test]
fn json_parse_roots_reviver_across_gc_in_to_string() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const text = { toString() { ({});
        return "1";
      }};
      function reviver(_k, _v) { return 2; }
      return [text, reviver, undefined];
    })()"#,
  )?;

  let [text_val, reviver_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.json();
  let args = [text_val, reviver_val];

  // Keep the argument values alive while preparing the host call, then drop the roots so the
  // builtin is responsible for rooting across any GC it triggers.
  {
    let mut keepalive = heap.scope();
    keepalive.push_roots(&args)?;
  }

  let gc_before = heap.gc_runs();

  let mut scope = heap.scope();
  let out = builtins::json_parse(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(callee),
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected JSON.parse to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(2.0));
  Ok(())
}

#[test]
fn date_utc_roots_later_args_across_gc_in_first_tonumber() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const year = { valueOf() { ({});
        return 1970;
      }};
      const month = { valueOf() { return 0; } };
      const day = { valueOf() { return 1; } };
      return [year, month, day];
    })()"#,
  )?;

  let [year_val, month_val, day_val] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.date_constructor();
  let args = [year_val, month_val, day_val];

  {
    let mut keepalive = heap.scope();
    keepalive.push_roots(&args)?;
  }

  let gc_before = heap.gc_runs();

  let mut scope = heap.scope();
  let out = builtins::date_utc(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(callee),
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Date.UTC to trigger GC under tiny heap limits"
  );
  assert_eq!(out, Value::Number(0.0));
  Ok(())
}

#[test]
fn map_constructor_roots_iterable_across_gc_before_get_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const iterable = [[1, 2]];
      return [iterable, undefined, undefined];
    })()"#,
  )?;

  let [iterable_val, _, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.map();
  let new_target = Value::Object(callee);
  let args = [iterable_val];

  {
    let mut keepalive = heap.scope();
    keepalive.push_roots(&args)?;
  }

  let gc_before = heap.gc_runs();

  let mut scope = heap.scope();
  let out = builtins::map_constructor_construct(
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
    "expected Map constructor to trigger GC under tiny heap limits"
  );

  scope.push_root(out)?;
  let Value::Object(map_obj) = out else {
    return Err(VmError::InvariantViolation("Map constructor returned non-object"));
  };

  assert_eq!(
    scope.heap().map_get_with_tick(map_obj, Value::Number(1.0), || Ok(()))?,
    Some(Value::Number(2.0))
  );
  Ok(())
}

#[test]
fn set_constructor_roots_iterable_across_gc_before_get_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const iterable = [1, 2];
      return [iterable, undefined, undefined];
    })()"#,
  )?;

  let [iterable_val, _, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.set();
  let new_target = Value::Object(callee);
  let args = [iterable_val];

  {
    let mut keepalive = heap.scope();
    keepalive.push_roots(&args)?;
  }

  let gc_before = heap.gc_runs();

  let mut scope = heap.scope();
  let out = builtins::set_constructor_construct(
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
    "expected Set constructor to trigger GC under tiny heap limits"
  );

  scope.push_root(out)?;
  let Value::Object(set_obj) = out else {
    return Err(VmError::InvariantViolation("Set constructor returned non-object"));
  };

  assert_eq!(scope.heap().set_size(set_obj)?, 2);
  assert!(scope
    .heap()
    .set_has_with_tick(set_obj, Value::Number(1.0), || Ok(()))?);
  assert!(scope
    .heap()
    .set_has_with_tick(set_obj, Value::Number(2.0), || Ok(()))?);
  Ok(())
}

#[test]
fn weak_map_constructor_roots_iterable_across_gc_before_get_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const key = { id: 1 };
      const iterable = [[key, 2]];
      return [iterable, key, undefined];
    })()"#,
  )?;

  let [iterable_val, key_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.weak_map();
  let args = [iterable_val];

  {
    let mut keepalive = heap.scope();
    keepalive.push_roots(&[iterable_val, key_val])?;
  }

  let gc_before = heap.gc_runs();

  let mut scope = heap.scope();
  let out = builtins::weak_map_constructor_construct(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    &args,
    Value::Object(callee),
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected WeakMap constructor to trigger GC under tiny heap limits"
  );

  scope.push_roots(&[out, key_val])?;
  let Value::Object(map_obj) = out else {
    return Err(VmError::InvariantViolation("WeakMap constructor returned non-object"));
  };

  assert_eq!(
    scope.heap().weak_map_get(map_obj, key_val)?,
    Some(Value::Number(2.0))
  );
  Ok(())
}

#[test]
fn weak_set_constructor_roots_iterable_across_gc_before_get_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const key = { id: 1 };
      const iterable = [key];
      return [iterable, key, undefined];
    })()"#,
  )?;

  let [iterable_val, key_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.weak_set();
  let args = [iterable_val];

  {
    let mut keepalive = heap.scope();
    keepalive.push_roots(&[iterable_val, key_val])?;
  }

  let gc_before = heap.gc_runs();

  let mut scope = heap.scope();
  let out = builtins::weak_set_constructor_construct(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    &args,
    Value::Object(callee),
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected WeakSet constructor to trigger GC under tiny heap limits"
  );

  scope.push_roots(&[out, key_val])?;
  let Value::Object(set_obj) = out else {
    return Err(VmError::InvariantViolation("WeakSet constructor returned non-object"));
  };

  assert!(scope.heap().weak_set_has(set_obj, key_val)?);
  Ok(())
}

#[test]
fn promise_reject_roots_reason_across_gc_in_new_promise_capability() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const reason = { id: 123 };
      return [reason, undefined, undefined];
    })()"#,
  )?;

  let [reason_val, _, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.promise();
  let args = [reason_val];

  {
    let mut keepalive = heap.scope();
    keepalive.push_roots(&args)?;
  }

  let gc_before = heap.gc_runs();

  let mut scope = heap.scope();
  let out = builtins::promise_reject(
    vm,
    &mut scope,
    &mut host,
    &mut hooks,
    callee,
    Value::Object(callee),
    &args,
  )?;

  assert!(
    scope.heap().gc_runs() > gc_before,
    "expected Promise.reject to trigger GC under tiny heap limits"
  );

  scope.push_root(out)?;
  let Value::Object(promise_obj) = out else {
    return Err(VmError::InvariantViolation("Promise.reject returned non-object"));
  };
  assert_eq!(scope.heap().promise_state(promise_obj)?, PromiseState::Rejected);

  let reason = scope
    .heap()
    .promise_result(promise_obj)?
    .ok_or(VmError::InvariantViolation(
      "missing Promise.reject promise result",
    ))?;
  scope.push_root(reason)?;

  // Root the reason object before allocating the `"id"` key string.
  let id_s = scope.alloc_string("id")?;
  scope.push_root(Value::String(id_s))?;
  let id_key = PropertyKey::from_string(id_s);

  let Value::Object(reason_obj) = reason else {
    return Err(VmError::InvariantViolation(
      "expected Promise.reject reason to be object",
    ));
  };
  assert_eq!(
    scope.heap().object_get_own_data_property_value(reason_obj, &id_key)?,
    Some(Value::Number(123.0))
  );
  Ok(())
}

#[test]
fn promise_constructor_roots_executor_across_gc_in_ordinary_create() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let args_array = rt.exec_script(
    r#"(() => {
      const marker = { called: false };
      function executor(resolve, _reject) { marker.called = true; resolve(1); }
      return [executor, marker, undefined];
    })()"#,
  )?;

  let [executor_val, marker_val, _] = extract_fast_array_elems3(&mut rt, args_array)?;

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut host = ();
  let mut hooks = NoopHostHooks::default();

  let intr = vm.intrinsics().expect("intrinsics initialized");
  let callee = intr.promise();
  let new_target = Value::Object(callee);
  let args = [executor_val];

  {
    let mut keepalive = heap.scope();
    keepalive.push_roots(&[executor_val, marker_val])?;
  }

  let gc_before = heap.gc_runs();

  let mut scope = heap.scope();
  let out = builtins::promise_constructor_construct(
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
    "expected Promise constructor to trigger GC under tiny heap limits"
  );

  // Root the outputs before allocating the `"called"` key string.
  scope.push_roots(&[out, marker_val])?;

  let called_s = scope.alloc_string("called")?;
  scope.push_root(Value::String(called_s))?;
  let called_key = PropertyKey::from_string(called_s);

  let Value::Object(marker_obj) = marker_val else {
    return Err(VmError::InvariantViolation("expected marker to be object"));
  };
  assert_eq!(
    scope.heap().object_get_own_data_property_value(marker_obj, &called_key)?,
    Some(Value::Bool(true))
  );
  Ok(())
}
