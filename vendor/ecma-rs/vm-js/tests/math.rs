use vm_js::{
  Budget, GcObject, Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Scope, TerminationReason,
  Value, Vm, VmError, VmHostHooks, VmOptions,
};

struct TestRt {
  vm: Vm,
  heap: Heap,
  realm: Realm,
}

impl TestRt {
  fn new(options: VmOptions) -> Result<Self, VmError> {
    let mut vm = Vm::new(options);
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let realm = Realm::new(&mut vm, &mut heap)?;
    Ok(Self { vm, heap, realm })
  }
}

impl Drop for TestRt {
  fn drop(&mut self) {
    self.realm.teardown(&mut self.heap);
  }
}

fn get_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let Some(desc) = scope.heap().get_property(obj, &key)? else {
    return Ok(None);
  };
  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(Some(value)),
    PropertyKind::Accessor { .. } => Err(VmError::PropertyNotData),
  }
}

fn assert_is_pos_zero(n: f64) {
  assert_eq!(n, 0.0);
  assert!(!n.is_sign_negative(), "expected +0, got -0");
}

fn assert_is_neg_zero(n: f64) {
  assert_eq!(n, 0.0);
  assert!(n.is_sign_negative(), "expected -0, got +0");
}

#[test]
fn max_min_and_sign_handle_negative_zero() -> Result<(), VmError> {
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();

  let max = get_data_property(&mut scope, math, "max")?.unwrap();
  let min = get_data_property(&mut scope, math, "min")?.unwrap();
  let sign = get_data_property(&mut scope, math, "sign")?.unwrap();

  let out = rt
    .vm
    .call_without_host(&mut scope, max, Value::Object(math), &[Value::Number(-0.0), Value::Number(0.0)])?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.max did not return number"));
  };
  assert_is_pos_zero(n);

  let out = rt
    .vm
    .call_without_host(&mut scope, min, Value::Object(math), &[Value::Number(0.0), Value::Number(-0.0)])?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.min did not return number"));
  };
  assert_is_neg_zero(n);

  let out = rt
    .vm
    .call_without_host(&mut scope, sign, Value::Object(math), &[Value::Number(-0.0)])?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.sign did not return number"));
  };
  assert_is_neg_zero(n);

  Ok(())
}

#[test]
fn round_is_spec_correct_for_half_ulp_edges() -> Result<(), VmError> {
  // Regression test for IEEE-754 double-rounding:
  // `0.5 - Number.EPSILON/4` must round down to +0.
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();
  let mut scope = rt.heap.scope();
  let math = intr.math();
  let round = get_data_property(&mut scope, math, "round")?.unwrap();

  let x = 0.5 - f64::EPSILON / 4.0;
  let out = rt
    .vm
    .call_without_host(&mut scope, round, Value::Object(math), &[Value::Number(x)])?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.round did not return number"));
  };
  assert_is_pos_zero(n);
  assert_eq!(1.0 / n, f64::INFINITY);

  // Tie-breaking goes toward +∞, so `Math.round(-0.5)` must preserve negative zero.
  let out = rt.vm.call_without_host(
    &mut scope,
    round,
    Value::Object(math),
    &[Value::Number(-0.5)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.round did not return number"));
  };
  assert_is_neg_zero(n);
  assert_eq!(1.0 / n, f64::NEG_INFINITY);

  Ok(())
}

#[test]
fn unary_math_functions_preserve_negative_zero() -> Result<(), VmError> {
  // Many `%Math%` unary methods are specified to preserve the sign of zero.
  //
  // Examples (test262):
  // - Math.asin(-0) === -0
  // - Math.expm1(-0) === -0
  // - Math.log1p(-0) === -0
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();

  let names = [
    "asin", "atan", "asinh", "atanh", "cbrt", "expm1", "log1p", "sin", "sinh", "tan", "tanh",
  ];

  for name in names {
    let func = get_data_property(&mut scope, math, name)?.unwrap();
    let out = rt.vm.call_without_host(
      &mut scope,
      func,
      Value::Object(math),
      &[Value::Number(-0.0)],
    )?;
    let Value::Number(n) = out else {
      return Err(VmError::Unimplemented("Math method did not return number"));
    };
    assert_is_neg_zero(n);
  }

  Ok(())
}

