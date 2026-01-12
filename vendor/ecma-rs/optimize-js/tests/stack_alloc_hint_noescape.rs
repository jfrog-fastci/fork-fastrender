use optimize_js::analysis::annotate_program;
use optimize_js::analysis::escape::EscapeState;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::opt::optpass_scalar_replace::optpass_scalar_replace;
use optimize_js::CompileCfgOptions;
use optimize_js::TopLevelMode;

fn find_object_alloc<'a>(
  cfg: &'a optimize_js::cfg::cfg::Cfg,
) -> Option<&'a optimize_js::il::inst::Inst> {
  let mut labels = cfg.graph.labels_sorted();
  labels.sort_unstable();
  for label in labels {
    let block = cfg.bblocks.get(label);
    for inst in block.iter() {
      if inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object")
      {
        return Some(inst);
      }
    }
  }
  None
}

#[test]
fn noescape_allocations_get_stack_alloc_candidate_hint() {
  // `Program::compile*` runs scalar replacement on preserved SSA CFGs (`ssa_body`) as part of the
  // whole-program metadata pipeline. To test `optpass_scalar_replace` directly, keep SSA in
  // `ProgramFunction::body` so we can run the pass ourselves on the pre-scalar-replacement CFG.
  let mut program = optimize_js::compile_source_with_cfg_options(
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
    CompileCfgOptions {
      keep_ssa: true,
      // Avoid unrelated opt passes from rewriting the CFG under test.
      run_opt_passes: false,
      ..Default::default()
    },
  )
  .expect("compile");

  annotate_program(&mut program);

  // Find the function that contains the object allocation (the "caller" function, not the helper).
  let idx = program
    .functions
    .iter()
    .position(|func| {
      func.body.bblocks.all().flat_map(|(_, block)| block.iter()).any(|inst| {
        inst.t == InstTyp::Call
          && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object")
      })
    })
    .expect("expected one function to contain an object allocation");

  let cfg = &mut program.functions[idx].body;

  // The main compilation pipeline may already set `stack_alloc_candidate` on SSA bodies. Clear it so
  // this test continues to validate that `optpass_scalar_replace` can (re)apply the hint and report
  // a change.
  for (_, block) in cfg.bblocks.all_mut() {
    for inst in block.iter_mut() {
      if inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object")
      {
        inst.meta.stack_alloc_candidate = false;
      }
    }
  }

  let result = optpass_scalar_replace(cfg);
  assert!(result.changed, "expected pass to mark changes (stack alloc hint)");

  let alloc = find_object_alloc(cfg).expect("allocation should still exist");
  assert_eq!(
    alloc.meta.result_escape,
    Some(EscapeState::NoEscape),
    "expected allocation to be classified as NoEscape"
  );
  assert!(
    alloc.meta.stack_alloc_candidate,
    "expected stack_alloc_candidate hint to be set"
  );
}
