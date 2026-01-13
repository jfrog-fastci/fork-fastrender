use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(1024 * 1024, 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn regexp_unicode_surrogate_pairs_are_single_code_points_in_u_mode() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "regexp_surrogate_pairs.js",
    r#"
      (
        /^.$/u.test('\ud800\udc00') === true &&

        /^[\ud800\udc00]$/u.test('\ud800\udc00') === true &&
        /[\ud800\udc00]/u.test('\ud800') === false &&
        /[\ud800\udc00]/u.test('\udc00') === false &&

        /^\S$/u.test('\ud800\udc00') === true &&

        /(.+).*\1/u.test('\ud800\udc00\ud800') === false &&

        /^[\ud834\udf06]$/u.test('\ud834\udf06') === true
      )
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

