use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use optimize_js::opt::optpass_scalar_replace::optpass_scalar_replace;
use optimize_js::TopLevelMode;
use optimize_js::{OptimizationStats, Program, ProgramFunction};

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
  // Build a minimal SSA CFG for:
  //   const o = { x: 1, y: 2 };
  //   o.y = 3;
  //   const a = o.x;
  //   const b = o.y;
  //   return a + b;
  //
  // This is constructed directly rather than using `compile_source`, because the main compilation
  // pipeline may already run scalar replacement on SSA bodies.
  let mut graph = CfgGraph::default();
  graph.ensure_label(0);
  let mut bblocks = CfgBBlocks::default();
  bblocks.add(
    0,
    vec![
      // o = { x: 1, y: 2 }
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        vec![
          Arg::Builtin("__optimize_js_object_prop".to_string()),
          Arg::Const(Const::Str("x".to_string())),
          Arg::Const(Const::Num(parse_js::num::JsNumber(1.0))),
          Arg::Builtin("__optimize_js_object_prop".to_string()),
          Arg::Const(Const::Str("y".to_string())),
          Arg::Const(Const::Num(parse_js::num::JsNumber(2.0))),
        ],
        vec![],
      ),
      // o.y = 3
      Inst::prop_assign(
        Arg::Var(0),
        Arg::Const(Const::Str("y".to_string())),
        Arg::Const(Const::Num(parse_js::num::JsNumber(3.0))),
      ),
      // a = o.x
      Inst::bin(
        1,
        Arg::Var(0),
        BinOp::GetProp,
        Arg::Const(Const::Str("x".to_string())),
      ),
      // b = o.y
      Inst::bin(
        2,
        Arg::Var(0),
        BinOp::GetProp,
        Arg::Const(Const::Str("y".to_string())),
      ),
      // a + b
      Inst::bin(3, Arg::Var(1), BinOp::Add, Arg::Var(2)),
      Inst::ret(Some(Arg::Var(3))),
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

  assert_eq!(program.functions.len(), 1);
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
