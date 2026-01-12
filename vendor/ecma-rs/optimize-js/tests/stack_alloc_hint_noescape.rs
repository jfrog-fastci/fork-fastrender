use optimize_js::analysis::annotate_escape_and_ownership;
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
      if inst.t == InstTyp::ObjectLit
        || (inst.t == InstTyp::Call
          && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object"))
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
  let program = optimize_js::compile_source_with_cfg_options(
    r#"
      function caller(k) {
        const o = {};
        // Use the object through a non-constant property key so scalar replacement rejects it,
        // while still keeping the allocation local (`NoEscape`).
        o[k] = 1;
        return o[k];
      }
      caller("x");
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

  assert_eq!(program.functions.len(), 1, "expected one nested function");
  let func = &program.functions[0];

  // `compile_source` runs scalar replacement eagerly on `ssa_body`. For this test, run the pass
  // on a fresh CFG clone with escape metadata re-annotated so we can assert the hint is produced.
  let mut cfg = func.body.clone();
  annotate_escape_and_ownership(&mut cfg, &func.params);

  // The main compilation pipeline may already set `stack_alloc_candidate` on SSA bodies. Clear it so
  // this test continues to validate that `optpass_scalar_replace` can (re)apply the hint and report
  // a change.
  for (_, block) in cfg.bblocks.all_mut() {
    for inst in block.iter_mut() {
      if inst.t == InstTyp::ObjectLit
        || (inst.t == InstTyp::Call
          && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object"))
      {
        inst.meta.stack_alloc_candidate = false;
      }
    }
  }

  let result = optpass_scalar_replace(&mut cfg);
  assert!(result.changed, "expected pass to mark changes (stack alloc hint)");

  let alloc = find_object_alloc(&cfg).expect("allocation should still exist");
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
