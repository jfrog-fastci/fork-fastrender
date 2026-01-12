use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::dom::Dom;
use optimize_js::il::inst::{Arg, Const, Inst, Purity};
use optimize_js::opt::optpass_licm::optpass_licm;

#[test]
fn licm_does_not_hoist_impure_call() {
  // Simple natural loop with an impure call inside the loop body. Even if we mark the callee as
  // `Pure`, LICM must refuse to hoist when the call's effect set is unknown / non-pure.

  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(1, 2);
  graph.connect(2, 1);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, Vec::new());
  bblocks.add(1, Vec::new());

  let mut call = Inst::call(
    None::<u32>,
    Arg::Var(0),
    Arg::Const(Const::Undefined),
    Vec::new(),
    Vec::new(),
  );
  call.meta.callee_purity = Purity::Pure;
  call.meta.effects.mark_unknown();
  bblocks.add(2, vec![call]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let dom_before = Dom::calculate(&cfg);
  let pass = optpass_licm(&mut cfg, &dom_before);
  assert!(
    !pass.changed,
    "expected LICM to make no changes (impure call should not be hoisted)"
  );

  let body = cfg.bblocks.get(2);
  assert!(
    body.iter().any(|inst| inst.t == optimize_js::il::inst::InstTyp::Call),
    "expected call to remain in loop body, got {body:?}"
  );

  let preheader = cfg
    .graph
    .parents_sorted(1)
    .into_iter()
    .find(|p| *p == 0)
    .expect("expected block 0 to remain the loop preheader");
  assert_eq!(preheader, 0);
  assert!(
    cfg.bblocks.get(0).is_empty(),
    "expected preheader to remain empty when no hoisting occurs"
  );
}
