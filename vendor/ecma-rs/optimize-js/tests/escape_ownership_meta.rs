#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::escape::EscapeState;
use optimize_js::analysis::ownership::UseMode;
use optimize_js::analysis::annotate_program;
use optimize_js::il::inst::InstTyp;
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
  // `annotate_program` attaches ownership/escape + consumption metadata to the analyzed CFG (SSA
  // form when available), not necessarily to the SSA-deconstructed `body`.
  let cfg = func.analyzed_cfg();

  let mut alloc_escape = None;
  let mut return_use = None;

  let mut labels = cfg.graph.labels_sorted();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::ObjectLit {
        let (tgt, _args) = inst.as_object_lit();
        assert!(tgt.is_some(), "object literal inst should produce a value");
        alloc_escape = Some(inst.meta.result_escape);
      }

      if inst.t == InstTyp::Return {
        return_use = Some(inst.meta.arg_use_mode(0));
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
