use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(1024 * 1024, 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn regexp_canonicalize_kelvin_sign_requires_unicode_flag() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "regexp_kelvin.js",
    r#"
      (
        /\u212a/i.test('k') === false &&
        /\u212a/i.test('K') === false &&
        /\u212a/u.test('k') === false &&
        /\u212a/u.test('K') === false &&
        /\u212a/iu.test('k') === true &&
        /\u212a/iu.test('K') === true
      )
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn regexp_canonicalize_does_not_apply_full_case_folding() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "regexp_full_case_folding.js",
    r#"
      (
        /[\u0390]/ui.test("\u1fd3") &&
        /[\u1fd3]/ui.test("\u0390") &&
        /[\u03b0]/ui.test("\u1fe3") &&
        /[\u1fe3]/ui.test("\u03b0") &&
        /[\ufb05]/ui.test("\ufb06") &&
        /[\ufb06]/ui.test("\ufb05")
      )
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn regexp_word_characters_include_extra_word_chars_under_unicode_ignore_case() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "regexp_word_characters.js",
    r#"
      (
        /\w/i.test("\u212a") === false &&
        /\w/iu.test("\u212a") === true &&
        /\b/i.test("\u212a") === false &&
        /\b/iu.test("\u212a") === true &&
        new RegExp("\u212a", "iv").test("k") === true &&
        new RegExp("\u212a", "iv").test("K") === true
      )
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

