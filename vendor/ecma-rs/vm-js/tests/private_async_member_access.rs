use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn private_member_access_after_await_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out_x = -1;
      var out_m = -1;
      var out_tag = "";
      var err = "";

      class C {
        static #x = 7;
        static #m() { return this.#x + 1; }
        static #tag(strings) { return strings[0] + this.#x; }

        static async readX() { return (await Promise.resolve(this)).#x; }
        static async callM() { return (await Promise.resolve(this)).#m(); }
        static async callTag() { return (await Promise.resolve(this)).#tag`hi`; }
      }

      C.readX().then(v => { out_x = v; }, e => { err = e && e.name; });
      C.callM().then(v => { out_m = v; }, e => { err = e && e.name; });
      C.callTag().then(v => { out_tag = v; }, e => { err = e && e.name; });

      out_x === -1 && out_m === -1 && out_tag === "" && err === ""
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"out_x === 7 && out_m === 8 && out_tag === "hi7" && err === "" "#)?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
