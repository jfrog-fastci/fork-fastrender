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

fn caller_fn<'a>(program: &'a optimize_js::Program) -> &'a optimize_js::ProgramFunction {
  program
    .functions
    .iter()
    .find(|func| {
      func.cfg_ssa().and_then(find_object_alloc).is_some()
    })
    .expect("expected one function to contain an object allocation")
}

fn object_alloc_escape_states(cfg: &optimize_js::cfg::cfg::Cfg) -> Vec<Option<EscapeState>> {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .filter(|inst| {
      inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object")
    })
    .map(|inst| inst.meta.result_escape)
    .collect()
}

#[test]
fn ssa_escape_does_not_force_global_escape_for_non_capturing_helper_call() {
  let program = compile_source(
    r#"
      function caller() {
        const o = {};
        ((x) => x)(o);
        return 0;
      }
      caller();
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  let func = caller_fn(&program);
  let ssa_cfg = func.cfg_ssa().expect("ssa_body should be populated");
  let alloc = find_object_alloc(ssa_cfg).expect("allocation should exist");
  assert_eq!(
    alloc.meta.result_escape,
    Some(EscapeState::NoEscape),
    "expected allocation passed to helper(o) to remain NoEscape"
  );
}

#[test]
fn ssa_escape_propagates_return_aliasing_through_helper_call() {
  let program = compile_source(
    r#"
      function caller() {
        const o = {};
        return ((x) => x)(o);
      }
      caller();
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  let func = caller_fn(&program);
  let ssa_cfg = func.cfg_ssa().expect("ssa_body should be populated");
  let alloc = find_object_alloc(ssa_cfg).expect("allocation should exist");
  assert_eq!(
    alloc.meta.result_escape,
    Some(EscapeState::ReturnEscape),
    "expected allocation returned via helper(o) to be ReturnEscape"
  );
}

#[test]
fn ssa_escape_marks_global_escape_when_helper_stores_to_outer_scope() {
  let program = compile_source(
    r#"
      let g;
      function caller() {
        const o = {};
        ((x) => { g = x; return x; })(o);
        return 0;
      }
      caller();
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  let func = caller_fn(&program);
  let ssa_cfg = func.cfg_ssa().expect("ssa_body should be populated");
  let alloc = find_object_alloc(ssa_cfg).expect("allocation should exist");
  assert_eq!(
    alloc.meta.result_escape,
    Some(EscapeState::GlobalEscape),
    "expected allocation stored to outer variable to be GlobalEscape"
  );
}

#[test]
fn ssa_escape_propagates_receiver_escape_through_helper_arg_escape() {
  let program = compile_source(
    r#"
      function caller() {
        const x = {};
        const y = {};
        return ((a, b) => { b.p = a; return b; })(x, y);
      }
      caller();
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  let func = caller_fn(&program);
  let ssa_cfg = func.ssa_body.as_ref().expect("ssa_body should be populated");
  let allocs = object_alloc_escape_states(ssa_cfg);
  assert_eq!(allocs.len(), 2, "expected exactly two object allocations");
  assert!(
    allocs
      .iter()
      .all(|s| *s == Some(EscapeState::ReturnEscape)),
    "expected both allocations to be ReturnEscape, got {allocs:?}"
  );
}
