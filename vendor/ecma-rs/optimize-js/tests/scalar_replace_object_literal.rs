use optimize_js::analysis::annotate_program;
use optimize_js::il::inst::{Arg, BinOp, InstTyp};
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

fn any_prop_ops(cfg: &optimize_js::cfg::cfg::Cfg) -> bool {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .any(|inst| {
      inst.t == InstTyp::PropAssign || (inst.t == InstTyp::Bin && inst.bin_op == BinOp::GetProp)
    })
}

#[test]
fn scalar_replace_eliminates_object_literal_and_field_ops() {
  let mut program = optimize_js::compile_source(
    r#"
      const f = () => {
        const o = { x: 1, y: 2 };
        o.y = 3;
        const a = o.x;
        const b = o.y;
        return a + b;
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
    1,
    "expected one object allocation before scalar replacement"
  );
  assert!(any_prop_ops(cfg), "expected property ops before scalar replacement");

  let result = optpass_scalar_replace(cfg);
  assert!(result.changed, "expected scalar replacement to report changes");

  assert_eq!(
    count_object_allocs(cfg),
    0,
    "expected object allocation to be removed after scalar replacement"
  );
  assert!(
    !any_prop_ops(cfg),
    "expected all GetProp/PropAssign ops to be eliminated after scalar replacement"
  );
}

