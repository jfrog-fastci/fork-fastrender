use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  // Force frequent GC cycles so generator continuation rooting is exercised.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 0));
  JsRuntime::new(vm, heap)
}

#[test]
fn generator_yield_in_with_stmt_object_and_body() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
    function* g() {
      var x = 1;
      with ((yield "obj"), { x: 2 }) {
        yield x;
        x = 3;
        yield x;
      }
      yield x;
    }

    const it = g();
    const a = it.next();
    const b = it.next();
    const c = it.next();
    const d = it.next();
    const e = it.next();

    a.value === "obj" && a.done === false &&
    b.value === 2 && b.done === false &&
    c.value === 3 && c.done === false &&
    d.value === 1 && d.done === false &&
    e.done === true
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_yield_in_for_triple_init() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
    function* g() {
      for ((yield "init"), 0; false; ) {}
      return "done";
    }
    const it = g();
    const a = it.next();
    const b = it.next();
    a.value === "init" && a.done === false &&
    b.done === true && b.value === "done"
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_yield_in_for_triple_test_post_and_body_with_lexical_envs() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
    function* g() {
      const fns = [];
      for (let i = 0; (yield ("test:" + i)), i < 2; (yield ("post:" + i)), i++) {
        fns.push(() => i);
        yield ("body:" + i);
      }
      return fns.map(fn => fn()).join(",");
    }

    const it = g();
    const seq = [
      it.next().value, // test:0
      it.next().value, // body:0
      it.next().value, // post:0
      it.next().value, // test:1
      it.next().value, // body:1
      it.next().value, // post:1
      it.next().value, // test:2 (final check)
    ];
    const done = it.next();

    seq[0] === "test:0" &&
    seq[1] === "body:0" &&
    seq[2] === "post:0" &&
    seq[3] === "test:1" &&
    seq[4] === "body:1" &&
    seq[5] === "post:1" &&
    seq[6] === "test:2" &&
    done.done === true &&
    done.value === "0,1"
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_yield_in_for_triple_let_initializer_preserves_tdz_across_suspend() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
    function* g() {
      for (let i = ((yield (() => i)), 0); i < 1; i++) {}
    }

    const it = g();
    const f = it.next().value;
    let tdz_ok = false;
    try {
      f();
    } catch (e) {
      tdz_ok = e && e.name === "ReferenceError";
    }
    const done = it.next();

    tdz_ok && done.done === true
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_yield_in_for_in_rhs_preserves_tdz() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
    function* g() {
      try {
        for (let k in ((yield 1), k)) {}
        return "no error";
      } catch (e) {
        return e.name;
      }
    }

    const it = g();
    const a = it.next();
    const b = it.next();
    a.value === 1 && a.done === false &&
    b.done === true && b.value === "ReferenceError"
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_yield_in_for_in_body_survives_gc_and_resumes_enumeration() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  rt.exec_script(
    r#"
    function* g() {
      const o = { a: 1, b: 2, c: 3 };
      for (let k in o) {
        yield k;
        // Allocate some objects after the yield so resumption triggers GC while the continuation
        // frames are temporarily moved onto the Rust stack.
        const tmp = [];
        for (let i = 0; i < 200; i++) {
          tmp.push({ i });
        }
      }
      return "done";
    }
    globalThis.it = g();
  "#,
  )?;

  assert_eq!(rt.exec_script("it.next().value === 'a'")?, Value::Bool(true));
  rt.heap.collect_garbage();
  assert_eq!(rt.exec_script("it.next().value === 'b'")?, Value::Bool(true));
  rt.heap.collect_garbage();
  assert_eq!(rt.exec_script("it.next().value === 'c'")?, Value::Bool(true));
  rt.heap.collect_garbage();
  assert_eq!(
    rt.exec_script("(()=>{const r = it.next(); return r.done === true && r.value === 'done';})()")?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn generator_yield_in_switch_discriminant_case_and_body() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
    function* g() {
      const log = [];
      switch ((yield "disc"), 2) {
        case ((yield "case1"), 1):
          log.push("case1");
          break;
        case ((yield "case2"), 2):
          log.push("case2");
          yield "body";
          break;
        default:
          log.push("default");
      }
      return log.join(",");
    }

    const it = g();
    const a = it.next();
    const b = it.next();
    const c = it.next();
    const d = it.next();
    const e = it.next();

    a.value === "disc" && a.done === false &&
    b.value === "case1" && b.done === false &&
    c.value === "case2" && c.done === false &&
    d.value === "body" && d.done === false &&
    e.done === true && e.value === "case2"
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_yield_in_class_decl_extends_preserves_strict_mode_on_resume() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
    function* g() {
      try {
        class C extends ((yield "heritage"), (__vmjs_unbound_test_var = 1), Object) {}
        return "no error";
      } catch (e) {
        return e.name;
      }
    }

    // Ensure the global does not exist so sloppy assignment would create it.
    try { delete globalThis.__vmjs_unbound_test_var; } catch (e) {}

    const it = g();
    const a = it.next();
    const b = it.next();

    a.value === "heritage" && a.done === false &&
    b.done === true && b.value === "ReferenceError"
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
