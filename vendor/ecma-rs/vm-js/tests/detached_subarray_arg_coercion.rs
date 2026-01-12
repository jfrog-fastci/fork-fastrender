use vm_js::{Agent, Budget, PropertyKey, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    // Plenty for these small scripts, but large enough that we won't accidentally trip GC paths
    // that could obscure the ordering we want to test.
    vm_js::HeapLimits::new(1024 * 1024, 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn typed_array_subarray_coerces_args_before_throwing_on_detached_buffer() -> Result<(), VmError> {
  let mut agent = new_agent();

  // Create `ab` and `u` in JS so we exercise the real builtins/prototypes.
  let _ = agent.run_script(
    "setup.js",
    "var ab = new ArrayBuffer(8); var u = new Uint8Array(ab, 1, 2);",
    Budget::unlimited(1),
    None,
  )?;

  // Detach `ab` from Rust without executing any JS user code.
  let global = agent.realm().global_object();
  let ab_obj = {
    let mut scope = agent.heap_mut().scope();
    let key = PropertyKey::from_string(scope.alloc_string("ab")?);
    match scope.heap().get(global, &key)? {
      Value::Object(o) => o,
      other => panic!("expected global ab to be an object, got {other:?}"),
    }
  };
  agent.heap_mut().detach_array_buffer(ab_obj)?;

  let called = agent.run_script(
    "u_subarray.js",
    r#"
      var called=false;
      var threw=false;
      try {
        u.subarray({ valueOf(){ called=true; return 0; } });
      } catch(e) {
        threw = e.name === 'TypeError';
      }
      called === true && threw === true
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(called, Value::Bool(true));

  Ok(())
}

