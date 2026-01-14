use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_private_field_logical_or_assignment_short_circuits_without_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var called = false;
        function rhs() {
          called = true;
          return (function*() { yield 1; })();
        }

        class C {
          static #x = 1;
          static getX(){ return this.#x; }
          static *g(){
            // RHS contains a yield*, but must not be evaluated because #x is truthy.
            const r = (this.#x ||= (yield* rhs()));
            return r === 1 && this.#x === 1 && called === false;
          }
        }

        const it = C.g();
        const r1 = it.next();
        r1.done === true && r1.value === true && called === false && C.getX() === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_logical_and_assignment_short_circuits_without_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var called = false;
        function rhs() {
          called = true;
          return (function*() { yield 1; })();
        }

        class C {
          static #x = 0;
          static getX(){ return this.#x; }
          static *g(){
            // RHS contains a yield*, but must not be evaluated because #x is falsy.
            const r = (this.#x &&= (yield* rhs()));
            return r === 0 && this.#x === 0 && called === false;
          }
        }

        const it = C.g();
        const r1 = it.next();
        r1.done === true && r1.value === true && called === false && C.getX() === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_nullish_coalescing_assignment_short_circuits_without_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var called = false;
        function rhs() {
          called = true;
          return (function*() { yield 1; })();
        }

        class C {
          static #x = 0;
          static getX(){ return this.#x; }
          static *g(){
            // RHS contains a yield*, but must not be evaluated because #x is non-nullish.
            const r = (this.#x ??= (yield* rhs()));
            return r === 0 && this.#x === 0 && called === false;
          }
        }

        const it = C.g();
        const r1 = it.next();
        r1.done === true && r1.value === true && called === false && C.getX() === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_nullish_coalescing_assignment_captures_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          static #x = null;
          static getX(){ return this.#x; }
          static setX(v){ this.#x = v; }
          static *g(){
            const r = (this.#x ??= (yield 0));
            return r === 5 && this.#x === 5;
          }
        }

        const it = C.g();
        const r1 = it.next();

        // Mutate after the yield but before resuming. The decision to assign was made before the
        // yield (because #x was nullish), so the assignment must still happen.
        C.setX(0);

        const r2 = it.next(5);

        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === true &&
        C.getX() === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_logical_and_assignment_captures_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          static #x = 1;
          static getX(){ return this.#x; }
          static setX(v){ this.#x = v; }
          static *g(){
            const r = (this.#x &&= (yield 0));
            return r === 5 && this.#x === 5;
          }
        }

        const it = C.g();
        const r1 = it.next();

        // Mutate after the yield but before resuming. The decision to assign was made before the
        // yield (because #x was truthy), so the assignment must still happen.
        C.setX(0);

        const r2 = it.next(5);

        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === true &&
        C.getX() === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_logical_or_assignment_captures_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          static #x = 0;
          static getX(){ return this.#x; }
          static setX(v){ this.#x = v; }
          static *g(){
            const r = (this.#x ||= (yield 0));
            return r === 5 && this.#x === 5;
          }
        }

        const it = C.g();
        const r1 = it.next();

        // Mutate after the yield but before resuming. The decision to assign was made before the
        // yield (because #x was falsy), so the assignment must still happen.
        C.setX(1);

        const r2 = it.next(5);

        r1.done === false && r1.value === 0 &&
        r2.done === true && r2.value === true &&
        C.getX() === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_logical_or_assignment_captures_decision_across_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 5;
        }

        class C {
          static #x = 0;
          static getX(){ return this.#x; }
          static setX(v){ this.#x = v; }
          static *g(){
            const r = (this.#x ||= (yield* rhs()));
            return r === 5 && this.#x === 5;
          }
        }

        const it = C.g();
        const r1 = it.next();

        // Mutate after the first yield but before resuming. Keep this mutation active so engines
        // that re-check the decision after intermediate yields are caught.
        C.setX(1);

        const r2 = it.next();
        const r3 = it.next();

        r1.done === false && r1.value === "rhs1" &&
        r2.done === false && r2.value === "rhs2" &&
        r3.done === true && r3.value === true &&
        C.getX() === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_logical_and_assignment_captures_decision_across_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 5;
        }

        class C {
          static #x = 1;
          static getX(){ return this.#x; }
          static setX(v){ this.#x = v; }
          static *g(){
            const r = (this.#x &&= (yield* rhs()));
            return r === 5 && this.#x === 5;
          }
        }

        const it = C.g();
        const r1 = it.next();

        // Mutate after the first yield but before resuming. Keep this mutation active so engines
        // that re-check the decision after intermediate yields are caught.
        C.setX(0);

        const r2 = it.next();
        const r3 = it.next();

        r1.done === false && r1.value === "rhs1" &&
        r2.done === false && r2.value === "rhs2" &&
        r3.done === true && r3.value === true &&
        C.getX() === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_nullish_coalescing_assignment_captures_decision_across_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 5;
        }

        class C {
          static #x = null;
          static getX(){ return this.#x; }
          static setX(v){ this.#x = v; }
          static *g(){
            const r = (this.#x ??= (yield* rhs()));
            return r === 5 && this.#x === 5;
          }
        }

        const it = C.g();
        const r1 = it.next();

        // Mutate after the first yield but before resuming. Keep this mutation active so engines
        // that re-check the decision after intermediate yields are caught.
        C.setX(0);

        const r2 = it.next();
        const r3 = it.next();

        r1.done === false && r1.value === "rhs1" &&
        r2.done === false && r2.value === "rhs2" &&
        r3.done === true && r3.value === true &&
        C.getX() === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
