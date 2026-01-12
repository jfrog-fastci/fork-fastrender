#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::driver::annotate_program;
use optimize_js::il::inst::InstTyp;
use optimize_js::TopLevelMode;

#[test]
fn try_catch_does_not_break_analyses() {
  let src = r#"
    export const f = (g) => {
      try {
        g();
        throw 1;
      } catch (e) {
        return e;
      } finally {
        g();
      }
    };
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  let _analyses = annotate_program(&mut program);

  // Ensure the exception constructs survive lowering/analysis and do not cause panics.
  assert_eq!(program.functions.len(), 1);
  let func = program.functions.get(0).expect("expected function");
  let cfg = func.analyzed_cfg();
  let has_catch = cfg
    .bblocks
    .all()
    .any(|(_, block)| block.iter().any(|inst| inst.t == InstTyp::Catch));
  assert!(has_catch, "expected lowered CFG to contain a Catch instruction");
}
