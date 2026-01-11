#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::il::inst::InstTyp;
use optimize_js::TopLevelMode;

#[test]
fn function_falling_off_end_inserts_explicit_return() {
  let src = r#"
    const f = () => { let x = 1; };
    f();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);

  let has_return = program.functions[0]
    .body
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .any(|inst| inst.t == InstTyp::Return);

  assert!(
    has_return,
    "expected implicit function return to be lowered as an explicit Return terminator"
  );
}

#[test]
fn top_level_does_not_insert_implicit_return() {
  let program = compile_source("let x = 1; x;", TopLevelMode::Module, false);

  let has_return = program
    .top_level
    .body
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .any(|inst| inst.t == InstTyp::Return);

  assert!(
    !has_return,
    "top-level bodies should not synthesize Return terminators"
  );
}

