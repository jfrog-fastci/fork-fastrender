use optimize_js::compile_source;
use optimize_js::il::inst::{Arg, ArgUseMode, InstTyp};
use optimize_js::TopLevelMode;

fn find_var_return<'a>(
  cfg: &'a optimize_js::cfg::cfg::Cfg,
) -> Option<&'a optimize_js::il::inst::Inst> {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find(|inst| inst.t == InstTyp::Return && matches!(inst.args.get(0), Some(Arg::Var(_))))
}

#[test]
fn ssa_cfg_is_annotated_with_consumption_metadata() {
  let program = compile_source(
    r#"
      function f() {
        const o = {};
        return o;
      }
      f();
    "#,
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
  let ssa_cfg = func.cfg_ssa().expect("expected SSA body to be preserved");
  let ret = find_var_return(ssa_cfg).expect("return should exist in SSA cfg");
  assert_eq!(
    ret.meta.arg_use_modes,
    vec![ArgUseMode::Consume],
    "expected SSA return value to be marked as consumed"
  );

  // Policy: deconstructed CFG is not annotated; metadata lives on `ssa_body`.
  let body_ret = find_var_return(func.cfg_deconstructed()).expect("return should exist in body cfg");
  assert!(
    body_ret.meta.arg_use_modes.is_empty(),
    "expected deconstructed cfg to be unannotated"
  );
}

