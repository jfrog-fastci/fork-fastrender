use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force frequent GC cycles so generator resumption must correctly root any values held only in
  // continuation frames while they are temporarily stored outside the heap.
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 0));
  JsRuntime::new(vm, heap).unwrap()
}

// Use enough arguments that `gen_root_values_for_continuation` pushes a large root list. The first
// `next()` (SuspendedStart) grows `root_stack` to ~63; the second `next()` (SuspendedYield) adds
// extra roots (resume value + continuation frame values), forcing another growth + GC while the
// continuation is taken out of the heap.
const ARG_COUNT: usize = 60;

fn init_args_script() -> String {
  format!(
    r#"
      var args = new Array({ARG_COUNT});
      for (var i = 0; i < args.length; i++) args[i] = i;
    "#
  )
}

#[test]
fn generator_resume_roots_new_args_frame_values() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return new (class {{ constructor(x) {{ this.x = x; }} }})(yield 1).x;
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(42);
        r1.value === 1 && r1.done === false &&
        r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_exponentiation_assignment_frame_values() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return ({{ x: 2 }}).x **= (yield 1);
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(3);
        r1.value === 1 && r1.done === false &&
        r2.value === 8 && r2.done === true
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_subtraction_assignment_frame_values() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return ({{ x: 10 }}).x -= (yield 1);
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(3);
        r1.value === 1 && r1.done === false &&
        r2.value === 7 && r2.done === true
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_logical_or_assignment_frame_values() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return ({{ x: 0 }}).x ||= (yield 1);
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(42);
        r1.value === 1 && r1.done === false &&
        r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_logical_and_assignment_frame_values() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return ({{ x: 1 }}).x &&= (yield 1);
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(42);
        r1.value === 1 && r1.done === false &&
        r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_nullish_coalescing_assignment_frame_values() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return ({{ x: null }}).x ??= (yield 1);
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(42);
        r1.value === 1 && r1.done === false &&
        r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_bitwise_left_shift_assignment_frame_values() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return ({{ x: 1 }}).x <<= (yield 1);
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(2);
        r1.value === 1 && r1.done === false &&
        r2.value === 4 && r2.done === true
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_computed_member_assignment_base_before_key_yield() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return ({{}})[(yield 1)] = 2;
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next("x");
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === true
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_array_literal_after_single_element_yield() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return [ (yield 1), 2 ];
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(42);
        r1.value === 1 && r1.done === false &&
        r2.done === true &&
        Array.isArray(r2.value) &&
        r2.value.length === 2 &&
        r2.value[0] === 42 &&
        r2.value[1] === 2
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}

#[test]
fn generator_resume_roots_array_literal_after_spread_element_yield() {
  let mut rt = new_runtime_gc();
  rt
    .exec_script(&format!(
      r#"
        function* g() {{
          return [ ...(yield 1), 2 ];
        }}
        {init_args}
        var it = g.apply(null, args);
        var r1 = it.next();
      "#,
      init_args = init_args_script(),
    ))
    .unwrap();

  // Reuse the preallocated `args` array as the resume value so we don't trigger a GC cycle before
  // the generator resumption code has taken the continuation out of the heap.
  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(args);
        r1.value === 1 && r1.done === false &&
        r2.done === true &&
        Array.isArray(r2.value) &&
        r2.value.length === (args.length + 1) &&
        r2.value[0] === 0 &&
        r2.value[args.length - 1] === (args.length - 1) &&
        r2.value[args.length] === 2
      "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator resumption to trigger at least one GC cycle"
  );
}
