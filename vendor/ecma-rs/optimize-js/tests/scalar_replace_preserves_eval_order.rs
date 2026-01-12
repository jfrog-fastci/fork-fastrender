use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::il::inst::{BinOp, Inst};
use optimize_js::opt::optpass_scalar_replace::optpass_scalar_replace;
use optimize_js::TopLevelMode;
use parse_js::num::JsNumber;
use optimize_js::{OptimizationStats, Program, ProgramFunction};

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
  // Build a minimal SSA CFG for:
  //   const o = { a: g(1), b: g(2), c: g(3) };
  //   return o.a;
  //
  // This is constructed directly rather than using `compile_source`, because the main compilation
  // pipeline may already run scalar replacement on SSA bodies.
  let mut graph = CfgGraph::default();
  graph.ensure_label(0);
  let mut bblocks = CfgBBlocks::default();
  bblocks.add(
    0,
    vec![
      Inst::call(
        1,
        Arg::Builtin("g".to_string()),
        Arg::Const(Const::Undefined),
        vec![Arg::Const(Const::Num(JsNumber(1.0)))],
        vec![],
      ),
      Inst::call(
        2,
        Arg::Builtin("g".to_string()),
        Arg::Const(Const::Undefined),
        vec![Arg::Const(Const::Num(JsNumber(2.0)))],
        vec![],
      ),
      Inst::call(
        3,
        Arg::Builtin("g".to_string()),
        Arg::Const(Const::Undefined),
        vec![Arg::Const(Const::Num(JsNumber(3.0)))],
        vec![],
      ),
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        vec![
          Arg::Builtin("__optimize_js_object_prop".to_string()),
          Arg::Const(Const::Str("a".to_string())),
          Arg::Var(1),
          Arg::Builtin("__optimize_js_object_prop".to_string()),
          Arg::Const(Const::Str("b".to_string())),
          Arg::Var(2),
          Arg::Builtin("__optimize_js_object_prop".to_string()),
          Arg::Const(Const::Str("c".to_string())),
          Arg::Var(3),
        ],
        vec![],
      ),
      Inst::bin(
        4,
        Arg::Var(0),
        BinOp::GetProp,
        Arg::Const(Const::Str("a".to_string())),
      ),
      Inst::ret(Some(Arg::Var(4))),
    ],
  );
  let cfg_fn = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let mut top_graph = CfgGraph::default();
  top_graph.ensure_label(0);
  let mut top_blocks = CfgBBlocks::default();
  top_blocks.add(0, vec![Inst::ret(None)]);
  let cfg_top = Cfg {
    graph: top_graph,
    bblocks: top_blocks,
    entry: 0,
  };

  let mut program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 0,
    functions: vec![ProgramFunction {
      debug: None,
      meta: Default::default(),
      body: cfg_fn.clone(),
      params: Vec::new(),
      ssa_body: Some(cfg_fn),
      stats: OptimizationStats::default(),
    }],
    top_level: ProgramFunction {
      debug: None,
      meta: Default::default(),
      body: cfg_top.clone(),
      params: Vec::new(),
      ssa_body: Some(cfg_top),
      stats: OptimizationStats::default(),
    },
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  annotate_program(&mut program);

  assert_eq!(program.functions.len(), 1, "expected one nested function");
  let func = &mut program.functions[0];
  let cfg = &mut func.body;

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
