#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::decompile::{lower_program, LoweredArg, LoweredInst};
use optimize_js::il::inst::Const;
use optimize_js::TopLevelMode;
use parse_js::num::JsNumber;

#[test]
fn return_stmt_is_lowered_into_a_return_inst() {
  let program = compile_source(
    r#"
      const make = () => {
        return 1;
      };
      make();
    "#,
    TopLevelMode::Module,
    false,
  );
  let lowered = lower_program(&program);

  assert_eq!(lowered.functions.len(), 1);
  let func = &lowered.functions[0];

  let mut saw_return_one = false;
  for block in func.bblocks.iter() {
    for inst in block.insts.iter() {
      if let LoweredInst::Return {
        value: Some(LoweredArg::Const(Const::Num(JsNumber(1.0)))),
      } = inst
      {
        saw_return_one = true;
      }
    }
  }

  assert!(saw_return_one, "expected return 1; to lower into a Return inst");
}

#[test]
fn throw_stmt_is_lowered_into_a_throw_inst() {
  let program = compile_source(
    r#"
      const fail = () => {
        throw 1;
      };
      fail();
    "#,
    TopLevelMode::Module,
    false,
  );
  let lowered = lower_program(&program);

  assert_eq!(lowered.functions.len(), 1);
  let func = &lowered.functions[0];

  let mut saw_throw_one = false;
  for block in func.bblocks.iter() {
    for inst in block.insts.iter() {
      if let LoweredInst::Throw {
        value: LoweredArg::Const(Const::Num(JsNumber(1.0))),
      } = inst
      {
        saw_throw_one = true;
      }
    }
  }

  assert!(saw_throw_one, "expected throw 1; to lower into a Throw inst");
}
