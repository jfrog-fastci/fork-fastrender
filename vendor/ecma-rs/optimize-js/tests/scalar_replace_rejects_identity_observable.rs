use optimize_js::analysis::annotate_program;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::opt::optpass_scalar_replace::optpass_scalar_replace;
use optimize_js::TopLevelMode;

fn count_object_allocs(cfg: &optimize_js::cfg::cfg::Cfg) -> usize {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .filter(|inst| {
      inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object")
    })
    .count()
}

#[test]
fn scalar_replace_rejects_strict_equality_identity_observation() {
  let mut program = optimize_js::compile_source(
    r#"
      const f = () => {
        const a = { x: 1 };
        const b = { x: 1 };
        if (a === b) return 1;
        return 0;
      };
      f();
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  annotate_program(&mut program);

  assert_eq!(program.functions.len(), 1, "expected one nested function");
  let func = &mut program.functions[0];
  let cfg = func
    .ssa_body
    .as_mut()
    .expect("expected SSA body to be preserved for analyses");

  assert_eq!(
    count_object_allocs(cfg),
    2,
    "expected two object allocations before scalar replacement"
  );

  // Object identity is observed via `===`, so scalar replacement must not run.
  let _result = optpass_scalar_replace(cfg);

  assert_eq!(
    count_object_allocs(cfg),
    2,
    "expected allocations to remain when identity is observable"
  );
}

