use crate::analysis::nullability;
use crate::cfg::cfg::Cfg;
use crate::eval::consteval::coerce_to_bool;
use crate::il::inst::{Arg, InstTyp};
use crate::opt::PassResult;
use crate::ssa::phi_simplify::simplify_phis;
use itertools::Itertools;

// Correctness:
// - When we detach bblocks A and B (because A can never branch to B in reality e.g. const eval is always true/false), we move all bblocks in subgraph G, which contains all bblocks only reachable from B.
// - We must then detach all bblocks within G i.e. remove all edges to bblocks outside of G. This isn't recursive, as the bblocks only reachable from B doesn't change as we remove these bblocks or their edges.
// - We must clean up any usages of defs within G outside of G. Outside of G, these uses can only appear in Phi nodes.
pub fn optpass_impossible_branches(cfg: &mut Cfg) -> PassResult {
  let mut result = PassResult::default();
  loop {
    let mut iteration_changed = false;
    let mut nullability_result: Option<nullability::NullabilityResult> = None;
    for label in cfg.graph.labels_sorted() {
      let Some(inst) = cfg.bblocks.get(label).last() else {
        continue;
      };
      if inst.t != InstTyp::CondGoto {
        continue;
      };
      let (cond, true_label, false_label) = inst.as_cond_goto();
      if true_label == false_label {
        // Drop the CondGoto.
        // No need to update the graph, it's connected correctly, it's just a redundant inst.
        // TODO Should this optimization be part of optapss_impossible_branches?
        cfg.bblocks.get_mut(label).pop().unwrap();
        result.mark_changed();
        iteration_changed = true;
        continue;
      }

      if let Arg::Const(cond) = cond {
        let never_child = if coerce_to_bool(cond) {
          false_label
        } else {
          true_label
        };
        // Drop CondGoto inst.
        cfg.bblocks.get_mut(label).pop().unwrap();
        // Detach from child.
        cfg.graph.disconnect(label, never_child);
        result.mark_cfg_changed();
        iteration_changed = true;
        continue;
      }

      // Non-constant conditions: try nullability-driven pruning (e.g. `%x` is proven null,
      // so `%x !== null` can never take the true edge).
      let nullability = nullability_result.get_or_insert_with(|| nullability::calculate_nullability(cfg));
      let true_reachable = nullability.edge_is_reachable(label, true_label);
      let false_reachable = nullability.edge_is_reachable(label, false_label);
      if true_reachable == false_reachable {
        continue;
      }

      let never_child = if true_reachable { false_label } else { true_label };
      cfg.bblocks.get_mut(label).pop().unwrap();
      cfg.graph.disconnect(label, never_child);
      result.mark_cfg_changed();
      iteration_changed = true;
      // The CFG changed; restart so analysis is recomputed on the updated graph.
      break;
    }

    // Detaching bblocks means that we may have removed entire subgraphs (i.e. its descendants). Therefore, we must recalculate again the accessible bblocks.
    // NOTE: We cannot delete now, as we need to access the children of these deleted nodes first. (They won't have children after deleting.)
    let mut to_delete = cfg.graph.find_unreachable(cfg.entry).collect_vec();
    to_delete.sort_unstable();

    // Delete bblocks now so that only valid bblocks remain, which is the set of bblocks to iterate for pruning Phi insts.
    let did_delete = !to_delete.is_empty();
    cfg.graph.delete_many(to_delete.clone());
    cfg.bblocks.remove_many(to_delete);
    if did_delete {
      result.mark_cfg_changed();
      iteration_changed = true;
    }

    if simplify_phis(cfg) {
      result.mark_changed();
      iteration_changed = true;
    }

    #[cfg(debug_assertions)]
    {
      crate::ssa::phi_simplify::validate_phis(cfg)
        .expect("phi validation failed after impossible branches");
    }

    if !iteration_changed {
      break;
    }
    result.mark_changed();
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
  use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
  use num_bigint::BigInt;

  fn cfg(edges: &[(u32, u32)], blocks: &[(u32, Vec<Inst>)]) -> Cfg {
    let mut graph = CfgGraph::default();
    for &(p, c) in edges {
      graph.connect(p, c);
    }
    let mut bblocks = CfgBBlocks::default();
    for (label, insts) in blocks {
      bblocks.add(*label, insts.clone());
    }
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  #[test]
  fn prunes_edge_when_nullability_proves_branch_unreachable() {
    // %0 = null; %1 = (%0 !== null); if %1 goto 1 else 2
    let mut cfg = cfg(
      &[(0, 1), (0, 2)],
      &[
        (
          0,
          vec![
            Inst::var_assign(0, Arg::Const(Const::Null)),
            Inst::bin(1, Arg::Var(0), BinOp::NotStrictEq, Arg::Const(Const::Null)),
            Inst::cond_goto(Arg::Var(1), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
    );

    let pass = optpass_impossible_branches(&mut cfg);
    assert!(pass.cfg_changed);
    assert_eq!(cfg.graph.children_sorted(0), vec![2]);
    assert!(cfg.bblocks.maybe_get(1).is_none());
    assert_ne!(
      cfg.bblocks.get(0).last().map(|inst| inst.t.clone()),
      Some(InstTyp::CondGoto)
    );
  }

  #[test]
  fn prunes_edge_for_bigint_const_conditions_using_js_truthiness() {
    // if (0n) goto 1 else 2  => always false, so 1 becomes unreachable.
    let mut cfg = cfg(
      &[(0, 1), (0, 2)],
      &[
        (
          0,
          vec![Inst::cond_goto(
            Arg::Const(Const::BigInt(BigInt::from(0))),
            1,
            2,
          )],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
    );

    let pass = optpass_impossible_branches(&mut cfg);
    assert!(pass.cfg_changed);
    assert_eq!(cfg.graph.children_sorted(0), vec![2]);
    assert!(cfg.bblocks.maybe_get(1).is_none());
    assert_ne!(
      cfg.bblocks.get(0).last().map(|inst| inst.t.clone()),
      Some(InstTyp::CondGoto)
    );
  }
}
