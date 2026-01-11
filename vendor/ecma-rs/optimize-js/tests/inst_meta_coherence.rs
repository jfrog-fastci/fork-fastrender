use hir_js::ExprId;
use optimize_js::analysis::escape::EscapeState;
use optimize_js::analysis::purity::Purity;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{
  Arg, Const, Inst, InstTyp, OwnershipState, StringEncoding, TypeInfo,
};
use optimize_js::opt::optpass_trivial_dce::optpass_trivial_dce;
use optimize_js::ssa::phi_simplify::simplify_phis;
use optimize_js::ssa::ssa_deconstruct::deconstruct_ssa;
use optimize_js::types::ValueTypeSummary;
use optimize_js::util::counter::Counter;

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

fn non_default_type_info() -> TypeInfo {
  let mut info = TypeInfo::default();
  info.string_encoding = Some(StringEncoding::Utf8);
  info
}

#[test]
fn phi_simplify_preserves_result_metadata() {
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);

  let mut phi = Inst::phi_empty(10);
  phi.insert_phi(0, Arg::Const(Const::Bool(true)));
  phi.meta.type_id = some_type_id();
  phi.meta.hir_expr = Some(ExprId(7));
  phi.meta.type_summary = Some(ValueTypeSummary::NUMBER);
  phi.meta.excludes_nullish = true;
  phi.meta.result_type = non_default_type_info();
  phi.meta.ownership = OwnershipState::Owned;
  phi.meta.result_escape = Some(EscapeState::ReturnEscape);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, Vec::new());
  bblocks.add(1, vec![phi]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  assert!(simplify_phis(&mut cfg), "expected simplify_phis to change cfg");

  let insts = cfg.bblocks.get(1);
  assert_eq!(insts.len(), 1);
  assert_eq!(insts[0].t, InstTyp::VarAssign);

  let meta = &insts[0].meta;
  assert_eq!(meta.type_id, some_type_id());
  assert_eq!(meta.hir_expr, Some(ExprId(7)));
  assert_eq!(meta.type_summary, Some(ValueTypeSummary::NUMBER));
  assert!(meta.excludes_nullish);
  assert_eq!(meta.result_type, non_default_type_info());
  assert_eq!(meta.ownership, OwnershipState::Owned);
  assert_eq!(meta.result_escape, Some(EscapeState::ReturnEscape));
}

#[test]
fn ssa_deconstruct_propagates_phi_metadata_to_edge_copies() {
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(0, 2);
  graph.connect(1, 2);

  let mut phi = Inst::phi_empty(10);
  phi.insert_phi(0, Arg::Const(Const::Bool(true)));
  phi.insert_phi(1, Arg::Const(Const::Bool(false)));
  phi.meta.type_id = some_type_id();
  phi.meta.hir_expr = Some(ExprId(42));
  phi.meta.type_summary = Some(ValueTypeSummary::BOOLEAN);
  phi.meta.excludes_nullish = true;
  phi.meta.result_type = non_default_type_info();
  phi.meta.ownership = OwnershipState::Owned;
  phi.meta.result_escape = Some(EscapeState::ReturnEscape);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![Inst::cond_goto(Arg::Const(Const::Bool(true)), 2, 1)]);
  bblocks.add(1, Vec::new());
  bblocks.add(2, vec![phi]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };
  let mut c_label = Counter::new(3);

  deconstruct_ssa(&mut cfg, &mut c_label);

  for &label in &[3, 4] {
    let insts = cfg.bblocks.get(label);
    assert_eq!(insts.len(), 1);
    assert_eq!(insts[0].t, InstTyp::VarAssign);

    let meta = &insts[0].meta;
    assert_eq!(meta.type_id, some_type_id());
    assert_eq!(meta.hir_expr, Some(ExprId(42)));
    assert_eq!(meta.type_summary, Some(ValueTypeSummary::BOOLEAN));
    assert!(meta.excludes_nullish);
    assert_eq!(meta.result_type, non_default_type_info());
    assert_eq!(meta.ownership, OwnershipState::Owned);
    assert_eq!(meta.result_escape, Some(EscapeState::ReturnEscape));
  }
}

#[test]
fn trivial_dce_clears_call_result_metadata_when_tgt_is_removed() {
  let mut graph = CfgGraph::default();
  // Ensure label 0 exists in the graph.
  graph.connect(0, 0);

  let mut call = Inst::call(
    1,
    Arg::Builtin("f".to_string()),
    Arg::Const(Const::Undefined),
    Vec::new(),
    Vec::new(),
  );
  call.meta.effects.mark_unknown();
  // Mark as impure so Trivial DCE keeps the call but drops the unused target.
  call.meta.callee_purity = Purity::Impure;

  call.meta.type_id = some_type_id();
  call.meta.hir_expr = Some(ExprId(99));
  call.meta.type_summary = Some(ValueTypeSummary::OBJECT);
  call.meta.excludes_nullish = true;
  call.meta.result_type = non_default_type_info();
  call.meta.ownership = OwnershipState::Owned;
  call.meta.result_escape = Some(EscapeState::ReturnEscape);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![call]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  optpass_trivial_dce(&mut cfg);

  let insts = cfg.bblocks.get(0);
  assert_eq!(insts.len(), 1);
  assert_eq!(insts[0].t, InstTyp::Call);
  assert!(insts[0].tgts.is_empty());

  let meta = &insts[0].meta;
  // Effect/purity metadata should remain.
  assert!(meta.effects.unknown);
  assert_eq!(meta.callee_purity, Purity::Impure);

  // Result-value metadata should be cleared.
  assert_eq!(meta.type_id, None);
  assert_eq!(meta.hir_expr, None);
  assert_eq!(meta.type_summary, None);
  assert!(!meta.excludes_nullish);
  assert_eq!(meta.result_type, TypeInfo::default());
  assert_eq!(meta.ownership, OwnershipState::Unknown);
  assert_eq!(meta.result_escape, None);
}
