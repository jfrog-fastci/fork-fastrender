use super::dataflow::{
  AnalysisBoundary, BlockState, DataFlowAnalysis, DataFlowResult, Direction,
  ResolvedAnalysisBoundary,
};
use super::dataflow_edge::{run_forward_edge_dataflow, ForwardEdgeDataFlowAnalysis};
use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use crate::il::inst::{Arg, Inst, InstTyp};
use ahash::HashMap;
use std::collections::BTreeSet;

#[derive(Default)]
struct ForwardCollectLabels;

#[derive(Default)]
struct BackwardCollectLabels;

fn union_sets(states: &[(u32, &BTreeSet<u32>)]) -> BTreeSet<u32> {
  states
    .iter()
    .flat_map(|(_, set)| set.iter().copied())
    .collect()
}

impl DataFlowAnalysis for ForwardCollectLabels {
  type State = BTreeSet<u32>;

  const DIRECTION: Direction = Direction::Forward;

  fn bottom(&self, _cfg: &Cfg) -> Self::State {
    BTreeSet::new()
  }

  fn meet(&mut self, states: &[(u32, &Self::State)]) -> Self::State {
    union_sets(states)
  }

  fn apply_to_block(&mut self, label: u32, _block: &[Inst], state: &Self::State) -> Self::State {
    let mut next = state.clone();
    next.insert(label);
    next
  }

  fn apply_to_instruction(
    &mut self,
    _label: u32,
    _inst_idx: usize,
    _inst: &Inst,
    _state: &mut Self::State,
  ) {
  }
}

impl DataFlowAnalysis for BackwardCollectLabels {
  type State = BTreeSet<u32>;

  const DIRECTION: Direction = Direction::Backward;

  fn bottom(&self, _cfg: &Cfg) -> Self::State {
    BTreeSet::new()
  }

  fn meet(&mut self, states: &[(u32, &Self::State)]) -> Self::State {
    union_sets(states)
  }

  fn apply_to_block(&mut self, label: u32, _block: &[Inst], state: &Self::State) -> Self::State {
    let mut next = state.clone();
    next.insert(label);
    next
  }

  fn apply_to_instruction(
    &mut self,
    _label: u32,
    _inst_idx: usize,
    _inst: &Inst,
    _state: &mut Self::State,
  ) {
  }
}

