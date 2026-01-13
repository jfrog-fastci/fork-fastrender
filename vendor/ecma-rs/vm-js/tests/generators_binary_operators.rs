use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generators_bitwise_and_shift_operators_allow_yield_in_operands() {
  let mut rt = new_runtime();
  let ok = rt
    .exec_script(
      r#"
        (() => {
          // Bitwise OR.
          function* g_or() { return (yield 1) | 2; }
          var it_or = g_or();
          var r1 = it_or.next();
          var r2 = it_or.next(5);
          if (!(r1.value === 1 && r1.done === false)) return false;
          if (!(r2.value === 7 && r2.done === true)) return false;

          // Shift left.
          function* g_shl() { return (yield 1) << 1; }
          var it_shl = g_shl();
          var r3 = it_shl.next();
          var r4 = it_shl.next(3);
          if (!(r3.value === 1 && r3.done === false)) return false;
          if (!(r4.value === 6 && r4.done === true)) return false;

          // Unsigned right shift.
          function* g_ushr() { return (yield 8) >>> 1; }
          var it_ushr = g_ushr();
          var r5 = it_ushr.next();
          var r6 = it_ushr.next(8);
          if (!(r5.value === 8 && r5.done === false)) return false;
          if (!(r6.value === 4 && r6.done === true)) return false;

          return true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(ok, Value::Bool(true));
}

#[test]
fn generators_in_and_instanceof_operators_allow_yield_in_operands() {
  let mut rt = new_runtime();
  let ok = rt
    .exec_script(
      r#"
        (() => {
          function* g_in() { return (yield "a") in ({a: 1}); }
          var it_in = g_in();
          var r1 = it_in.next();
          var r2 = it_in.next("a");
          if (!(r1.value === "a" && r1.done === false)) return false;
          if (!(r2.value === true && r2.done === true)) return false;

          function* g_instanceof() { return (yield (function(){})) instanceof Function; }
          var it_inst = g_instanceof();
          var r3 = it_inst.next();
          var fn = r3.value;
          if (!(typeof fn === "function" && r3.done === false)) return false;
          var r4 = it_inst.next(fn);
          if (!(r4.value === true && r4.done === true)) return false;

          return true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(ok, Value::Bool(true));
}

#[test]
fn generators_bigint_typeerrors_are_catchable_after_yield() {
  let mut rt = new_runtime();
  let ok = rt
    .exec_script(
      r#"
        (() => {
          // BigInt + Number should throw TypeError, and the throw must be surfaced on resume.
          function* g_add() { return (yield 1n) + 1; }
          var it_add = g_add();
          var r1 = it_add.next();
          if (!(r1.value === 1n && r1.done === false)) return false;
          try {
            it_add.next(1n);
            return false;
          } catch (e) {
            if (!(e && e.name === "TypeError")) return false;
          }

          // BigInt `>>>` should throw TypeError (BigInt only supports `<<` and `>>`).
          function* g_ushr() { return (yield 1n) >>> 1; }
          var it_ushr = g_ushr();
          var r2 = it_ushr.next();
          if (!(r2.value === 1n && r2.done === false)) return false;
          try {
            it_ushr.next(1n);
            return false;
          } catch (e) {
            if (!(e && e.name === "TypeError")) return false;
          }

          return true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(ok, Value::Bool(true));
}

