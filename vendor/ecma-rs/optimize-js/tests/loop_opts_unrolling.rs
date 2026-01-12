use optimize_js::analysis::find_loops::find_loops;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::dom::Dom;
use optimize_js::il::inst::{Arg, BinOp, Const, Inst};
use optimize_js::opt::optpass_loop_opts::optpass_loop_opts;
use parse_js::num::JsNumber;

#[test]
fn constant_trip_count_loop_is_fully_unrolled() {
  // CFG:
  //
  //   0: preheader
  //   1: header
  //       i = phi { 0: 0, 2: i_next }
  //       cond = i < 4
  //       if cond goto 2 else 3
  //   2: body/latch
  //       unknown_store("x", i)
  //       t = i + 42
  //       i_next = i + 1
  //       goto 1
  //   3: exit
  //       unknown_store("y", i)  // should observe final i == 4 after unrolling

  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(1, 2);
  graph.connect(1, 3);
  graph.connect(2, 1);
  graph.ensure_label(3);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![]);

  let mut phi = Inst::phi_empty(0);
  phi.insert_phi(0, Arg::Const(Const::Num(JsNumber(0.0))));
  phi.insert_phi(2, Arg::Var(2));
  bblocks.add(
    1,
    vec![
      phi,
      Inst::bin(
        1,
        Arg::Var(0),
        BinOp::Lt,
        Arg::Const(Const::Num(JsNumber(4.0))),
      ),
      Inst::cond_goto(Arg::Var(1), 2, 3),
    ],
  );

  bblocks.add(
    2,
    vec![
      Inst::unknown_store("x".to_string(), Arg::Var(0)),
      Inst::bin(
        4,
        Arg::Var(0),
        BinOp::Add,
        Arg::Const(Const::Num(JsNumber(42.0))),
      ),
      // Induction update.
      Inst::bin(
        2,
        Arg::Var(0),
        BinOp::Add,
        Arg::Const(Const::Num(JsNumber(1.0))),
      ),
    ],
  );

  bblocks.add(3, vec![Inst::unknown_store("y".to_string(), Arg::Var(0))]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let pass = optpass_loop_opts(&mut cfg);
  assert!(pass.changed, "expected loop opts to unroll the loop");
  assert!(pass.cfg_changed, "expected unrolling to change the CFG");

  // The loop should be gone.
  let dom = Dom::calculate(&cfg);
  let loops = find_loops(&cfg, &dom);
  assert!(
    loops.is_empty(),
    "expected unrolled CFG to have no natural loops, got {loops:?}"
  );

  // The unrolled block should contain four stores with constants 0..3.
  let unrolled_label = 4;
  let unrolled = cfg.bblocks.get(unrolled_label);
  let stores = unrolled
    .iter()
    .filter(|inst| inst.t == optimize_js::il::inst::InstTyp::UnknownStore)
    .collect::<Vec<_>>();
  assert_eq!(
    stores.len(),
    4,
    "expected 4 unrolled stores to `x`, got {stores:?}"
  );
  for (idx, inst) in stores.iter().enumerate() {
    assert_eq!(&inst.unknown, "x");
    assert!(
      matches!(
        inst.args.as_slice(),
        [Arg::Const(Const::Num(JsNumber(n)))] if *n == idx as f64
      ),
      "expected store {idx} to write constant {idx}, got {inst:?}"
    );
  }

  // The exit store should see the final i == 4.
  let exit = cfg.bblocks.get(3);
  assert!(
    matches!(
      exit[0].args.as_slice(),
      [Arg::Const(Const::Num(JsNumber(n)))] if *n == 4.0
    ),
    "expected exit store to observe i == 4, got {exit:?}"
  );
}

