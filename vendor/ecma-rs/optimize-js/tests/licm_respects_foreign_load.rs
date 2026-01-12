use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::Inst;
use optimize_js::opt::optpass_licm::optpass_licm;
use optimize_js::symbol::semantics::SymbolId;

#[test]
fn licm_does_not_hoist_foreign_load() {
  // Foreign loads are treated as observable reads and must never be hoisted.

  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(1, 2);
  graph.connect(2, 1);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, Vec::new());
  bblocks.add(1, Vec::new());
  bblocks.add(2, vec![Inst::foreign_load(0, SymbolId(1))]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let pass = optpass_licm(&mut cfg);
  assert!(
    !pass.changed,
    "expected LICM to make no changes (foreign load must not be hoisted)"
  );

  let body = cfg.bblocks.get(2);
  assert!(
    body
      .iter()
      .any(|inst| inst.t == optimize_js::il::inst::InstTyp::ForeignLoad),
    "expected foreign load to remain in loop body, got {body:?}"
  );
}

