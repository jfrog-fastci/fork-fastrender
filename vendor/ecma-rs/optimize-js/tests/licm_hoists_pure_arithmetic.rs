use optimize_js::analysis::find_loops::find_loops;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::dom::Dom;
use optimize_js::il::inst::{Arg, BinOp, Const, Inst};
use optimize_js::opt::optpass_licm::optpass_licm;
use parse_js::num::JsNumber;

#[test]
fn licm_hoists_pure_arithmetic_to_loop_preheader() {
  // CFG:
  //
  //   0: a=1; b=2; if (true) goto 1 else 4
  //   1: i = phi { 0: 0, 2: i_next }
  //   2: c = a + b        // loop invariant, should be hoisted
  //      i_next = i + c   // not invariant (depends on phi)
  //      goto 1
  //   4: exit
  //
  // Block 0 is not a canonical preheader (it has two successors), so LICM must insert a new
  // preheader block on the 0 -> 1 edge and hoist `c = a + b` into it.

  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(0, 4);
  graph.connect(1, 2);
  graph.connect(2, 1);
  graph.ensure_label(4);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(
    0,
    vec![
      Inst::var_assign(0, Arg::Const(Const::Num(JsNumber(1.0)))),
      Inst::var_assign(1, Arg::Const(Const::Num(JsNumber(2.0)))),
      Inst::cond_goto(Arg::Const(Const::Bool(true)), 1, 4),
    ],
  );

  let mut phi = Inst::phi_empty(2);
  phi.insert_phi(0, Arg::Const(Const::Num(JsNumber(0.0))));
  phi.insert_phi(2, Arg::Var(4));
  bblocks.add(1, vec![phi]);

  bblocks.add(
    2,
    vec![
      Inst::bin(3, Arg::Var(0), BinOp::Add, Arg::Var(1)),
      Inst::bin(4, Arg::Var(2), BinOp::Add, Arg::Var(3)),
    ],
  );

  bblocks.add(4, Vec::new());

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let dom_before = Dom::calculate(&cfg);
  let pass = optpass_licm(&mut cfg, &dom_before);
  assert!(pass.changed, "expected LICM to hoist at least one instruction");
  assert!(pass.cfg_changed, "expected LICM to insert a loop preheader block");

  let dom = Dom::calculate(&cfg);
  let loops = find_loops(&cfg, &dom);
  let nodes = loops.get(&1).expect("expected a loop with header 1");
  let preheader = cfg
    .graph
    .parents_sorted(1)
    .into_iter()
    .find(|p| !nodes.contains(p))
    .expect("expected loop header to have an outside predecessor");

  assert_eq!(
    cfg.graph.children_sorted(preheader),
    vec![1],
    "preheader should have exactly one successor: the loop header"
  );

  let pre_bb = cfg.bblocks.get(preheader);
  assert!(
    pre_bb.iter().any(|inst| inst.tgts == vec![3]),
    "expected the invariant `c = a + b` instruction (tgt %3) to be hoisted into the preheader; preheader={pre_bb:?}"
  );

  let body_bb = cfg.bblocks.get(2);
  assert!(
    body_bb.iter().all(|inst| inst.tgts != vec![3]),
    "expected `%3 = a + b` to be removed from the loop body; body={body_bb:?}"
  );
  assert!(
    body_bb.iter().any(|inst| inst.tgts == vec![4]),
    "expected the loop-carried update to remain in the loop body; body={body_bb:?}"
  );
}
