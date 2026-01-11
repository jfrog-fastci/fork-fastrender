use num_bigint::BigInt;
use optimize_js::eval::consteval::{
  coerce_str_to_num, coerce_to_bool, js_cmp, js_div, js_loose_eq, maybe_eval_const_bin_expr,
  maybe_eval_const_builtin_call, maybe_eval_const_builtin_val, maybe_eval_const_un_expr,
};
use optimize_js::il::inst::Const::{BigInt as ConstBigInt, Num as ConstNum, Str as ConstStr};
use optimize_js::il::inst::{BinOp, UnOp};
use parse_js::num::JsNumber as JN;
use std::cmp::Ordering;

#[test]
fn number_builtin_matches_string_to_number() {
  let eval_number = |s: &str| match maybe_eval_const_builtin_call("Number", &[ConstStr(s.into())]) {
    Some(ConstNum(JN(v))) => v,
    other => panic!("unexpected eval result for {s:?}: {other:?}"),
  };

  assert_eq!(eval_number("  "), 0.0);
  assert_eq!(eval_number("0x10"), 16.0);
  assert_eq!(eval_number("0b10"), 2.0);
  assert_eq!(eval_number("0o10"), 8.0);

  let inf = eval_number("Infinity");
  assert!(inf.is_infinite() && inf.is_sign_positive());
  assert!(eval_number("not a number").is_nan());
}

#[test]
fn math_round_matches_ecmascript_ties_to_plus_infinity() {
  let eval_round = |n: f64| match maybe_eval_const_builtin_call("Math.round", &[ConstNum(JN(n))]) {
    Some(ConstNum(JN(v))) => v,
    other => panic!("unexpected eval result for {n}: {other:?}"),
  };

  assert_eq!(eval_round(1.5), 2.0);
  assert_eq!(eval_round(-1.5), -1.0);

  let neg_zero = eval_round(-0.1);
  assert_eq!(neg_zero, 0.0);
  assert!(
    neg_zero.is_sign_negative(),
    "Math.round(-0.1) should preserve -0"
  );
  let neg_zero_half = eval_round(-0.5);
  assert_eq!(neg_zero_half, 0.0);
  assert!(
    neg_zero_half.is_sign_negative(),
    "Math.round(-0.5) should preserve -0"
  );
}

#[test]
fn bigint_and_string_loose_equality_follows_string_to_bigint() {
  assert!(js_loose_eq(
    &ConstBigInt(BigInt::from(1)),
    &ConstStr("1".into())
  ));
  assert!(js_loose_eq(
    &ConstBigInt(BigInt::from(-1)),
    &ConstStr("-1".into())
  ));
  assert!(!js_loose_eq(
    &ConstBigInt(BigInt::from(1)),
    &ConstStr("1.0".into())
  ));
  assert!(!js_loose_eq(
    &ConstBigInt(BigInt::from(1)),
    &ConstStr("1n".into())
  ));

  // `BigInt("")` and `BigInt("   ")` produce `0n`.
  assert!(js_loose_eq(
    &ConstBigInt(BigInt::from(0)),
    &ConstStr("".into())
  ));
  assert!(js_loose_eq(
    &ConstBigInt(BigInt::from(0)),
    &ConstStr("   ".into())
  ));
  assert!(!js_loose_eq(
    &ConstBigInt(BigInt::from(1)),
    &ConstStr("".into())
  ));

  assert!(js_loose_eq(
    &ConstBigInt(BigInt::from(15)),
    &ConstStr("0xF".into())
  ));
  assert!(js_loose_eq(
    &ConstBigInt(BigInt::from(15)),
    &ConstStr("0x0F".into())
  ));
  assert!(js_loose_eq(
    &ConstBigInt(BigInt::from(15)),
    &ConstStr("0X0F".into())
  ));
  assert!(js_loose_eq(
    &ConstBigInt(BigInt::from(2)),
    &ConstStr("0b10".into())
  ));
  assert!(!js_loose_eq(
    &ConstBigInt(BigInt::from(-15)),
    &ConstStr("-0xF".into())
  ));
  assert!(!js_loose_eq(
    &ConstBigInt(BigInt::from(15)),
    &ConstStr("+0xF".into())
  ));
}

