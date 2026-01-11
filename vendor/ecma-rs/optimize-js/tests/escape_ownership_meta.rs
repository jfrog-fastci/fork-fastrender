#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::escape::EscapeState;
use optimize_js::analysis::ownership::UseMode;
use optimize_js::analysis::annotate_program;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::TopLevelMode;

#[test]
fn escape_and_ownership_metadata_is_attached() {
  let src = r#"
    const f = () => {
      let a = {};
      return a;
    };
    void f;
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  annotate_program(&mut program);
  
  let func = program.functions.get(0).expect("expected one nested function");
  let cfg = func.analyzed_cfg();

  let mut alloc_escape = None;
  let mut return_use = None;
  let mut labels = cfg.graph.labels_sorted();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::Call {
        let (tgt, callee, _this, _args, _spreads) = inst.as_call();
        if matches!(callee, Arg::Builtin(name) if name == "__optimize_js_object") {
          assert!(tgt.is_some(), "object literal call should produce a value");
          alloc_escape = Some(inst.meta.result_escape);
        }
      }

      if inst.t == InstTyp::Return {
        assert_eq!(
          inst.meta.arg_use_modes.len(),
          inst.args.len(),
          "arg_use_modes must be aligned with Inst::args"
        );
        return_use = Some(inst.meta.arg_use_modes[0]);
      }
    }
  }

  assert_eq!(
    alloc_escape.flatten(),
    Some(EscapeState::ReturnEscape),
    "expected returned object allocation to be classified as ReturnEscape"
  );
  assert_eq!(
    return_use,
    Some(UseMode::Consume),
    "expected return to consume its argument"
  );
}
