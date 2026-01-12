use crate::cfg::cfg::Cfg;
use crate::cfg::cfg::CfgEdgeKind;
use crate::il::inst::InstTyp;
use crate::opt::PassResult;
use crate::ssa::phi_simplify::simplify_phis;
use itertools::Itertools;

/**
 * WARNING: Read comment in cfg.rs.
 */
fn can_prune_bblock(
  parents: &[u32],
  children: &[u32],
  is_only_child_of_all_parents: bool,
  is_empty: bool,
) -> bool {
  // If we're the only child of exactly one parent, control flows straight through us, so we can merge with the parent.
  if parents.len() == 1 && is_only_child_of_all_parents {
    return true;
  }
  // If we're empty and only have one child, control flows straight through us from all parents, so we can be removed.
  if children.len() == 1 && is_empty {
    return true;
  };
  // If we're empty and are the only child of all parents, control flows unconditionally to us from parents, but we do nothing, so we can be removed.
  // If we're empty, we cannot have multiple children, because we would have a CondGoto.
  if is_empty && is_only_child_of_all_parents {
    return true;
  }
  false
}

fn maybe_reroot_entry(result: &mut PassResult, cfg: &mut Cfg) -> bool {
  if cfg.entry != 0 {
    return false;
  }
  let Some(entry_block) = cfg.bblocks.maybe_get(0) else {
    return false;
  };
  if !entry_block.is_empty() {
    return false;
  }
  let parents = cfg.graph.parents_sorted(0);
  if !parents.is_empty() {
    return false;
  }
  let children = cfg.graph.children_sorted(0);
  if children.is_empty() || children.len() > 1 {
    return false;
  }
  let new_entry = children[0];
  if new_entry == 0 {
    return false;
  }
  {
    let new_entry_block = cfg.bblocks.get(new_entry);
    for inst in new_entry_block.iter() {
      if inst.t != InstTyp::Phi {
        break;
      }
      if inst.labels.contains(&0) && inst.labels.len() == 1 {
        // Removing the old entry would leave this Phi without any sources.
        return false;
      }
    }
  }

  for inst in cfg.bblocks.get_mut(new_entry).iter_mut() {
    if inst.t != InstTyp::Phi {
      break;
    }
    inst.remove_phi(0);
  }

  cfg.graph.delete_many([0]);
  cfg.bblocks.remove(0);
  cfg.entry = new_entry;
  result.mark_cfg_changed();
  true
}

fn patch_terminator_label(cfg: &mut Cfg, parent: u32, old: u32, new: u32) {
  let Some(inst) = cfg.bblocks.get_mut(parent).last_mut() else {
    return;
  };
  match inst.t {
    InstTyp::CondGoto | InstTyp::Invoke | InstTyp::Throw => {
      for l in inst.labels.iter_mut() {
        if *l == old {
          *l = new;
        }
      }
    }
    _ => {}
  }
}