#[test]
fn bigint_and_string_relational_comparisons_use_string_to_number() {
  assert_eq!(
    js_cmp(&ConstBigInt(BigInt::from(3)), &ConstStr(" 4 ".into())),
    Some(Ordering::Less)
  );
  assert_eq!(
    js_cmp(&ConstBigInt(BigInt::from(3)), &ConstStr("4.5".into())),
    Some(Ordering::Less)
  );
  assert_eq!(
    js_cmp(&ConstStr("4.5".into()), &ConstBigInt(BigInt::from(3))),
    Some(Ordering::Greater)
  );
  assert_eq!(
    js_cmp(
      &ConstBigInt(BigInt::from(3)),
      &ConstStr("not a number".into())
    ),
    None
  );
}

#[test]
fn bigint_and_number_comparisons_follow_spec_without_rounding() {
  // 9007199254740993 is not exactly representable as f64 (it rounds to 2^53).
  let rounded = 9007199254740993.0_f64;
  assert_eq!(rounded, 9007199254740992.0);

  let bigint = ConstBigInt(BigInt::from(9007199254740993i64));
  let num = ConstNum(JN(rounded));
  assert!(
    !js_loose_eq(&bigint, &num),
    "BigInt == Number should compare against the exact numeric value, not a rounded BigInt->f64 conversion"
  );
  assert_eq!(js_cmp(&bigint, &num), Some(Ordering::Greater));
  assert_eq!(js_cmp(&num, &bigint), Some(Ordering::Less));

  let exact_bigint = ConstBigInt(BigInt::from(9007199254740992i64));
  let exact_num = ConstNum(JN(9007199254740992.0));
  assert!(js_loose_eq(&exact_bigint, &exact_num));

  assert_eq!(js_cmp(&ConstBigInt(BigInt::from(3)), &ConstNum(JN(3.2))), Some(Ordering::Less));
  assert_eq!(js_cmp(&ConstNum(JN(3.2)), &ConstBigInt(BigInt::from(3))), Some(Ordering::Greater));
  assert!(!js_loose_eq(&ConstBigInt(BigInt::from(3)), &ConstNum(JN(3.2))));
}

#[test]
fn bigint_truthiness_follows_spec() {
  assert!(!coerce_to_bool(&ConstBigInt(BigInt::from(0))));
  assert!(coerce_to_bool(&ConstBigInt(BigInt::from(1))));
  assert!(coerce_to_bool(&ConstBigInt(BigInt::from(-1))));
}

#[test]
fn negative_zero_is_preserved_through_division() {
  let neg_zero = coerce_str_to_num("-0");
  match neg_zero {
    v if v == 0.0 => assert!(v.is_sign_negative()),
    _ => panic!("expected -0 from string coercion"),
  }

  assert_eq!(js_div(1.0, neg_zero), f64::NEG_INFINITY);
  assert_eq!(js_div(-1.0, neg_zero), f64::INFINITY);
  assert!(js_loose_eq(&ConstNum(JN(0.0)), &ConstStr("-0".into())));
}

#[test]
fn bitwise_and_shift_ops_follow_to_int32_semantics() {
  let one = ConstNum(JN(1.0));
  let two = ConstNum(JN(2.0));
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Shl, &one, &two),
    Some(ConstNum(JN(4.0)))
  );

  let minus_one = ConstNum(JN(-1.0));
  let zero = ConstNum(JN(0.0));
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::UShr, &minus_one, &zero),
    Some(ConstNum(JN(4294967295.0)))
  );

  let fractional = ConstNum(JN(1.9));
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::BitOr, &fractional, &zero),
    Some(ConstNum(JN(1.0)))
  );

  let shift_32 = ConstNum(JN(32.0));
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Shl, &one, &shift_32),
    Some(ConstNum(JN(1.0)))
  );

  assert_eq!(
    maybe_eval_const_un_expr(UnOp::BitNot, &zero),
    Some(ConstNum(JN(-1.0)))
  );
}