#[test]
fn non_constant_trip_count_loop_is_not_unrolled() {
  // Same loop shape as the unrolling test, but the upper bound is unknown.
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(1, 2);
  graph.connect(1, 3);
  graph.connect(2, 1);
  graph.ensure_label(3);

  let mut bblocks = CfgBBlocks::default();
  // Preheader loads the unknown upper bound `n`.
  bblocks.add(0, vec![Inst::unknown_load(5, "n".to_string())]);

  let mut phi = Inst::phi_empty(0);
  phi.insert_phi(0, Arg::Const(Const::Num(JsNumber(0.0))));
  phi.insert_phi(2, Arg::Var(2));
  bblocks.add(
    1,
    vec![
      phi,
      Inst::bin(1, Arg::Var(0), BinOp::Lt, Arg::Var(5)),
      Inst::cond_goto(Arg::Var(1), 2, 3),
    ],
  );

  bblocks.add(
    2,
    vec![
      Inst::unknown_store("x".to_string(), Arg::Var(0)),
      Inst::bin(
        2,
        Arg::Var(0),
        BinOp::Add,
        Arg::Const(Const::Num(JsNumber(1.0))),
      ),
    ],
  );

  bblocks.add(3, vec![]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let pass = optpass_loop_opts(&mut cfg);
  assert!(
    !pass.changed,
    "expected loop opts to leave non-constant-trip loop unchanged"
  );

  let dom = Dom::calculate(&cfg);
  let loops = find_loops(&cfg, &dom);
  assert!(
    loops.contains_key(&1),
    "expected CFG to still contain the loop header, got {loops:?}"
  );
}

#[test]
fn strength_reduction_rewrites_uses_on_loop_exit() {
  // Similar counted loop shape, but compute `t = i * 4` in the header and consume it on the exit
  // edge. Strength reduction must rewrite that use even though the exit block is outside the loop.
  //
  // Trip count is 16 (> MAX_FULL_UNROLL_TRIP_COUNT) so the loop is not unrolled.
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(1, 2);
  graph.connect(1, 3);
  graph.connect(2, 1);
  graph.ensure_label(3);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![]);

  // i = phi { 0: 0, 2: i_next }
  let mut phi = Inst::phi_empty(0);
  phi.insert_phi(0, Arg::Const(Const::Num(JsNumber(0.0))));
  phi.insert_phi(2, Arg::Var(2));

  // t = i * 4
  // cond = i < 16
  bblocks.add(
    1,
    vec![
      phi,
      Inst::bin(
        4,
        Arg::Var(0),
        BinOp::Mul,
        Arg::Const(Const::Num(JsNumber(4.0))),
      ),
      Inst::bin(
        1,
        Arg::Var(0),
        BinOp::Lt,
        Arg::Const(Const::Num(JsNumber(16.0))),
      ),
      Inst::cond_goto(Arg::Var(1), 2, 3),
    ],
  );

  // i_next = i + 1
  bblocks.add(
    2,
    vec![Inst::bin(
      2,
      Arg::Var(0),
      BinOp::Add,
      Arg::Const(Const::Num(JsNumber(1.0))),
    )],
  );

  // use t on loop exit
  bblocks.add(3, vec![Inst::unknown_store("y".to_string(), Arg::Var(4))]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let pass = optpass_loop_opts(&mut cfg);
  assert!(
    pass.changed && !pass.cfg_changed,
    "expected strength reduction to change IL but not CFG, got {pass:?}"
  );

  // Loop should still exist (we intentionally picked a large trip count).
  let dom = Dom::calculate(&cfg);
  let loops = find_loops(&cfg, &dom);
  assert!(
    loops.contains_key(&1),
    "expected CFG to still contain the loop header, got {loops:?}"
  );

  // The Mul should be eliminated and the exit use must be rewritten to the derived induction phi.
  let header = cfg.bblocks.get(1);
  assert!(
    header.iter().all(|inst| !(inst.t == optimize_js::il::inst::InstTyp::Bin && inst.bin_op == BinOp::Mul)),
    "expected Mul to be eliminated from the loop header, got {header:?}"
  );

  let sr_phi = header
    .iter()
    .filter(|inst| inst.t == optimize_js::il::inst::InstTyp::Phi)
    .map(|inst| inst.tgts[0])
    .find(|&tgt| tgt != 0)
    .expect("expected strength reduction to insert a derived phi in the header");

  let exit = cfg.bblocks.get(3);
  assert!(
    matches!(
      exit[0].args.as_slice(),
      [Arg::Var(v)] if *v == sr_phi
    ),
    "expected exit use to be rewritten to derived phi %{sr_phi}, got {exit:?}"
  );
}

#[test]
fn strength_reduction_is_disabled_outside_safe_integer_range() {
  // Ensure the strength-reduction rewrite is conservative about JS numeric semantics and does not
  // rewrite `i * const` when the result can exceed the safe integer range (2^53 - 1).
  //
  // Trip count is 16 (> MAX_FULL_UNROLL_TRIP_COUNT) so the loop is not unrolled.
  let init_i = 4_503_599_627_370_496.0; // 2^52
  let bound_i = 4_503_599_627_370_512.0; // init + 16

  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(1, 2);
  graph.connect(1, 3);
  graph.connect(2, 1);
  graph.ensure_label(3);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![]);

  // i = phi { 0: init_i, 2: i_next }
  let mut phi = Inst::phi_empty(0);
  phi.insert_phi(0, Arg::Const(Const::Num(JsNumber(init_i))));
  phi.insert_phi(2, Arg::Var(2));

  // t = i * 3  (may exceed safe integer range)
  // cond = i < bound_i
  bblocks.add(
    1,
    vec![
      phi,
      Inst::bin(
        4,
        Arg::Var(0),
        BinOp::Mul,
        Arg::Const(Const::Num(JsNumber(3.0))),
      ),
      Inst::bin(
        1,
        Arg::Var(0),
        BinOp::Lt,
        Arg::Const(Const::Num(JsNumber(bound_i))),
      ),
      Inst::cond_goto(Arg::Var(1), 2, 3),
    ],
  );

  // i_next = i + 1
  bblocks.add(
    2,
    vec![Inst::bin(
      2,
      Arg::Var(0),
      BinOp::Add,
      Arg::Const(Const::Num(JsNumber(1.0))),
    )],
  );

  bblocks.add(3, vec![]);

  let mut cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let pass = optpass_loop_opts(&mut cfg);
  assert!(
    !pass.changed,
    "expected loop opts to avoid strength reduction outside safe integer range, got {pass:?}"
  );

  let header = cfg.bblocks.get(1);
  assert!(
    header
      .iter()
      .any(|inst| inst.t == optimize_js::il::inst::InstTyp::Bin && inst.bin_op == BinOp::Mul),
    "expected Mul to remain in the loop header, got {header:?}"
  );
}
