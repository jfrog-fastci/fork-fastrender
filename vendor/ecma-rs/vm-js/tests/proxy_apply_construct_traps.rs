use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn proxy_apply_trap_receives_args_array() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"let seen;
         let p=new Proxy(function(){}, { apply(t,thisArg,args){ seen=args.length; return 1; } });
         p(1,2,3);
         seen===3"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_apply_trap_gets_correct_this_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"let seen;
         let p=new Proxy(function(){}, { apply(t,thisArg,args){ seen=thisArg; return 1; } });
         p.call(123);
         seen===123"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_construct_trap_must_return_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"let p=new Proxy(function(){}, { construct(){ return 1; } });
         try { new p(); false } catch(e) { e instanceof TypeError }"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn proxy_construct_trap_receives_new_target() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"let seen;
       let p=new Proxy(function(){}, { construct(t,args,nt){ seen=nt===p; return {}; } });
       new p();
       seen===true"#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

