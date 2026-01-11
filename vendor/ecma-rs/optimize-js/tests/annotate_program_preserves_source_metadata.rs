use hir_js::ExprId;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, Const, Inst, ValueTypeSummary};
use optimize_js::{OptimizationStats, Program, ProgramFunction, TopLevelMode};

fn some_type_id() -> Option<optimize_js::types::TypeId> {
  #[cfg(feature = "typed")]
  {
    Some(typecheck_ts::TypeId(123))
  }
  #[cfg(not(feature = "typed"))]
  {
    Some(())
  }
}

#[test]
fn annotate_program_preserves_lowering_metadata_fields() {
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);

  let mut inst = Inst::var_assign(0, Arg::Const(Const::Undefined));
  // Seed fields that would normally be populated during lowering/type checking.
  inst.meta.type_id = some_type_id();
  inst.meta.hir_expr = Some(ExprId(7));
  inst.meta.type_summary = Some(ValueTypeSummary::NUMBER);
  inst.meta.excludes_nullish = true;

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![inst]);
  bblocks.add(1, Vec::new());

  let mut program = Program {
    functions: Vec::new(),
    top_level: ProgramFunction {
      debug: None,
      body: Cfg {
        graph,
        bblocks,
        entry: 0,
      },
      params: Vec::new(),
      ssa_body: None,
      stats: OptimizationStats::default(),
    },
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  annotate_program(&mut program);

  let meta = &program.top_level.body.bblocks.get(0)[0].meta;
  assert_eq!(meta.type_id, some_type_id(), "expected type_id to be preserved");
  assert_eq!(meta.hir_expr, Some(ExprId(7)), "expected hir_expr to be preserved");
  assert_eq!(
    meta.type_summary,
    Some(ValueTypeSummary::NUMBER),
    "expected type_summary to be preserved"
  );
  assert!(
    meta.excludes_nullish,
    "expected excludes_nullish to be preserved"
  );
}

