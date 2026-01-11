#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use emit_js::EmitOptions;
use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::{program_to_js, DecompileOptions, TopLevelMode};
use parse_js::num::JsNumber;

#[test]
fn throw_statements_in_functions_are_supported() {
  let src = r#"
    const fail = () => {
      throw err();
    };
    fail();
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
      .any(|(_, block)| block.last().is_some_and(|inst| inst.t == InstTyp::Throw)),
    "expected arrow function body CFG to end with Throw"
  );

  // Nested functions are not yet emitted by the decompiler, but `program_to_js`
  // should still run without panicking while inspecting nested function CFGs.
  let _ = program_to_js(&program, &DecompileOptions::default(), EmitOptions::minified());
}

#[test]
fn throw_statements_lower_to_throw_insts() {
  let src = r#"
    const fail = () => {
      throw 1;
    };
    fail();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);

  let saw_throw_one = program.functions[0]
    .body
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .any(|inst| {
      inst.t == InstTyp::Throw
        && inst.args.as_slice() == [Arg::Const(Const::Num(JsNumber(1.0)))]
    });
  assert!(saw_throw_one, "expected Throw inst throwing 1");
}