fn cfg(labels: &[u32], edges: &[(u32, u32)]) -> Cfg {
  let mut graph = CfgGraph::default();
  for &(from, to) in edges {
    graph.connect(from, to);
  }
  let mut bblocks = CfgBBlocks::default();
  for &label in labels {
    bblocks.add(label, Vec::new());
  }
  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn cfg_with_bblocks(bblocks: Vec<(u32, Vec<Inst>)>, edges: &[(u32, u32)]) -> Cfg {
  let mut graph = CfgGraph::default();
  for &(from, to) in edges {
    graph.connect(from, to);
  }
  let mut cfg_bblocks = CfgBBlocks::default();
  for (label, block) in bblocks {
    cfg_bblocks.add(label, block);
  }
  Cfg {
    graph,
    bblocks: cfg_bblocks,
    entry: 0,
  }
}

fn assert_exit(states: &HashMap<u32, BlockState<BTreeSet<u32>>>, label: u32, expected: &[u32]) {
  let expected_set = expected.iter().copied().collect::<BTreeSet<_>>();
  assert_eq!(
    states.get(&label).map(|s| &s.exit),
    Some(&expected_set),
    "unexpected exit state for block {}",
    label
  );
}

fn assert_entry(states: &HashMap<u32, BlockState<BTreeSet<u32>>>, label: u32, expected: &[u32]) {
  let expected_set = expected.iter().copied().collect::<BTreeSet<_>>();
  assert_eq!(
    states.get(&label).map(|s| &s.entry),
    Some(&expected_set),
    "unexpected entry state for block {}",
    label
  );
}

fn forward(cfg: &Cfg, boundary: AnalysisBoundary) -> DataFlowResult<BTreeSet<u32>> {
  ForwardCollectLabels::default().analyze(cfg, boundary)
}

fn backward(cfg: &Cfg, boundary: AnalysisBoundary) -> DataFlowResult<BTreeSet<u32>> {
  BackwardCollectLabels::default().analyze(cfg, boundary)
}

#[derive(Default)]
struct BranchMarkingEdgeAnalysis;

impl ForwardEdgeDataFlowAnalysis for BranchMarkingEdgeAnalysis {
  type State = BTreeSet<u32>;

  fn bottom(&self, _cfg: &Cfg) -> Self::State {
    BTreeSet::new()
  }

  fn meet(&mut self, states: &[(u32, &Self::State)]) -> Self::State {
    union_sets(states)
  }

  fn apply_to_instruction(
    &mut self,
    _label: u32,
    _inst_idx: usize,
    _inst: &Inst,
    _state: &mut Self::State,
  ) {
  }

  fn apply_edge(
    &mut self,
    _pred: u32,
    succ: u32,
    pred_block: &[Inst],
    state_at_end_of_pred: &Self::State,
    _cfg: &Cfg,
  ) -> Self::State {
    let mut out = state_at_end_of_pred.clone();
    if let Some(inst) = pred_block.last() {
      if inst.t == InstTyp::CondGoto {
        let (_, t, f) = inst.as_cond_goto();
        if succ == t {
          out.insert(1);
        } else if succ == f {
          out.insert(2);
        }
      }
    }
    out
  }
}

fn set(vals: &[u32]) -> BTreeSet<u32> {
  vals.iter().copied().collect()
}

#[test]
fn diamond_two_exits() {
  let cfg = cfg(&[0, 1, 2, 3, 4], &[(0, 1), (0, 2), (1, 3), (2, 4)]);

  let forward_result = forward(&cfg, AnalysisBoundary::Entry(0));
  assert_exit(&forward_result.blocks, 0, &[0]);
  assert_exit(&forward_result.blocks, 1, &[0, 1]);
  assert_exit(&forward_result.blocks, 2, &[0, 2]);
  assert_exit(&forward_result.blocks, 3, &[0, 1, 3]);
  assert_exit(&forward_result.blocks, 4, &[0, 2, 4]);

  let backward_result = backward(&cfg, AnalysisBoundary::VirtualExit);
  let exit_label = match &backward_result.boundary {
    ResolvedAnalysisBoundary::VirtualExit {
      label,
      predecessors,
    } => {
      assert_eq!(predecessors, &vec![3, 4]);
      *label
    }
    ResolvedAnalysisBoundary::Entry(label) => panic!("expected virtual exit, got entry {label}"),
  };
  assert_entry(&backward_result.blocks, exit_label, &[exit_label]);
  assert_entry(&backward_result.blocks, 3, &[exit_label, 3]);
  assert_entry(&backward_result.blocks, 4, &[exit_label, 4]);
  assert_entry(&backward_result.blocks, 1, &[exit_label, 1, 3]);
  assert_entry(&backward_result.blocks, 2, &[exit_label, 2, 4]);
  assert_entry(&backward_result.blocks, 0, &[exit_label, 0, 1, 2, 3, 4]);
}

#[test]
fn diamond_single_exit() {
  let cfg = cfg(&[0, 1, 2, 3], &[(0, 1), (0, 2), (1, 3), (2, 3)]);

  let forward_result = forward(&cfg, AnalysisBoundary::Entry(0));
  assert_exit(&forward_result.blocks, 0, &[0]);
  assert_exit(&forward_result.blocks, 1, &[0, 1]);
  assert_exit(&forward_result.blocks, 2, &[0, 2]);
  assert_exit(&forward_result.blocks, 3, &[0, 1, 2, 3]);

  let backward_result = backward(&cfg, AnalysisBoundary::VirtualExit);
  let exit_label = match &backward_result.boundary {
    ResolvedAnalysisBoundary::VirtualExit {
      label,
      predecessors,
    } => {
      assert_eq!(predecessors, &vec![3]);
      *label
    }
    ResolvedAnalysisBoundary::Entry(label) => panic!("expected virtual exit, got entry {label}"),
  };
  assert_entry(&backward_result.blocks, exit_label, &[exit_label]);
  assert_entry(&backward_result.blocks, 3, &[exit_label, 3]);
  assert_entry(&backward_result.blocks, 1, &[exit_label, 1, 3]);
  assert_entry(&backward_result.blocks, 2, &[exit_label, 2, 3]);
  assert_entry(&backward_result.blocks, 0, &[exit_label, 0, 1, 2, 3]);
}

#[test]
fn infinite_loop_virtual_exit_from_sink_scc() {
  let cfg = cfg(&[0, 1, 2], &[(0, 1), (1, 2), (2, 1)]);

  let forward_result = forward(&cfg, AnalysisBoundary::Entry(0));
  assert_exit(&forward_result.blocks, 0, &[0]);
  assert_exit(&forward_result.blocks, 1, &[0, 1, 2]);
  assert_exit(&forward_result.blocks, 2, &[0, 1, 2]);

  let backward_result = backward(&cfg, AnalysisBoundary::VirtualExit);
  let exit_label = match &backward_result.boundary {
    ResolvedAnalysisBoundary::VirtualExit {
      label,
      predecessors,
    } => {
      assert_eq!(predecessors, &vec![1, 2]);
      *label
    }
    ResolvedAnalysisBoundary::Entry(label) => panic!("expected virtual exit, got entry {label}"),
  };
  assert_entry(&backward_result.blocks, exit_label, &[exit_label]);
  assert_entry(&backward_result.blocks, 1, &[exit_label, 1, 2]);
  assert_entry(&backward_result.blocks, 2, &[exit_label, 1, 2]);
  assert_entry(&backward_result.blocks, 0, &[exit_label, 0, 1, 2]);
}

#[test]
fn infinite_loop_virtual_exit_ignores_unreachable_terminal_block() {
  // Block 3 is unreachable from entry, but present in the graph as a disconnected node.
  // Virtual-exit construction should still seed from the sink SCC of the reachable loop.
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(1, 2);
  graph.connect(2, 1);
  graph.ensure_label(3);

  let mut bblocks = CfgBBlocks::default();
  for label in [0u32, 1, 2, 3] {
    bblocks.add(label, Vec::new());
  }
  let cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let backward_result = backward(&cfg, AnalysisBoundary::VirtualExit);
  match &backward_result.boundary {
    ResolvedAnalysisBoundary::VirtualExit { predecessors, .. } => {
      assert_eq!(predecessors, &vec![1, 2]);
    }
    ResolvedAnalysisBoundary::Entry(label) => panic!("expected virtual exit, got entry {label}"),
  };
}

#[test]
fn virtual_exit_label_does_not_collide_with_unconnected_bblock() {
  let cfg = cfg(&[0, 1, 2], &[(0, 1)]);
  let backward_result = backward(&cfg, AnalysisBoundary::VirtualExit);
  match &backward_result.boundary {
    ResolvedAnalysisBoundary::VirtualExit { label, .. } => {
      assert!(
        cfg.bblocks.maybe_get(*label).is_none(),
        "virtual exit label {label} collides with an existing basic block",
      );
    }
    ResolvedAnalysisBoundary::Entry(label) => panic!("expected virtual exit, got entry {label}"),
  }
}

#[test]
fn unreachable_blocks_start_at_bottom() {
  let cfg = cfg(&[0, 1, 2, 3], &[(0, 1), (1, 2)]);
  let forward_result = forward(&cfg, AnalysisBoundary::Entry(0));
  assert_entry(&forward_result.blocks, 3, &[]);
  assert_exit(&forward_result.blocks, 3, &[]);
}

#[test]
fn deterministic_across_edge_ordering() {
  let cfg1 = cfg(&[0, 1, 2, 3], &[(0, 1), (1, 2), (1, 3), (0, 2)]);
  let cfg2 = cfg(&[0, 1, 2, 3], &[(1, 3), (0, 2), (1, 2), (0, 1)]);

  let f1 = forward(&cfg1, AnalysisBoundary::Entry(0));
  let f2 = forward(&cfg2, AnalysisBoundary::Entry(0));
  assert_eq!(f1.blocks, f2.blocks);

  let b1 = backward(&cfg1, AnalysisBoundary::VirtualExit);
  let b2 = backward(&cfg2, AnalysisBoundary::VirtualExit);
  assert_eq!(b1.blocks, b2.blocks);
}

#[test]
fn loop_propagation_converges() {
  let cfg = cfg(&[0, 1, 2, 3], &[(0, 1), (1, 2), (2, 1), (1, 3)]);

  let result = forward(&cfg, AnalysisBoundary::Entry(0));
  assert_exit(&result.blocks, 0, &[0]);
  assert_exit(&result.blocks, 1, &[0, 1, 2]);
  assert_exit(&result.blocks, 2, &[0, 1, 2]);
  assert_exit(&result.blocks, 3, &[0, 1, 2, 3]);
}

#[test]
fn forward_edge_dataflow_refines_per_successor_state() {
  let cfg = cfg_with_bblocks(
    vec![
      (0, vec![Inst::cond_goto(Arg::Var(0), 1, 2)]),
      (1, vec![]),
      (2, vec![]),
      (3, vec![]),
    ],
    &[(0, 1), (0, 2), (1, 3), (2, 3)],
  );

  let result = run_forward_edge_dataflow(&mut BranchMarkingEdgeAnalysis::default(), &cfg, 0);
  assert_eq!(result.block_exit.get(&0), Some(&set(&[])));
  assert_eq!(result.edge_out.get(&(0, 1)), Some(&set(&[1])));
  assert_eq!(result.edge_out.get(&(0, 2)), Some(&set(&[2])));
  assert_eq!(result.block_entry.get(&1), Some(&set(&[1])));
  assert_eq!(result.block_entry.get(&2), Some(&set(&[2])));
  assert_eq!(result.block_entry.get(&3), Some(&set(&[1, 2])));
}

#[test]
fn forward_edge_dataflow_deterministic_across_edge_ordering() {
  let cfg1 = cfg_with_bblocks(
    vec![
      (0, vec![Inst::cond_goto(Arg::Var(0), 1, 2)]),
      (1, vec![]),
      (2, vec![]),
      (3, vec![]),
    ],
    &[(0, 1), (0, 2), (1, 3), (2, 3)],
  );
  let cfg2 = cfg_with_bblocks(
    vec![
      (0, vec![Inst::cond_goto(Arg::Var(0), 1, 2)]),
      (1, vec![]),
      (2, vec![]),
      (3, vec![]),
    ],
    &[(2, 3), (0, 2), (1, 3), (0, 1)],
  );

  let r1 = run_forward_edge_dataflow(&mut BranchMarkingEdgeAnalysis::default(), &cfg1, 0);
  let r2 = run_forward_edge_dataflow(&mut BranchMarkingEdgeAnalysis::default(), &cfg2, 0);
  assert_eq!(r1, r2);
}
