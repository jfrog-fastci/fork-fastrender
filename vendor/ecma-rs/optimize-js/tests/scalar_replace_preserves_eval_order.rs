use optimize_js::analysis::annotate_program;
use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::opt::optpass_scalar_replace::optpass_scalar_replace;
use optimize_js::TopLevelMode;
use parse_js::num::JsNumber;

fn collect_non_internal_call_arg0_nums(cfg: &optimize_js::cfg::cfg::Cfg) -> Vec<f64> {
  let mut out = Vec::new();
  let mut labels = cfg.graph.labels_sorted();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get(label).iter() {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (_tgt, callee, _this, args, spreads) = inst.as_call();
      if !spreads.is_empty() {
        continue;
      }
      if matches!(callee, Arg::Builtin(name) if name.starts_with("__optimize_js_")) {
        continue;
      }
      let Some(Arg::Const(Const::Num(JsNumber(n)))) = args.get(0) else {
        continue;
      };
      out.push(*n);
    }
  }
  out
}

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
fn scalar_replace_preserves_literal_initializer_eval_order() {
  let mut program = optimize_js::compile_source(
    r#"
      const f = () => {
        const o = { a: g(1), b: g(2), c: g(3) };
        return o.a;
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

  assert_eq!(count_object_allocs(cfg), 1, "expected allocation before replacement");

  let before_calls = collect_non_internal_call_arg0_nums(cfg);
  assert_eq!(
    before_calls,
    vec![1.0, 2.0, 3.0],
    "expected initializer calls to be in source order before scalar replacement"
  );

  let result = optpass_scalar_replace(cfg);
  assert!(result.changed, "expected scalar replacement to change cfg");

  assert_eq!(count_object_allocs(cfg), 0, "expected allocation to be removed");

  let after_calls = collect_non_internal_call_arg0_nums(cfg);
  assert_eq!(
    after_calls,
    vec![1.0, 2.0, 3.0],
    "expected initializer call order to be preserved after scalar replacement"
  );
}