#[test]
fn round_handles_signed_zero_and_rounding_edges() -> Result<(), VmError> {
  // test262: built-ins/Math/round/S15.8.2.15_A7.js
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let round = get_data_property(&mut scope, math, "round")?.unwrap();

  // Values in [-0.5, -0] round to -0.
  for x in [-0.5, -0.25, -0.0] {
    let out = rt.vm.call_without_host(
      &mut scope,
      round,
      Value::Object(math),
      &[Value::Number(x)],
    )?;
    let Value::Number(n) = out else {
      return Err(VmError::Unimplemented("Math.round did not return number"));
    };
    assert_is_neg_zero(n);
  }

  // Values just below 0.5 must round to +0 even when `x + 0.5` would round to 1.0 in binary64.
  let x = 0.5 - f64::EPSILON / 4.0;
  let out = rt.vm.call_without_host(
    &mut scope,
    round,
    Value::Object(math),
    &[Value::Number(x)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.round did not return number"));
  };
  assert_is_pos_zero(n);

  // Large odd integers around 2^52 must round to themselves.
  let eps = f64::EPSILON;
  let cases = [
    -(2.0 / eps - 1.0),
    -(1.5 / eps - 1.0),
    -(1.0 / eps + 1.0),
    1.0 / eps + 1.0,
    1.5 / eps - 1.0,
    2.0 / eps - 1.0,
  ];
  for x in cases {
    let out = rt.vm.call_without_host(
      &mut scope,
      round,
      Value::Object(math),
      &[Value::Number(x)],
    )?;
    assert_eq!(out, Value::Number(x));
  }

  Ok(())
}

#[test]
fn atan2_preserves_signed_zero() -> Result<(), VmError> {
  // test262: built-ins/Math/atan2/S15.8.2.5_A5.js / A9.js
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let atan2 = get_data_property(&mut scope, math, "atan2")?.unwrap();

  let out = rt.vm.call_without_host(
    &mut scope,
    atan2,
    Value::Object(math),
    &[Value::Number(0.0), Value::Number(0.0)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.atan2 did not return number"));
  };
  assert_is_pos_zero(n);

  let out = rt.vm.call_without_host(
    &mut scope,
    atan2,
    Value::Object(math),
    &[Value::Number(-0.0), Value::Number(0.0)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.atan2 did not return number"));
  };
  assert_is_neg_zero(n);

  Ok(())
}

#[test]
fn hypot_infinity_overrides_nan() -> Result<(), VmError> {
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let hypot = get_data_property(&mut scope, math, "hypot")?.unwrap();

  let out = rt.vm.call_without_host(
    &mut scope,
    hypot,
    Value::Object(math),
    &[Value::Number(f64::NAN), Value::Number(f64::INFINITY)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.hypot did not return number"));
  };
  assert!(n.is_infinite() && n.is_sign_positive());
  Ok(())
}

#[test]
fn clz32_and_imul_match_spec_int32_semantics() -> Result<(), VmError> {
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let clz32 = get_data_property(&mut scope, math, "clz32")?.unwrap();
  let imul = get_data_property(&mut scope, math, "imul")?.unwrap();

  let out = rt
    .vm
    .call_without_host(&mut scope, clz32, Value::Object(math), &[Value::Number(1.0)])?;
  assert_eq!(out, Value::Number(31.0));

  let out = rt
    .vm
    .call_without_host(&mut scope, clz32, Value::Object(math), &[Value::Number(0.0)])?;
  assert_eq!(out, Value::Number(32.0));

  let out = rt
    .vm
    .call_without_host(&mut scope, clz32, Value::Object(math), &[Value::Number(-1.0)])?;
  assert_eq!(out, Value::Number(0.0));

  // (2^32 - 1) * 5 mod 2^32 == 2^32 - 5 == -5 as int32.
  let out = rt.vm.call_without_host(
    &mut scope,
    imul,
    Value::Object(math),
    &[Value::Number(4_294_967_295.0), Value::Number(5.0)],
  )?;
  assert_eq!(out, Value::Number(-5.0));

  Ok(())
}

#[test]
fn trunc_preserves_signed_zero_and_infinities() -> Result<(), VmError> {
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let trunc = get_data_property(&mut scope, math, "trunc")?.unwrap();

  let out = rt.vm.call_without_host(
    &mut scope,
    trunc,
    Value::Object(math),
    &[Value::Number(-0.0)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.trunc did not return number"));
  };
  assert_is_neg_zero(n);

  // (-1, 0) truncation should produce -0 (test262: built-ins/Math/trunc/S15.8.2.20_A1.js).
  let out = rt.vm.call_without_host(
    &mut scope,
    trunc,
    Value::Object(math),
    &[Value::Number(-0.1)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.trunc did not return number"));
  };
  assert_is_neg_zero(n);

  let out = rt.vm.call_without_host(
    &mut scope,
    trunc,
    Value::Object(math),
    &[Value::Number(0.1)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.trunc did not return number"));
  };
  assert_is_pos_zero(n);

  let out = rt.vm.call_without_host(
    &mut scope,
    trunc,
    Value::Object(math),
    &[Value::Number(f64::INFINITY)],
  )?;
  assert_eq!(out, Value::Number(f64::INFINITY));

  let out = rt.vm.call_without_host(
    &mut scope,
    trunc,
    Value::Object(math),
    &[Value::Number(f64::NEG_INFINITY)],
  )?;
  assert_eq!(out, Value::Number(f64::NEG_INFINITY));

  let out = rt.vm.call_without_host(
    &mut scope,
    trunc,
    Value::Object(math),
    &[Value::Number(f64::NAN)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.trunc did not return number"));
  };
  assert!(n.is_nan());

  Ok(())
}

#[test]
fn fround_preserves_signed_zero_and_rounds_ties_to_even() -> Result<(), VmError> {
  // test262: built-ins/Math/fround/S15.8.2.16_A1.js
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let fround = get_data_property(&mut scope, math, "fround")?.unwrap();

  // Preserve negative zero.
  let out = rt.vm.call_without_host(
    &mut scope,
    fround,
    Value::Object(math),
    &[Value::Number(-0.0)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.fround did not return number"));
  };
  assert_is_neg_zero(n);

  // Underflow tie: 0.5 * minSubnormal(float32) must round to +0/-0 (ties-to-even).
  let half_min_subnormal = (f32::from_bits(1) as f64) / 2.0;

  let out = rt.vm.call_without_host(
    &mut scope,
    fround,
    Value::Object(math),
    &[Value::Number(half_min_subnormal)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.fround did not return number"));
  };
  assert_is_pos_zero(n);

  let out = rt.vm.call_without_host(
    &mut scope,
    fround,
    Value::Object(math),
    &[Value::Number(-half_min_subnormal)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.fround did not return number"));
  };
  assert_is_neg_zero(n);

  Ok(())
}

#[test]
fn pow_handles_signed_zero_and_nan_exponentiation_edges() -> Result<(), VmError> {
  // test262: built-ins/Math/pow/…
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let pow = get_data_property(&mut scope, math, "pow")?.unwrap();

  // (-0) ** odd integer => -0.
  let out = rt.vm.call_without_host(
    &mut scope,
    pow,
    Value::Object(math),
    &[Value::Number(-0.0), Value::Number(3.0)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.pow did not return number"));
  };
  assert_is_neg_zero(n);

  // (-0) ** even integer => +0.
  let out = rt.vm.call_without_host(
    &mut scope,
    pow,
    Value::Object(math),
    &[Value::Number(-0.0), Value::Number(2.0)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.pow did not return number"));
  };
  assert_is_pos_zero(n);

  // (-0) ** negative odd integer => -Infinity.
  let out = rt.vm.call_without_host(
    &mut scope,
    pow,
    Value::Object(math),
    &[Value::Number(-0.0), Value::Number(-3.0)],
  )?;
  assert_eq!(out, Value::Number(f64::NEG_INFINITY));

  // (-0) ** negative even integer => +Infinity.
  let out = rt.vm.call_without_host(
    &mut scope,
    pow,
    Value::Object(math),
    &[Value::Number(-0.0), Value::Number(-2.0)],
  )?;
  assert_eq!(out, Value::Number(f64::INFINITY));

  // NaN ** ±0 => 1.
  let out = rt.vm.call_without_host(
    &mut scope,
    pow,
    Value::Object(math),
    &[Value::Number(f64::NAN), Value::Number(0.0)],
  )?;
  assert_eq!(out, Value::Number(1.0));

  // |base| == 1 and exponent is ±Infinity => NaN.
  let out = rt.vm.call_without_host(
    &mut scope,
    pow,
    Value::Object(math),
    &[Value::Number(-1.0), Value::Number(f64::INFINITY)],
  )?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.pow did not return number"));
  };
  assert!(n.is_nan());

  Ok(())
}

#[test]
fn atan2_quadrants_for_signed_zero_match_spec() -> Result<(), VmError> {
  // test262: built-ins/Math/atan2/S15.8.2.5_A8.js / A9.js
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let atan2 = get_data_property(&mut scope, math, "atan2")?.unwrap();

  // atan2(+0, -0) === +PI.
  let out = rt.vm.call_without_host(
    &mut scope,
    atan2,
    Value::Object(math),
    &[Value::Number(0.0), Value::Number(-0.0)],
  )?;
  assert_eq!(out, Value::Number(std::f64::consts::PI));

  // atan2(-0, -0) === -PI.
  let out = rt.vm.call_without_host(
    &mut scope,
    atan2,
    Value::Object(math),
    &[Value::Number(-0.0), Value::Number(-0.0)],
  )?;
  assert_eq!(out, Value::Number(-std::f64::consts::PI));

  Ok(())
}

fn xorshift64star_next(state: &mut u64) -> u64 {
  if *state == 0 {
    *state = 0x243F_6A88_85A3_08D3;
  }
  let mut x = *state;
  x ^= x >> 12;
  x ^= x << 25;
  x ^= x >> 27;
  *state = x;
  x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn bits_to_unit_double(x: u64) -> f64 {
  let bits = x >> 11;
  (bits as f64) * (1.0 / ((1u64 << 53) as f64))
}

#[test]
fn math_has_to_string_tag() -> Result<(), VmError> {
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();
  let to_string_tag = rt.realm.well_known_symbols().to_string_tag;

  let scope = rt.heap.scope();
  let math = intr.math();

  let key = PropertyKey::from_symbol(to_string_tag);
  let desc = scope
    .heap()
    .get_property(math, &key)?
    .expect("Math should have @@toStringTag");
  assert!(!desc.enumerable);
  assert!(desc.configurable);
  let PropertyKind::Data { value, writable } = desc.kind else {
    panic!("@@toStringTag should be a data property");
  };
  assert!(!writable);
  let Value::String(s) = value else {
    panic!("@@toStringTag value should be a string");
  };
  assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), "Math");
  Ok(())
}

#[test]
fn math_methods_have_correct_length_properties() -> Result<(), VmError> {
  let mut rt = TestRt::new(VmOptions::default())?;
  let intr = *rt.realm.intrinsics();
  let mut scope = rt.heap.scope();
  let math = intr.math();

  let atan2 = get_data_property(&mut scope, math, "atan2")?.unwrap();
  let Value::Object(atan2_fn) = atan2 else {
    return Err(VmError::Unimplemented("Math.atan2 is not a function"));
  };
  assert_eq!(
    get_data_property(&mut scope, atan2_fn, "length")?.unwrap(),
    Value::Number(2.0)
  );

  let max = get_data_property(&mut scope, math, "max")?.unwrap();
  let Value::Object(max_fn) = max else {
    return Err(VmError::Unimplemented("Math.max is not a function"));
  };
  assert_eq!(
    get_data_property(&mut scope, max_fn, "length")?.unwrap(),
    Value::Number(2.0)
  );

  let random = get_data_property(&mut scope, math, "random")?.unwrap();
  let Value::Object(random_fn) = random else {
    return Err(VmError::Unimplemented("Math.random is not a function"));
  };
  assert_eq!(
    get_data_property(&mut scope, random_fn, "length")?.unwrap(),
    Value::Number(0.0)
  );

  Ok(())
}

#[test]
fn random_is_seeded_and_host_overridable() -> Result<(), VmError> {
  // Deterministic by default: new VMs with the same seed produce the same first output.
  let seed = 0x243F_6A88_85A3_08D3;
  let mut expected_state = seed;
  let expected = bits_to_unit_double(xorshift64star_next(&mut expected_state));

  let mut rt = TestRt::new(VmOptions {
    math_random_seed: seed,
    ..VmOptions::default()
  })?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let random = get_data_property(&mut scope, math, "random")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, random, Value::Object(math), &[])?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("Math.random did not return number"));
  };
  assert_eq!(n, expected);

  // Host override wins.
  #[derive(Default)]
  struct Host {
    next: u64,
  }
  impl VmHostHooks for Host {
    fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {}

    fn host_math_random_u64(&mut self) -> Option<u64> {
      Some(self.next)
    }
  }

  let mut host = Host { next: 0 };
  let out = rt.vm.call_with_host(&mut scope, &mut host, random, Value::Object(math), &[])?;
  assert_eq!(out, Value::Number(0.0));

  Ok(())
}

#[test]
fn variadic_math_methods_tick_in_argument_loops() -> Result<(), VmError> {
  let mut rt = TestRt::new(VmOptions {
    check_time_every: 1,
    ..VmOptions::default()
  })?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();
  let math = intr.math();
  let max = get_data_property(&mut scope, math, "max")?.unwrap();

  // Fuel budget is intentionally too small:
  // - 1 tick at call entry
  // - `Math.max` ticks in its loop at i=0 and i=32
  rt.vm.set_budget(Budget {
    fuel: Some(2),
    deadline: None,
    check_time_every: 1,
  });

  let mut args: Vec<Value> = (0..33).map(|i| Value::Number(i as f64)).collect();
  args[0] = Value::Number(-0.0);

  let err = rt
    .vm
    .call_without_host(&mut scope, max, Value::Object(math), &args)
    .unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected OutOfFuel termination, got {other:?}"),
  }

  Ok(())
}
