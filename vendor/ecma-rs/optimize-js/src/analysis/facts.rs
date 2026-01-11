//! Shared helper types for analysis results.
//!
//! The optimizer's analyses generally store facts at **basic-block boundaries**
//! (block entry, and optionally per-edge) to avoid bloating [`crate::il::inst::Inst`]
//! with per-instruction metadata. When a pass needs facts at a specific
//! instruction it can **replay** the analysis transfer function within the
//! block, starting from the stored block entry state.
//!
//! This module defines stable location keys ([`InstLoc`], [`Edge`]) and a small
//! replay helper used by multiple analyses.

use crate::cfg::cfg::Cfg;
use crate::il::inst::Inst;

/// Stable key for an instruction position within a CFG.
///
/// This is a purely structural location: it is only valid as long as the CFG's
/// basic blocks and instruction ordering have not changed since the analysis was
/// computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct InstLoc {
  pub block: u32,
  pub inst: usize,
}

/// Stable key for a directed CFG edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Edge {
  pub from: u32,
  pub to: u32,
}

/// Replay a forward analysis transfer function within a single basic block.
///
/// Returns the state **before** instruction `inst_idx`, i.e. after applying
/// transfers for instructions `< inst_idx`.
///
/// `inst_idx` may be equal to the block's instruction count to obtain the block
/// exit state.
pub fn replay_forward_before_inst<State: Clone>(
  cfg: &Cfg,
  label: u32,
  entry_state: &State,
  inst_idx: usize,
  mut apply: impl FnMut(u32, usize, &Inst, &mut State),
) -> State {
  let block = cfg.bblocks.get(label);
  assert!(
    inst_idx <= block.len(),
    "inst index {inst_idx} out of bounds for block {label} (len={})",
    block.len()
  );
  let mut state = entry_state.clone();
  for (idx, inst) in block.iter().enumerate().take(inst_idx) {
    apply(label, idx, inst, &mut state);
  }
  state
}

/// Replay a forward analysis transfer function within a single basic block.
///
/// Returns the state **after** instruction `inst_idx`, i.e. after applying the
/// transfer for that instruction.
pub fn replay_forward_after_inst<State: Clone>(
  cfg: &Cfg,
  label: u32,
  entry_state: &State,
  inst_idx: usize,
  mut apply: impl FnMut(u32, usize, &Inst, &mut State),
) -> State {
  let block = cfg.bblocks.get(label);
  assert!(
    inst_idx < block.len(),
    "inst index {inst_idx} out of bounds for block {label} (len={})",
    block.len()
  );
  let mut state = replay_forward_before_inst(cfg, label, entry_state, inst_idx, &mut apply);
  apply(label, inst_idx, &block[inst_idx], &mut state);
  state
}

