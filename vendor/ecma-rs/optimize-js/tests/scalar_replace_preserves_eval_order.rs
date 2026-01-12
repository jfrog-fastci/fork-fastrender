use optimize_js::analysis::escape::EscapeState;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use optimize_js::opt::optpass_scalar_replace::optpass_scalar_replace;
use parse_js::num::JsNumber;

fn cfg_single_block(insts: Vec<Inst>) -> Cfg {
  let mut graph = CfgGraph::default();
  graph.ensure_label(0);
  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, insts);
  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

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
    .filter(|inst| inst.t == InstTyp::ObjectLit)
    .count()
}

#[test]
fn scalar_replace_preserves_literal_initializer_eval_order() {
  // Construct a minimal SSA-style CFG that mimics lowering of:
  //   const o = { a: g(1), b: g(2), c: g(3) };
  //   return o.a;
  //
  // The initializer call results are computed *before* the allocation, and the object literal
  // encodes them as args. Scalar replacement should eliminate the allocation without changing the
  // order of the (side-effecting) calls to `g`.
  let g = 0u32;
  let call1 = 1u32;
  let call2 = 2u32;
  let call3 = 3u32;
  let obj = 4u32;
  let get_a = 5u32;

  let mut obj_lit = Inst::object_lit(
    obj,
    vec![
      Arg::Builtin("__optimize_js_object_prop".to_string()),
      Arg::Const(Const::Str("a".to_string())),
      Arg::Var(call1),
      Arg::Builtin("__optimize_js_object_prop".to_string()),
      Arg::Const(Const::Str("b".to_string())),
      Arg::Var(call2),
      Arg::Builtin("__optimize_js_object_prop".to_string()),
      Arg::Const(Const::Str("c".to_string())),
      Arg::Var(call3),
    ],
  );
  obj_lit.meta.result_escape = Some(EscapeState::NoEscape);

  let mut cfg = cfg_single_block(vec![
    Inst::call(
      call1,
      Arg::Var(g),
      Arg::Const(Const::Undefined),
      vec![Arg::Const(Const::Num(JsNumber(1.0)))],
      Vec::new(),
    ),
    Inst::call(
      call2,
      Arg::Var(g),
      Arg::Const(Const::Undefined),
      vec![Arg::Const(Const::Num(JsNumber(2.0)))],
      Vec::new(),
    ),
    Inst::call(
      call3,
      Arg::Var(g),
      Arg::Const(Const::Undefined),
      vec![Arg::Const(Const::Num(JsNumber(3.0)))],
      Vec::new(),
    ),
    obj_lit,
    Inst::bin(
      get_a,
      Arg::Var(obj),
      BinOp::GetProp,
      Arg::Const(Const::Str("a".to_string())),
    ),
    Inst::ret(Some(Arg::Var(get_a))),
  ]);

  assert_eq!(count_object_allocs(&cfg), 1, "expected allocation before replacement");

  let before_calls = collect_non_internal_call_arg0_nums(&cfg);
  assert_eq!(
    before_calls,
    vec![1.0, 2.0, 3.0],
    "expected initializer calls to be in source order before scalar replacement"
  );

  let result = optpass_scalar_replace(&mut cfg);
  assert!(result.changed, "expected scalar replacement to change cfg");

  assert_eq!(count_object_allocs(&cfg), 0, "expected allocation to be removed");

  let after_calls = collect_non_internal_call_arg0_nums(&cfg);
  assert_eq!(
    after_calls,
    vec![1.0, 2.0, 3.0],
    "expected initializer call order to be preserved after scalar replacement"
  );
}
