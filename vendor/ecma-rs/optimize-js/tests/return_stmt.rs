#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use emit_js::EmitOptions;
use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::{program_to_js, DecompileOptions, TopLevelMode};
use parse_js::num::JsNumber;

#[test]
fn return_statements_in_functions_are_supported() {
  // Arrow function expression bodies lower to an implicit `return` in HIR.
  let src = r#"
    const make = () => 1;
    make();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(
    program.functions.len(),
    1,
    "expected arrow function body to be compiled"
  );

  assert!(
    program.functions[0]
      .body
      .bblocks
      .all()
      .any(|(_, block)| block.last().is_some_and(|inst| inst.t == InstTyp::Return)),
    "expected arrow function body CFG to end with Return"
  );

  // Nested functions are not yet emitted by the decompiler, but `program_to_js`
  // should still run without panicking while inspecting nested function CFGs.
  let _ = program_to_js(&program, &DecompileOptions::default(), EmitOptions::minified());
}

#[test]
fn return_statements_lower_to_return_insts() {
  let src = r#"
    const make = () => {
      return 1;
    };
    make();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);

  let saw_return_one = program.functions[0]
    .body
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .any(|inst| {
      inst.t == InstTyp::Return
        && inst.args.as_slice() == [Arg::Const(Const::Num(JsNumber(1.0)))]
    });
  assert!(saw_return_one, "expected Return inst returning 1");
}

#[test]
fn return_void_lower_to_return_undefined() {
  let src = r#"
    const make = () => {
      return;
    };
    make();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);

  let saw_return_undefined = program.functions[0]
    .body
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .any(|inst| inst.t == InstTyp::Return && inst.as_return().is_none());
  assert!(
    saw_return_undefined,
    "expected Return inst with implicit undefined for `return;`"
  );
}