pub fn optpass_cfg_prune(cfg: &mut Cfg) -> PassResult {
  let mut result = PassResult::default();
  // Iterate until convergence, instead of waiting for another optimisation pass.
  loop {
    // Merge all empty leaf bblocks into one, so that we can detect CondGoto that actually go to both empty leaves.
    let mut empty_leaves = Vec::new();
    // WARNING: We must update graph within this loop, instead of simply marking and then removing afterwards, as we possibly pop instructions which could make a non-empty bblock empty, but if we don't then immediately update the graph some invariants won't hold (e.g. empty bblocks have <= 1 children). This means we can't use common utility graph functions.
    let mut converged = true;
    if maybe_reroot_entry(&mut result, cfg) {
      continue;
    }
    for cur in cfg.graph.labels_sorted() {
      if cur == cfg.entry {
        continue;
      };

      // Do not prune blocks that participate in exceptional control flow. These blocks often serve
      // as catch/finally landing pads, and merging/removing them would lose unwind structure.
      if !cfg.graph.exceptional_parents_sorted(cur).is_empty()
        || !cfg.graph.exceptional_children_sorted(cur).is_empty()
      {
        continue;
      }

      let parents = cfg.graph.parents_sorted(cur);
      let children = cfg.graph.children_sorted(cur);
      let is_only_child_of_all_parents = parents
        .iter()
        .all(|&parent| cfg.graph.children_sorted(parent).len() == 1);
      let is_empty = cfg.bblocks.get(cur).is_empty();
      let is_leaf = children.is_empty();

      // Self-loops are not safe to prune; they have an effect (e.g. busy loop).
      if children.contains(&cur) {
        continue;
      }

      // Don't merge blocks into a parent that ends with a terminator instruction
      // that semantically transfers control to this block (e.g. `throw_to`).
      if parents.len() == 1
        && is_only_child_of_all_parents
        && cfg
          .bblocks
          .get(parents[0])
          .last()
          .is_some_and(|inst| matches!(inst.t, InstTyp::Throw | InstTyp::Invoke))
      {
        continue;
      }

      if is_empty && is_leaf {
        empty_leaves.push(cur);
      }

      if !can_prune_bblock(&parents, &children, is_only_child_of_all_parents, is_empty) {
        continue;
      }

      for &c in children.iter() {
        let kind = cfg
          .graph
          .edge_kind(cur, c)
          .expect("expected existing edge kind in cfg prune");
        // Detach from children.
        cfg.graph.disconnect(cur, c);
        // Connect parents to children.
        for &parent in parents.iter() {
          cfg.graph.connect_edge(parent, c, kind);
        }
      }
      // Detach from parents.
      for &parent in parents.iter() {
        cfg.graph.disconnect(parent, cur);
      }

      // Pop from graph and bblocks.
      let insts = cfg.pop(cur);
      result.mark_cfg_changed();
      // Move insts to parents, before any CondGoto, and update that CondGoto.
      for &parent in parents.iter() {
        let p_bblock = cfg.bblocks.get_mut(parent);
        let p_term = p_bblock
          .last()
          .is_some_and(|i| matches!(i.t, InstTyp::CondGoto | InstTyp::Invoke | InstTyp::Throw))
          .then(|| p_bblock.pop().unwrap());
        p_bblock.extend(insts.clone());
        if let Some(mut inst) = p_term {
          let child = *children.iter().exactly_one().unwrap();
          match inst.t {
            InstTyp::CondGoto | InstTyp::Invoke => {
              for l in inst.labels.iter_mut() {
                if *l == cur {
                  *l = child;
                };
              }
              if inst.t == InstTyp::CondGoto {
                // Don't insert CondGoto if it's redundant now.
                // (Other code, including within this function, assume CondGoto means 2 children.)
                if inst.labels[0] == inst.labels[1] {
                  continue;
                }
              }
              p_bblock.push(inst);
            }
            InstTyp::Throw => {
              if let Some(l) = inst.labels.get_mut(0) {
                if *l == cur {
                  *l = child;
                }
              }
              p_bblock.push(inst);
            }
            _ => unreachable!(),
          }
        }
      }
      // Update phi nodes in children.
      for &c in children.iter() {
        for c_inst in cfg.bblocks.get_mut(c) {
          if c_inst.t != InstTyp::Phi {
            // No more phi nodes.
            break;
          };
          if let Some(ex) = c_inst.remove_phi(cur) {
            for &parent in parents.iter() {
              if let Some(idx) = c_inst.labels.iter().position(|&l| l == parent) {
                c_inst.args[idx] = ex.clone();
              } else {
                c_inst.insert_phi(parent, ex.clone());
              }
            }
          };
        }
      }
      converged = false;
      result.mark_cfg_changed();
    }

    // Now that we've found all empty leaves, replace all CondGoto labels that go to any of them with one bblock label.
      if empty_leaves.len() > 1 {
        empty_leaves.sort_unstable();
        empty_leaves.dedup();
        let mut existing_labels = cfg.graph.labels().collect_vec();
      existing_labels.sort_unstable();
      existing_labels.dedup();
      empty_leaves.retain(|label| existing_labels.binary_search(label).is_ok());
      if empty_leaves.len() > 1 {
        for (label, bblocks) in cfg.bblocks.all_mut() {
          let Some(inst) = bblocks.last_mut() else {
            continue;
          };
          if inst.t != InstTyp::CondGoto {
            continue;
          }
            let mut all_children_empty = true;
            for child in inst.labels.iter_mut() {
              if empty_leaves.contains(child) {
                let new_child = empty_leaves[0];
                let kind = cfg
                  .graph
                  .edge_kind(label, *child)
                  .unwrap_or(CfgEdgeKind::Normal);
                cfg.graph.disconnect(label, *child);
                cfg.graph.connect_edge(label, new_child, kind);
                *child = new_child;
                result.mark_cfg_changed();
              } else {
                all_children_empty = false;
              };
          }
          if all_children_empty {
            // Drop the CondGoto.
            bblocks.pop().unwrap();
          };
        }
        // For all other empty leaves, redirect any remaining incoming edges into the merged leaf
        // (e.g. implicit fallthrough) and then remove them.
        //
        // Note: the CondGoto-to-leaf edges are handled above by patching the terminating
        // instruction's labels. But empty leaf blocks can still be targeted by graph-only edges
        // (implicit fallthrough); those also need redirecting before we can safely `pop` them.
        let merged_leaf = empty_leaves[0];
        for label in empty_leaves.into_iter().skip(1) {
          for parent in cfg.graph.parents_sorted(label) {
            // Preserve edge kind (normal vs exceptional) on redirected edges.
            let kind = cfg
              .graph
              .edge_kind(parent, label)
              .unwrap_or(CfgEdgeKind::Normal);

            // Patch any explicit terminator in the parent that still references the old label.
            {
              let bblock = cfg.bblocks.get_mut(parent);
              let term = bblock
                .last()
                .is_some_and(|i| matches!(i.t, InstTyp::CondGoto | InstTyp::Invoke | InstTyp::Throw))
                .then(|| bblock.pop().unwrap());
              if let Some(mut term) = term {
                match term.t {
                  InstTyp::CondGoto | InstTyp::Invoke => {
                    for l in term.labels.iter_mut() {
                      if *l == label {
                        *l = merged_leaf;
                      }
                    }
                    if term.t == InstTyp::CondGoto && term.labels[0] == term.labels[1] {
                      // Drop redundant CondGoto.
                    } else {
                      bblock.push(term);
                    }
                  }
                  InstTyp::Throw => {
                    if let Some(l) = term.labels.get_mut(0) {
                      if *l == label {
                        *l = merged_leaf;
                      }
                    }
                    bblock.push(term);
                  }
                  _ => unreachable!(),
                }
              }
            }

            cfg.graph.disconnect(parent, label);
            cfg.graph.connect_edge(parent, merged_leaf, kind);
          }
          if cfg.bblocks.maybe_get(label).is_some() {
            cfg.pop(label);
          }
        }
        result.mark_cfg_changed();
      }
    }

    if simplify_phis(cfg) {
      result.mark_changed();
      converged = false;
    }

    #[cfg(debug_assertions)]
    {
      crate::ssa::phi_simplify::validate_phis(cfg).expect("phi validation failed after cfg prune");
    }

    if converged {
      break;
    }
  }
  result
}
