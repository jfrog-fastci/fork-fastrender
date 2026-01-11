#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use emit_js::EmitOptions;
use optimize_js::il::inst::InstTyp;
use optimize_js::{program_to_js, DecompileOptions, TopLevelMode};

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
