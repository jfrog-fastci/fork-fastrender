use crate::analysis::nullability;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, Inst, InstTyp};
use crate::opt::PassResult;

fn resolve_replacement(arg: &Arg, replacements: &ahash::HashMap<u32, Arg>) -> Arg {
  match arg {
    Arg::Var(v) => {
      let mut current = *v;
      let mut seen = ahash::HashSet::<u32>::default();
      loop {
        if !seen.insert(current) {
          return Arg::Var(current);
        }
        let Some(next) = replacements.get(&current) else {
          return Arg::Var(current);
        };
        match next {
          Arg::Var(next_v) => current = *next_v,
          other => return other.clone(),
        }
      }
    }
    _ => arg.clone(),
  }
}

fn first_non_phi(bb: &[Inst]) -> usize {
  bb.iter().position(|inst| inst.t != InstTyp::Phi).unwrap_or(bb.len())
}

/// Null-check elimination and deduplication.
///
/// This pass removes explicit `InstTyp::NullCheck` instructions when the checked
/// value is proven non-nullish by `analysis::nullability`, and merges duplicate
/// checks where possible.
pub fn optpass_nullcheck_elim(cfg: &mut Cfg) -> PassResult {
  // Fast path: if the CFG contains no explicit null checks, skip the nullability analysis.
  // This pass is in the general optimisation loop, and computing nullability on large CFGs is
  // non-trivial overhead for untyped pipelines that never emit `NullCheck`.
  if !cfg
    .bblocks
    .all()
    .any(|(_, bb)| bb.iter().any(|inst| inst.t == InstTyp::NullCheck))
  {
    return PassResult::default();
  }

  let mut result = PassResult::default();

  let analysis = nullability::calculate_nullability(cfg);
  let mut replacements: ahash::HashMap<u32, Arg> = ahash::HashMap::default();

  // 1) Remove checks that are proven redundant by the nullability dataflow.
  let labels = cfg.graph.labels_sorted();
  for label in &labels {
    let bb = cfg.bblocks.get(*label);
    if bb.is_empty() {
      continue;
    }
    let mut to_delete = Vec::<usize>::new();
    for (idx, inst) in bb.iter().enumerate() {
      if inst.t != InstTyp::NullCheck {
        continue;
      }
      let (tgt, value) = inst.as_null_check();
      let state = analysis.state_before_inst(cfg, *label, idx);
      let mask = analysis.fact_for_arg(&state, value);
      if !mask.is_non_nullish() {
        continue;
      }
      if let Some(tgt) = tgt {
        // The check is redundant: it forwards the value unchanged.
        replacements.insert(tgt, value.clone());
      }
      to_delete.push(idx);
    }

    if !to_delete.is_empty() {
      let bb = cfg.bblocks.get_mut(*label);
      for idx in to_delete.into_iter().rev() {
        bb.remove(idx);
      }
      result.mark_changed();
    }
  }

  if !replacements.is_empty() {
    for label in &labels {
      for inst in cfg.bblocks.get_mut(*label).iter_mut() {
        for arg in inst.args.iter_mut() {
          *arg = resolve_replacement(arg, &replacements);
        }
      }
    }
  }

  // 2) Simple branch-hoisting for common patterns:
  // If both arms of an `if` begin with the same check-only NullCheck, hoist it
  // into the branching block right before the CondGoto.
  for label in &labels {
    let bb = cfg.bblocks.get(*label);
    let Some(term) = bb.last() else {
      continue;
    };
    if term.t != InstTyp::CondGoto {
      continue;
    }
    let then_label = term.labels.get(0).copied().unwrap_or_default();
    let else_label = term.labels.get(1).copied().unwrap_or_default();

    let then_idx = {
      let then_bb = cfg.bblocks.get(then_label);
      let idx = first_non_phi(then_bb);
      if idx < then_bb.len()
        && then_bb[idx].t == InstTyp::NullCheck
        && then_bb[idx].tgts.is_empty()
      {
        Some((idx, then_bb[idx].args[0].clone()))
      } else {
        None
      }
    };
    let else_idx = {
      let else_bb = cfg.bblocks.get(else_label);
      let idx = first_non_phi(else_bb);
      if idx < else_bb.len()
        && else_bb[idx].t == InstTyp::NullCheck
        && else_bb[idx].tgts.is_empty()
      {
        Some((idx, else_bb[idx].args[0].clone()))
      } else {
        None
      }
    };

    let (then_idx, check_arg) = match (then_idx, else_idx) {
      (Some((t_idx, t_arg)), Some((e_idx, e_arg))) if t_arg == e_arg => (t_idx, t_arg),
      _ => continue,
    };

    // Hoist into the end of the predecessor block (right before the CondGoto).
    let pred_bb = cfg.bblocks.get_mut(*label);
    if pred_bb.is_empty() {
      continue;
    }
    let insert_at = pred_bb.len() - 1;
    pred_bb.insert(insert_at, Inst::null_check(None::<u32>, check_arg));

    // Remove the duplicate checks from both successor blocks.
    let then_bb = cfg.bblocks.get_mut(then_label);
    then_bb.remove(then_idx);
    let else_bb = cfg.bblocks.get_mut(else_label);
    let else_idx = first_non_phi(else_bb);
    // Recompute, as removing from `then_bb` didn't affect `else_bb`, but `else_idx`
    // must account for potential phis.
    else_bb.remove(else_idx);

    result.mark_changed();
  }

  result
}
