use optimize_js::analysis::annotate_program;
use optimize_js::analysis::escape::EscapeState;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::opt::optpass_scalar_replace::optpass_scalar_replace;
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
  let mut program = optimize_js::compile_source(
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

  annotate_program(&mut program);

  // Find the function that contains the object allocation (the "caller" function, not the helper).
  let idx = program
    .functions
    .iter()
    .position(|func| {
      func
        .ssa_body
        .as_ref()
        .and_then(|cfg| {
          cfg
            .bblocks
            .all()
            .flat_map(|(_, block)| block.iter())
            .find(|inst| {
              inst.t == InstTyp::Call
                && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object")
            })
            .map(|_| ())
        })
        .is_some()
    })
    .expect("expected one function to contain an object allocation");

  let cfg = program.functions[idx]
    .ssa_body
    .as_mut()
    .expect("expected SSA cfg");

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
