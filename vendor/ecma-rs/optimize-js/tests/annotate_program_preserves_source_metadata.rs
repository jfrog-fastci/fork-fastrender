use hir_js::ExprId;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, AwaitBehavior, Const, Inst, ValueTypeSummary};
use optimize_js::il::meta::{EscapeState as ValueEscapeState, IntRange as ValueIntRange, Nullability, ValueFacts};
use optimize_js::{OptimizationStats, Program, ProgramFunction, TextRange, TopLevelMode};

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
  inst.meta.span = Some(TextRange::new(2, 5));
  inst.meta.type_summary = Some(ValueTypeSummary::NUMBER);
  inst.meta.excludes_nullish = true;
  inst.meta.preserve_var_assign = true;
  inst.meta.await_behavior = Some(AwaitBehavior::MayNotYield);
  inst.meta.stack_alloc_candidate = true;
  let mut value_facts = ValueFacts::default();
  value_facts.escape = Some(ValueEscapeState::NoEscape);
  value_facts.int_range = Some(ValueIntRange {
    min: Some(1),
    max: Some(2),
  });
  value_facts.nullability = Some(Nullability::NonNullish);
  inst.meta.value = Some(value_facts);
  #[cfg(feature = "native-async-ops")]
  {
    inst.meta.await_known_resolved = true;
  }

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![inst]);
  bblocks.add(1, Vec::new());

  let cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let mut program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 10,
    functions: Vec::new(),
    top_level: ProgramFunction {
      debug: None,
      body: cfg.clone(),
      params: Vec::new(),
      // `annotate_program` resets metadata on both the SSA and SSA-deconstructed CFGs. Most real
      // programs have `ssa_body` populated, so assert both paths preserve lowering metadata.
      ssa_body: Some(cfg),
      stats: OptimizationStats::default(),
    },
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  annotate_program(&mut program);

  for (name, cfg) in [
    ("body", &program.top_level.body),
    (
      "ssa_body",
      program.top_level.ssa_body.as_ref().expect("ssa_body missing"),
    ),
  ] {
    let meta = &cfg.bblocks.get(0)[0].meta;
    assert_eq!(
      meta.type_id,
      some_type_id(),
      "expected type_id to be preserved ({name})"
    );
    assert_eq!(
      meta.hir_expr,
      Some(ExprId(7)),
      "expected hir_expr to be preserved ({name})"
    );
    assert_eq!(
      meta.span,
      Some(TextRange::new(2, 5)),
      "expected span to be preserved ({name})"
    );
    assert_eq!(
      meta.type_summary,
      Some(ValueTypeSummary::NUMBER),
      "expected type_summary to be preserved ({name})"
    );
    assert!(
      meta.excludes_nullish,
      "expected excludes_nullish to be preserved ({name})"
    );
    assert!(
      meta.preserve_var_assign,
      "expected preserve_var_assign to be preserved ({name})"
    );
    assert_eq!(
      meta.await_behavior,
      Some(AwaitBehavior::MayNotYield),
      "expected await_behavior to be preserved ({name})"
    );
    assert!(
      meta.stack_alloc_candidate,
      "expected stack_alloc_candidate to be preserved ({name})"
    );
    assert!(
      meta.value.is_some(),
      "expected value facts to be preserved ({name})"
    );
    #[cfg(feature = "native-async-ops")]
    assert!(
      meta.await_known_resolved,
      "expected await_known_resolved to be preserved ({name})"
    );
  }
}
