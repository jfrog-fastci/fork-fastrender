use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn new_gc_stress_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Use a small heap and a conservative GC threshold so generator resume paths hit
  // allocation-triggered GC reliably, without forcing a GC on *every* allocation (which makes the
  // test extremely slow).
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 512 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_compound_assignment_property_captures_old_value_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o = { a: 1 };
      function* g(){ return o.a += (yield 0); }
      var it = g();
      it.next();
      o.a = 100;
      var r = it.next(5);
      // Must use the pre-yield old value (1), not the mutated value (100).
      r.done === true && r.value === 6 && o.a === 6
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_property_captures_base_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o1 = { a: 1 };
      var o2 = { a: 10 };
      var o = o1;
      function* g(){ return o.a += (yield 0); }
      var it = g();
      it.next();
      o = o2;
      var r = it.next(5);
      r.done === true && r.value === 6 && o1.a === 6 && o2.a === 10
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_property_captures_computed_key_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o = { a: 1, b: 10 };
      var k = 'a';
      function* g(){ return o[k] += (yield 0); }
      var it = g();
      it.next();
      k = 'b';
      var r = it.next(5);
      r.done === true && r.value === 6 && o.a === 6 && o.b === 10
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_property_captures_base_and_computed_key_before_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o1 = { a: 1, b: 10 };
      var o2 = { a: 100, b: 1000 };
      var o = o1;
      var k = 'a';
      function* g(){ return o[k] += (yield 0); }
      var it = g();
      it.next();
      // Rebind both base and key after the yield but before resuming.
      o = o2;
      k = 'b';
      var r = it.next(2);
      r.done === true && r.value === 3 &&
      // Must still target the original base/key pair.
      o1.a === 3 && o1.b === 10 &&
      o2.a === 100 && o2.b === 1000
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_compound_assignment_evaluates_base_key_and_old_value_once_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var baseCount = 0;
      var keyCount = 0;
      var getCount = 0;
      var setCount = 0;
      var stored = 0;

      var o1 = {
        get a() { getCount++; return 1; },
        set a(v) { setCount++; stored = v; },
      };

      var o2 = { a: 100 };
      var k2 = "a";

      function getO() { baseCount++; return o1; }
      function getK() { keyCount++; return "a"; }

      function* g() { getO()[getK()] += (yield 0); return stored; }
      var it = g();
      var r1 = it.next();

      // By the time the generator yields, base + key are evaluated and the old value is read.
      var ok1 =
        r1.value === 0 && r1.done === false &&
        baseCount === 1 && keyCount === 1 &&
        getCount === 1 && setCount === 0;

      // Change the base/key producers after yielding; the assignment must not re-evaluate them.
      getO = function () { baseCount++; return o2; };
      getK = function () { keyCount++; return k2; };

      var r2 = it.next(2);
      var ok2 =
        r2.value === 3 && r2.done === true &&
        stored === 3 &&
        // No re-evaluation after resumption.
        baseCount === 1 && keyCount === 1 &&
        // Old value getter was called once (before the yield), setter once (after resume).
        getCount === 1 && setCount === 1 &&
        // And nothing was written to the rebound object.
        o2.a === 100;

      ok1 && ok2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_assignment_property_keeps_captured_base_alive_across_gc_on_resume() {
  let mut rt = new_gc_stress_runtime();
  let value = rt
    .exec_script(
      r#"
      function makeGarbage() {
        // Allocate enough ephemeral objects to force GC while resuming from `yield`.
        //
        // Prefer fewer, larger allocations so we trigger GC without growing the heap's slot table
        // to a huge size (which makes the test very slow).
        for (let i = 0; i < 16; i++) {
          new ArrayBuffer(64 * 1024);
        }
      }

      function* g() {
        // The LHS base object is only reachable from the generator continuation frame when the RHS
        // yields.
        return ({}).a = (yield 1, makeGarbage(), 42);
      }

      const it = g();
      it.next();
      const r = it.next(0);
      r.done === true && r.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
