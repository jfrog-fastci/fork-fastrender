use crate::cfg::cfg::{Cfg, Terminator};
use crate::il::inst::Inst;
use ahash::{HashMap, HashSet};
use itertools::Itertools;
use std::collections::VecDeque;

use super::dataflow::BlockState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeDataFlowResult<T> {
  pub entry: u32,
  pub blocks: HashMap<u32, BlockState<T>>,
  pub edges: HashMap<(u32, u32), T>,
}

impl<T> EdgeDataFlowResult<T> {
  pub fn block_entry(&self, label: u32) -> Option<&T> {
    self.blocks.get(&label).map(|b| &b.entry)
  }

  pub fn block_exit(&self, label: u32) -> Option<&T> {
    self.blocks.get(&label).map(|b| &b.exit)
  }

  pub fn edge(&self, from: u32, to: u32) -> Option<&T> {
    self.edges.get(&(from, to))
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardEdgeDataFlowResult<T> {
  pub entry: u32,
  pub block_entry: HashMap<u32, T>,
  /// State at the end of a block after applying instruction transfers, but before
  /// successor-specific edge refinement.
  pub block_exit: HashMap<u32, T>,
  pub edge_out: HashMap<(u32, u32), T>,
}

pub trait ForwardEdgeDataFlowAnalysis {
  type State: Clone + Eq;

  /// The bottom element for the lattice of [`Self::State`].
  fn bottom(&self, cfg: &Cfg) -> Self::State;

  /// Provide the initial state for the entry block. Defaults to bottom.
  fn boundary_state(&self, entry_label: u32, cfg: &Cfg) -> Self::State {
    let _ = (entry_label, cfg);
    self.bottom(cfg)
  }

  /// Combine states flowing into a block.
  fn meet(&mut self, incoming: &[(u32, &Self::State)]) -> Self::State;

  /// Like [`Self::meet`], but also provides the destination label and the
  /// previous entry state for that label.
  ///
  /// This is useful for analyses that need to apply widenings at loop headers.
  /// The default implementation forwards to [`Self::meet`] and ignores the
  /// extra context.
  fn meet_block(
    &mut self,
    label: u32,
    prev_entry: &Self::State,
    incoming: &[(u32, &Self::State)],
  ) -> Self::State {
    let _ = (label, prev_entry);
    self.meet(incoming)
  }

  /// Instruction-level transfer function.
  fn apply_to_instruction(
    &mut self,
    label: u32,
    inst_idx: usize,
    inst: &Inst,
    state: &mut Self::State,
  );

  /// Block-level transfer function derived from instruction-level transfers.
  fn apply_to_block(&mut self, label: u32, block: &[Inst], state: &Self::State) -> Self::State {
    let mut next = state.clone();
    for (idx, inst) in block.iter().enumerate() {
      self.apply_to_instruction(label, idx, inst, &mut next);
    }
    next
  }

  /// Edge-level transfer function applied to each outgoing edge of a block.
  fn apply_to_edge(
    &mut self,
    from: u32,
    to: u32,
    terminator: &Terminator,
    state: &Self::State,
  ) -> Self::State {
    let _ = (from, to, terminator);
    state.clone()
  }

  /// Successor-specific state refinement (legacy hook).
  ///
  /// Prefer overriding [`Self::apply_to_edge`] when possible.
  fn apply_edge(
    &mut self,
    from: u32,
    to: u32,
    _from_block: &[Inst],
    state_at_end_of_from: &Self::State,
    cfg: &Cfg,
  ) -> Self::State {
    let terminator = cfg.terminator(from);
    self.apply_to_edge(from, to, &terminator, state_at_end_of_from)
  }

  /// Optional widening applied when an edge state is updated.
  fn widen_edge(
    &mut self,
    from: u32,
    to: u32,
    old: &Self::State,
    new: &Self::State,
  ) -> Self::State {
    let _ = (from, to, old);
    new.clone()
  }

  fn analyze(&mut self, cfg: &Cfg, entry_label: u32) -> ForwardEdgeDataFlowResult<Self::State>
  where
    Self: Sized,
  {
    run_forward_edge_dataflow(self, cfg, entry_label)
  }
}

// Back-compat / naming alignment with `EXEC.plan.md` tasks.
pub use ForwardEdgeDataFlowAnalysis as EdgeDataFlowAnalysis;

fn cfg_labels(cfg: &Cfg, entry_label: u32) -> Vec<u32> {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.push(entry_label);
  labels.sort_unstable();
  labels.dedup();
  labels
}

pub fn run_edge_dataflow<A: ForwardEdgeDataFlowAnalysis>(
  analysis: &mut A,
  cfg: &Cfg,
  entry_label: u32,
) -> EdgeDataFlowResult<A::State> {
  assert!(
    cfg.bblocks.maybe_get(entry_label).is_some(),
    "CFG is missing entry block {entry_label}",
  );

  let labels = cfg_labels(cfg, entry_label);
  let bottom = analysis.bottom(cfg);

  let mut blocks = HashMap::<u32, BlockState<A::State>>::default();
  for label in labels.iter().copied() {
    blocks.insert(
      label,
      BlockState {
        entry: bottom.clone(),
        exit: bottom.clone(),
      },
    );
  }

  // Track outgoing state for each edge in the CFG (after edge refinement). We
  // initialize all edges to bottom so that meet inputs can always reference an
  // existing state deterministically.
  let mut edges = HashMap::<(u32, u32), A::State>::default();
  for from in labels.iter().copied() {
    for to in cfg.graph.children_sorted(from) {
      edges.insert((from, to), bottom.clone());
    }
  }

  let boundary_state = analysis.boundary_state(entry_label, cfg);
  let mut worklist = VecDeque::from([entry_label]);
  let mut queued = HashSet::from_iter([entry_label]);

  while let Some(label) = worklist.pop_front() {
    queued.remove(&label);

    let incoming = if label == entry_label {
      boundary_state.clone()
    } else {
      let preds = cfg.graph.parents_sorted(label);
      if preds.is_empty() {
        bottom.clone()
      } else {
        let merged_inputs = preds
          .iter()
          .map(|pred| (*pred, &edges[&(*pred, label)]))
          .collect_vec();
        analysis.meet_block(label, &blocks[&label].entry, &merged_inputs)
      }
    };

    let block = cfg.bblocks.get(label);
    let exit = analysis.apply_to_block(label, block, &incoming);

    for succ in cfg.graph.children_sorted(label) {
      let edge_new = analysis.apply_edge(label, succ, block, &exit, cfg);
      let edge = edges
        .get_mut(&(label, succ))
        .unwrap_or_else(|| panic!("missing edge state for ({label}, {succ})"));
      let widened = analysis.widen_edge(label, succ, edge, &edge_new);
      if *edge != widened {
        *edge = widened;
        if queued.insert(succ) {
          worklist.push_back(succ);
        }
      }
    }

    let block_state = blocks
      .get_mut(&label)
      .unwrap_or_else(|| panic!("missing block state for {label}"));
    if block_state.entry != incoming || block_state.exit != exit {
      *block_state = BlockState {
        entry: incoming,
        exit,
      };
    }
  }

  EdgeDataFlowResult {
    entry: entry_label,
    blocks,
    edges,
  }
}

pub fn run_forward_edge_dataflow<A: ForwardEdgeDataFlowAnalysis>(
  analysis: &mut A,
  cfg: &Cfg,
  entry: u32,
) -> ForwardEdgeDataFlowResult<A::State> {
  let EdgeDataFlowResult {
    entry,
    blocks,
    edges,
  } = run_edge_dataflow(analysis, cfg, entry);

  let mut block_entry = HashMap::<u32, A::State>::default();
  let mut block_exit = HashMap::<u32, A::State>::default();
  for (label, state) in blocks {
    block_entry.insert(label, state.entry);
    block_exit.insert(label, state.exit);
  }

  ForwardEdgeDataFlowResult {
    entry,
    block_entry,
    block_exit,
    edge_out: edges,
  }
}

#[cfg(test)]
mod tests {
  use super::{run_edge_dataflow, ForwardEdgeDataFlowAnalysis};
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph, Terminator};
  use crate::il::inst::{Arg, Inst};

  fn cfg(labels: &[u32], edges: &[(u32, u32)], block: impl Fn(u32) -> Vec<Inst>) -> Cfg {
    let mut graph = CfgGraph::default();
    for &(from, to) in edges {
      graph.connect(from, to);
    }
    let mut bblocks = CfgBBlocks::default();
    for &label in labels {
      bblocks.add(label, block(label));
    }
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  #[derive(Default)]
  struct BranchRefine;

  impl ForwardEdgeDataFlowAnalysis for BranchRefine {
    type State = Option<bool>;

    fn bottom(&self, _cfg: &Cfg) -> Self::State {
      None
    }

    fn meet(&mut self, incoming: &[(u32, &Self::State)]) -> Self::State {
      let mut iter = incoming.iter();
      let Some((_, first)) = iter.next() else {
        return None;
      };
      if iter.all(|(_, s)| s == first) {
        (*first).clone()
      } else {
        None
      }
    }

    fn apply_to_instruction(
      &mut self,
      _label: u32,
      _inst_idx: usize,
      _inst: &Inst,
      _state: &mut Self::State,
    ) {
    }

    fn apply_to_edge(
      &mut self,
      _from: u32,
      to: u32,
      terminator: &Terminator,
      state: &Self::State,
    ) -> Self::State {
      match terminator {
        Terminator::CondGoto { t, f, .. } => {
          if to == *t {
            Some(true)
          } else if to == *f {
            Some(false)
          } else {
            state.clone()
          }
        }
        _ => state.clone(),
      }
    }
  }

  #[test]
  fn edge_states_can_differ_for_conditional_branches() {
    let cfg = cfg(&[0, 1, 2], &[(0, 2), (0, 1)], |label| match label {
      0 => vec![Inst::cond_goto(Arg::Var(0), 1, 2)],
      _ => Vec::new(),
    });

    let result = run_edge_dataflow(&mut BranchRefine::default(), &cfg, 0);
    assert_eq!(result.edge(0, 1), Some(&Some(true)));
    assert_eq!(result.edge(0, 2), Some(&Some(false)));
    assert_eq!(result.block_entry(1), Some(&Some(true)));
    assert_eq!(result.block_entry(2), Some(&Some(false)));
  }

  #[derive(Default)]
  struct OrderSensitiveMeet;

  impl ForwardEdgeDataFlowAnalysis for OrderSensitiveMeet {
    type State = Vec<u32>;

    fn bottom(&self, _cfg: &Cfg) -> Self::State {
      Vec::new()
    }

    fn meet(&mut self, incoming: &[(u32, &Self::State)]) -> Self::State {
      incoming
        .iter()
        .flat_map(|(_, state)| state.iter().copied())
        .collect()
    }

    fn apply_to_block(&mut self, label: u32, _block: &[Inst], state: &Self::State) -> Self::State {
      let mut next = state.clone();
      next.push(label);
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

  #[test]
  fn deterministic_across_edge_insertion_ordering() {
    let cfg1 = cfg(&[0, 1, 2, 3], &[(0, 1), (0, 2), (1, 3), (2, 3)], |_| {
      Vec::new()
    });
    let cfg2 = cfg(&[0, 1, 2, 3], &[(2, 3), (0, 2), (1, 3), (0, 1)], |_| {
      Vec::new()
    });

    let r1 = run_edge_dataflow(&mut OrderSensitiveMeet::default(), &cfg1, 0);
    let r2 = run_edge_dataflow(&mut OrderSensitiveMeet::default(), &cfg2, 0);
    assert_eq!(r1, r2);
  }

  #[derive(Default)]
  struct WideningLoop {
    widen_calls: usize,
  }

  impl ForwardEdgeDataFlowAnalysis for WideningLoop {
    type State = u32;

    fn bottom(&self, _cfg: &Cfg) -> Self::State {
      0
    }

    fn meet(&mut self, incoming: &[(u32, &Self::State)]) -> Self::State {
      incoming.iter().map(|(_, s)| **s).max().unwrap_or(0)
    }

    fn apply_to_block(&mut self, _label: u32, _block: &[Inst], state: &Self::State) -> Self::State {
      state.saturating_add(1)
    }

    fn apply_to_instruction(
      &mut self,
      _label: u32,
      _inst_idx: usize,
      _inst: &Inst,
      _state: &mut Self::State,
    ) {
    }

    fn widen_edge(
      &mut self,
      from: u32,
      to: u32,
      old: &Self::State,
      new: &Self::State,
    ) -> Self::State {
      const TOP: u32 = 100;
      if from == 2 && to == 1 {
        self.widen_calls += 1;
        if *old >= TOP {
          return *old;
        }
        if new > old {
          return TOP;
        }
      }
      *new
    }
  }

  #[test]
  fn widening_hook_can_force_convergence_on_a_loop() {
    let cfg = cfg(&[0, 1, 2, 3], &[(0, 1), (1, 2), (2, 1), (1, 3)], |_| {
      Vec::new()
    });

    let mut analysis = WideningLoop::default();
    let result = run_edge_dataflow(&mut analysis, &cfg, 0);

    assert!(
      analysis.widen_calls > 0,
      "expected widening hook to be invoked"
    );
    assert_eq!(result.edge(2, 1), Some(&100));
    assert_eq!(result.block_entry(1), Some(&100));
    assert_eq!(result.block_exit(2), Some(&102));
  }
}
