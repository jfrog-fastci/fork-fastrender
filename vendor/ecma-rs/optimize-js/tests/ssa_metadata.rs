use optimize_js::analysis::escape::EscapeState;
use optimize_js::compile_source;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::TopLevelMode;

fn find_object_alloc<'a>(
  cfg: &'a optimize_js::cfg::cfg::Cfg,
) -> Option<&'a optimize_js::il::inst::Inst> {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find(|inst| {
      inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object")
    })
}

#[test]
fn ssa_cfg_is_annotated_with_escape_metadata() {
  let program = compile_source(
    "let f = () => { let a = {}; return a; };",
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  assert_eq!(
    program.functions.len(),
    1,
    "expected exactly one nested function to be compiled"
  );

  let func = &program.functions[0];
  assert!(func.cfg_ssa().is_some(), "expected SSA body to be preserved");

  let ssa_cfg = func.cfg_ssa().unwrap();
  let alloc = find_object_alloc(ssa_cfg).expect("object allocation call should exist in SSA cfg");
  assert_eq!(
    alloc.meta.result_escape,
    Some(EscapeState::ReturnEscape),
    "expected returned object allocation to be marked as ReturnEscape"
  );

  // Policy: deconstructed CFG is not annotated; metadata lives on `ssa_body`.
  let body_alloc =
    find_object_alloc(func.cfg_deconstructed()).expect("object allocation call should exist in body");
  assert_eq!(
    body_alloc.meta.result_escape,
    None,
    "expected deconstructed cfg to be unannotated"
  );
}