#[test]
fn bigint_arithmetic_and_bitops_are_constant_folded() {
  let one = ConstBigInt(BigInt::from(1));
  let two = ConstBigInt(BigInt::from(2));
  let three = ConstBigInt(BigInt::from(3));

  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &one, &two),
    Some(three.clone())
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Sub, &one, &two),
    Some(ConstBigInt(BigInt::from(-1)))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Mul, &ConstBigInt(BigInt::from(-3)), &two),
    Some(ConstBigInt(BigInt::from(-6)))
  );

  // Division and remainder are truncated toward zero (matching JS BigInt semantics).
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Div, &ConstBigInt(BigInt::from(7)), &three),
    Some(ConstBigInt(BigInt::from(2)))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Div, &ConstBigInt(BigInt::from(-7)), &three),
    Some(ConstBigInt(BigInt::from(-2)))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Mod, &ConstBigInt(BigInt::from(-7)), &three),
    Some(ConstBigInt(BigInt::from(-1)))
  );

  // BigInt bit operations.
  let minus_one = ConstBigInt(BigInt::from(-1));
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::BitAnd, &minus_one, &one),
    Some(one.clone())
  );
  assert_eq!(
    maybe_eval_const_un_expr(UnOp::BitNot, &ConstBigInt(BigInt::from(0))),
    Some(minus_one.clone())
  );

  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Shl, &one, &two),
    Some(ConstBigInt(BigInt::from(4)))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Shr, &ConstBigInt(BigInt::from(-3)), &one),
    Some(ConstBigInt(BigInt::from(-2)))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::UShr, &one, &one),
    None,
    "BigInt does not support `>>>`"
  );

  // Exponentiation.
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Exp, &ConstBigInt(BigInt::from(-2)), &three),
    Some(ConstBigInt(BigInt::from(-8)))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Exp, &ConstBigInt(BigInt::from(0)), &ConstBigInt(BigInt::from(0))),
    Some(one.clone())
  );

  // Operations that throw at runtime should not be folded.
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Div, &one, &ConstBigInt(BigInt::from(0))),
    None
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Mod, &one, &ConstBigInt(BigInt::from(0))),
    None
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Exp, &two, &ConstBigInt(BigInt::from(-1))),
    None
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Shl, &one, &ConstBigInt(BigInt::from(-1))),
    None
  );

  // Avoid compile-time amplification of huge BigInts from small literals.
  let huge = ConstBigInt(BigInt::from(1_000_000_000u64));
  assert_eq!(maybe_eval_const_bin_expr(BinOp::Shl, &one, &huge), None);
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Exp, &two, &huge),
    None,
    "2n ** 1e9n would allocate an enormous BigInt"
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Shl, &ConstBigInt(BigInt::from(0)), &huge),
    Some(ConstBigInt(BigInt::from(0)))
  );
}

#[test]
fn bigint_builtin_matches_to_bigint_semantics() {
  let eval_bigint = |arg: optimize_js::il::inst::Const| match maybe_eval_const_builtin_call("BigInt", &[arg]) {
    Some(ConstBigInt(v)) => v,
    other => panic!("unexpected eval result: {other:?}"),
  };

  assert_eq!(eval_bigint(ConstStr("".into())), BigInt::from(0));
  assert_eq!(eval_bigint(ConstStr("   ".into())), BigInt::from(0));
  assert_eq!(eval_bigint(ConstStr("0xF".into())), BigInt::from(15));
  assert_eq!(eval_bigint(optimize_js::il::inst::Const::Bool(true)), BigInt::from(1));
  assert_eq!(eval_bigint(optimize_js::il::inst::Const::Bool(false)), BigInt::from(0));
  assert_eq!(eval_bigint(ConstNum(JN(1.0))), BigInt::from(1));

  // 9007199254740993 is not exactly representable as f64 (it rounds to 2^53).
  assert_eq!(
    eval_bigint(ConstNum(JN(9007199254740993.0))),
    BigInt::from(9007199254740992i64)
  );

  assert_eq!(
    maybe_eval_const_builtin_call("BigInt", &[ConstNum(JN(1.1))]),
    None,
    "BigInt(1.1) throws RangeError"
  );
  assert_eq!(
    maybe_eval_const_builtin_call("BigInt", &[ConstNum(JN(f64::INFINITY))]),
    None
  );
  assert_eq!(
    maybe_eval_const_builtin_call("BigInt", &[optimize_js::il::inst::Const::Null]),
    None,
    "BigInt(null) throws TypeError"
  );
  assert_eq!(
    maybe_eval_const_builtin_call("BigInt", &[optimize_js::il::inst::Const::Undefined]),
    None,
    "BigInt(undefined) throws TypeError"
  );
  assert_eq!(
    maybe_eval_const_builtin_call("BigInt", &[ConstStr("not a number".into())]),
    None,
    "invalid BigInt string literal throws SyntaxError"
  );
  assert_eq!(
    maybe_eval_const_builtin_call("BigInt", &[ConstStr("-0xF".into())]),
    None,
    "signed prefixed forms are rejected"
  );
}

