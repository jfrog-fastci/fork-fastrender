use crate::cfg::cfg::Cfg;
use crate::il::inst::Inst;
use ahash::{HashMap, HashSet};
use itertools::Itertools;
use std::collections::VecDeque;

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
  fn boundary_state(&self, entry: u32, cfg: &Cfg) -> Self::State {
    let _ = (entry, cfg);
    self.bottom(cfg)
  }

  /// Combine states flowing into a block.
  fn meet(&mut self, inputs: &[(u32, &Self::State)]) -> Self::State;

  /// Instruction-level transfer function. Implementations should mutate
  /// `state` in-place to reflect the state that flows past `inst`.
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

  /// Successor-specific state refinement.
  fn apply_edge(
    &mut self,
    pred: u32,
    succ: u32,
    pred_block: &[Inst],
    state_at_end_of_pred: &Self::State,
    cfg: &Cfg,
  ) -> Self::State {
    let _ = (pred, succ, pred_block, cfg);
    state_at_end_of_pred.clone()
  }

  fn analyze(&mut self, cfg: &Cfg, entry: u32) -> ForwardEdgeDataFlowResult<Self::State>
  where
    Self: Sized,
  {
    run_forward_edge_dataflow(self, cfg, entry)
  }
}

pub fn run_forward_edge_dataflow<A: ForwardEdgeDataFlowAnalysis>(
  analysis: &mut A,
  cfg: &Cfg,
  entry: u32,
) -> ForwardEdgeDataFlowResult<A::State> {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.push(entry);
  labels.sort_unstable();
  labels.dedup();

  let bottom = analysis.bottom(cfg);
  let mut block_entry = HashMap::<u32, A::State>::default();
  let mut block_exit = HashMap::<u32, A::State>::default();
  for label in labels.iter().copied() {
    block_entry.insert(label, bottom.clone());
    block_exit.insert(label, bottom.clone());
  }

  // Track outgoing state for each edge in the CFG (after edge refinement). We
  // initialize all edges to bottom so that meet inputs can always reference an
  // existing state deterministically.
  let mut edge_out = HashMap::<(u32, u32), A::State>::default();
  for pred in labels.iter().copied() {
    for succ in cfg.graph.children_sorted(pred) {
      edge_out.insert((pred, succ), bottom.clone());
    }
  }

  let boundary_state = analysis.boundary_state(entry, cfg);
  let mut worklist = VecDeque::from([entry]);
  let mut queued = HashSet::from_iter([entry]);
  while let Some(label) = worklist.pop_front() {
    queued.remove(&label);

    let incoming = if label == entry {
      boundary_state.clone()
    } else {
      let preds = cfg.graph.parents_sorted(label);
      if preds.is_empty() {
        bottom.clone()
      } else {
        let merged_inputs = preds
          .iter()
          .map(|pred| (*pred, &edge_out[&(*pred, label)]))
          .collect_vec();
        analysis.meet(&merged_inputs)
      }
    };

    let block = cfg.bblocks.get(label);
    let exit = analysis.apply_to_block(label, block, &incoming);

    let entry_changed = block_entry[&label] != incoming;
    if entry_changed {
      block_entry.insert(label, incoming);
    }
    let exit_changed = block_exit[&label] != exit;
    if exit_changed {
      block_exit.insert(label, exit.clone());
    }

    for succ in cfg.graph.children_sorted(label) {
      let edge_state = analysis.apply_edge(label, succ, block, &exit, cfg);
      let edge_changed = edge_out[&(label, succ)] != edge_state;
      if edge_changed {
        edge_out.insert((label, succ), edge_state);
        if queued.insert(succ) {
          worklist.push_back(succ);
        }
      }
    }
  }

  ForwardEdgeDataFlowResult {
    entry,
    block_entry,
    block_exit,
    edge_out,
  }
}