#[test]
fn string_concatenation_uses_js_to_string_for_numbers() {
  let empty = ConstStr(String::new());

  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &ConstNum(JN(f64::INFINITY)), &empty),
    Some(ConstStr("Infinity".into()))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &empty, &ConstNum(JN(f64::INFINITY))),
    Some(ConstStr("Infinity".into()))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &ConstNum(JN(f64::NEG_INFINITY)), &empty),
    Some(ConstStr("-Infinity".into()))
  );

  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &ConstNum(JN(-0.0)), &ConstStr("x".into())),
    Some(ConstStr("0x".into()))
  );

  // Exponent form always includes an explicit `+` sign.
  let large = 1e30_f64;
  match maybe_eval_const_bin_expr(BinOp::Add, &ConstNum(JN(large)), &empty) {
    Some(ConstStr(s)) => assert!(
      s.contains("e+"),
      "expected exponent to include `+`, got {s:?}"
    ),
    other => panic!("unexpected eval result for {large}: {other:?}"),
  }

  // `NumberToString` uses decimal form for 1e20 but exponential form for 1e21.
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &ConstNum(JN(1e20)), &empty),
    Some(ConstStr("100000000000000000000".into()))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &ConstNum(JN(1e21)), &empty),
    Some(ConstStr("1e+21".into()))
  );

  // Scientific notation threshold is < 1e-6.
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &ConstNum(JN(1e-6)), &empty),
    Some(ConstStr("0.000001".into()))
  );
  assert_eq!(
    maybe_eval_const_bin_expr(BinOp::Add, &ConstNum(JN(1e-7)), &empty),
    Some(ConstStr("1e-7".into()))
  );
}

#[test]
fn string_relational_comparison_uses_utf16_code_unit_ordering() {
  // JS string comparison is defined over UTF-16 code units (not Unicode scalar values / UTF-8
  // byte order). This matters for characters outside the BMP, which are represented as surrogate
  // pairs in UTF-16.
  let emoji = ConstStr("\u{1F600}".into()); // 😀 => [0xD83D, 0xDE00]
  let pua = ConstStr("\u{E000}".into()); // BMP char => [0xE000]

  assert_eq!(js_cmp(&emoji, &pua), Some(Ordering::Less));
  assert_eq!(js_cmp(&pua, &emoji), Some(Ordering::Greater));
}

#[test]
fn builtin_infinity_and_undefined_are_constant_folded() {
  assert_eq!(
    maybe_eval_const_builtin_val("Infinity"),
    Some(ConstNum(JN(f64::INFINITY)))
  );
  assert_eq!(
    maybe_eval_const_builtin_val("Math.LN2"),
    Some(ConstNum(JN(std::f64::consts::LN_2)))
  );
  assert_eq!(
    maybe_eval_const_builtin_val("Number.MAX_VALUE"),
    Some(ConstNum(JN(f64::MAX)))
  );
  assert_eq!(
    maybe_eval_const_builtin_val("Number.MIN_VALUE"),
    Some(ConstNum(JN(f64::from_bits(1))))
  );
  assert_eq!(
    maybe_eval_const_builtin_val("undefined"),
    Some(optimize_js::il::inst::Const::Undefined)
  );
}
