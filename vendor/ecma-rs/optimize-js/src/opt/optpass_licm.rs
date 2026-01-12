use crate::analysis::find_loops::find_loops;
use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::{Arg, Inst, InstTyp, Purity};
use crate::opt::PassResult;
use ahash::HashMap;
use ahash::HashMapExt;
use ahash::HashSet;

fn next_unused_label(cfg: &Cfg) -> u32 {
  let max = cfg
    .graph
    .labels()
    .max()
    .unwrap_or(cfg.entry);
  max.checked_add(1).expect("label overflow in LICM")
}

fn next_unused_var(cfg: &Cfg) -> u32 {
  let mut max = 0u32;
  for (_, bb) in cfg.bblocks.all() {
    for inst in bb {
      for &tgt in &inst.tgts {
        max = max.max(tgt);
      }
      for arg in &inst.args {
        if let Arg::Var(v) = arg {
          max = max.max(*v);
        }
      }
    }
  }
  max.checked_add(1).expect("var overflow in LICM")
}

fn alloc_label(next: &mut u32) -> u32 {
  let out = *next;
  *next = next.checked_add(1).expect("label overflow in LICM");
  out
}

fn alloc_var(next: &mut u32) -> u32 {
  let out = *next;
  *next = next.checked_add(1).expect("var overflow in LICM");
  out
}

fn rewrite_edge(cfg: &mut Cfg, from: u32, old_to: u32, new_to: u32) {
  cfg.graph.disconnect(from, old_to);
  cfg.graph.connect(from, new_to);

  if let Some(term) = cfg.bblocks.get_mut(from).last_mut() {
    if term.t == InstTyp::CondGoto {
      for l in term.labels.iter_mut() {
        if *l == old_to {
          *l = new_to;
        }
      }
    }
  }
}

fn rewrite_phi_incoming_label(phi: &mut Inst, old_pred: u32, new_pred: u32) {
  debug_assert_eq!(phi.t, InstTyp::Phi);
  for l in phi.labels.iter_mut() {
    if *l == old_pred {
      *l = new_pred;
    }
  }
}

fn ensure_loop_preheader(
  cfg: &mut Cfg,
  header: u32,
  loop_nodes: &HashSet<u32>,
  next_label: &mut u32,
  next_var: &mut u32,
) -> Option<(u32, bool)> {
  // Identify predecessors coming from outside the loop.
  let preds = cfg.graph.parents_sorted(header);
  let mut outside_preds: Vec<u32> = preds.into_iter().filter(|p| !loop_nodes.contains(p)).collect();
  outside_preds.sort_unstable();
  outside_preds.dedup();

  if outside_preds.is_empty() {
    return None;
  }

  // Fast path: a canonical preheader already exists.
  if outside_preds.len() == 1 {
    let pred = outside_preds[0];
    let succs = cfg.graph.children_sorted(pred);
    if succs.len() == 1 && succs[0] == header {
      return Some((pred, false));
    }
  }

  let preheader = alloc_label(next_label);
  cfg.graph.ensure_label(preheader);
  cfg.bblocks.add(preheader, Vec::new());

  // Redirect all outside entries to go through the new preheader.
  for &pred in &outside_preds {
    rewrite_edge(cfg, pred, header, preheader);
  }
  cfg.graph.connect(preheader, header);

  // Fix up Phi nodes in the header: the incoming edges from outside preds now come from `preheader`.
  {
    let header_bb = cfg.bblocks.get_mut(header);

    // If the loop had a single outside predecessor, we can just rename that label in-place.
    if outside_preds.len() == 1 {
      let pred = outside_preds[0];
      for inst in header_bb.iter_mut() {
        if inst.t != InstTyp::Phi {
          break;
        }
        rewrite_phi_incoming_label(inst, pred, preheader);
      }
      return Some((preheader, true));
    }

    // Multiple outside predecessors: we may need to merge their incoming values in the preheader.
    let mut preheader_phis: Vec<Inst> = Vec::new();
    for inst in header_bb.iter_mut() {
      if inst.t != InstTyp::Phi {
        break;
      }
      // Collect incoming values from outside preds.
      let mut incoming: Vec<(u32, Arg)> = Vec::new();
      let mut i = 0;
      while i < inst.labels.len() {
        let l = inst.labels[i];
        if outside_preds.binary_search(&l).is_ok() {
          let arg = inst.args[i].clone();
          incoming.push((l, arg));
          inst.labels.remove(i);
          inst.args.remove(i);
        } else {
          i += 1;
        }
      }

      if incoming.is_empty() {
        continue;
      }

      incoming.sort_by_key(|(l, _)| *l);

      // If all incoming args are identical, we can reuse that value directly without inserting a
      // new phi in the preheader.
      let all_same = incoming
        .iter()
        .skip(1)
        .all(|(_, arg)| *arg == incoming[0].1);
      if all_same {
        inst.insert_phi(preheader, incoming[0].1.clone());
        continue;
      }

      // Otherwise, create a new Phi in the preheader and feed that to the header Phi.
      let new_tgt = alloc_var(next_var);
      let mut phi = Inst::phi_empty(new_tgt);
      for (pred, arg) in incoming {
        phi.insert_phi(pred, arg);
      }
      preheader_phis.push(phi);
      inst.insert_phi(preheader, Arg::Var(new_tgt));
    }

    if !preheader_phis.is_empty() {
      let pre_bb = cfg.bblocks.get_mut(preheader);
      // Ensure any preexisting phi nodes stay at the front.
      let first_non_phi = pre_bb
        .iter()
        .position(|inst| inst.t != InstTyp::Phi)
        .unwrap_or(pre_bb.len());
      pre_bb.splice(first_non_phi..first_non_phi, preheader_phis);
    }
  }

  Some((preheader, true))
}

fn build_def_blocks(cfg: &Cfg) -> HashMap<u32, u32> {
  let mut defs = HashMap::<u32, u32>::new();
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(l, _)| l).collect();
  labels.sort_unstable();
  for label in labels {
    let bb = cfg.bblocks.get(label);
    for inst in bb {
      for &tgt in &inst.tgts {
        defs.insert(tgt, label);
      }
    }
  }
  defs
}

fn is_hoist_candidate(inst: &Inst) -> bool {
  match inst.t {
    InstTyp::Bin => inst.bin_op != crate::il::inst::BinOp::GetProp,
    InstTyp::Un | InstTyp::VarAssign => true,
    InstTyp::Call => {
      inst.meta.callee_purity == Purity::Pure && inst.meta.effects.is_pure()
    }
    InstTyp::Assume => false,
    #[cfg(feature = "native-async-ops")]
    InstTyp::Await | InstTyp::PromiseAll | InstTyp::PromiseRace => false,
    // Observable reads/writes or control flow / SSA.
    InstTyp::ForeignLoad
    | InstTyp::UnknownLoad
    | InstTyp::ForeignStore
    | InstTyp::UnknownStore
    | InstTyp::PropAssign
    | InstTyp::Phi
    | InstTyp::CondGoto
    | InstTyp::Return
    | InstTyp::Throw
    | InstTyp::_Label
    | InstTyp::_Goto
    | InstTyp::_Dummy => false,
  }
}

fn operands_dominate_preheader(
  inst: &Inst,
  preheader: u32,
  dominates: &crate::dom::DominatesGraph,
  def_blocks: &HashMap<u32, u32>,
  entry: u32,
) -> bool {
  inst.args.iter().all(|arg| match arg {
    Arg::Var(v) => {
      let def_block = def_blocks.get(v).copied().unwrap_or(entry);
      dominates.dominates(def_block, preheader)
    }
    _ => true,
  })
}

fn find_canonical_preheader(cfg: &Cfg, header: u32, loop_nodes: &HashSet<u32>) -> Option<u32> {
  let mut outside_preds: Vec<u32> = cfg
    .graph
    .parents_sorted(header)
    .into_iter()
    .filter(|p| !loop_nodes.contains(p))
    .collect();
  outside_preds.sort_unstable();
  outside_preds.dedup();

  if outside_preds.len() != 1 {
    return None;
  }
  let pred = outside_preds[0];
  let succs = cfg.graph.children_sorted(pred);
  if succs.len() == 1 && succs[0] == header {
    Some(pred)
  } else {
    None
  }
}

pub fn optpass_licm(cfg: &mut Cfg) -> PassResult {
  let mut result = PassResult::default();

  let mut next_label = next_unused_label(cfg);
  let mut next_var = next_unused_var(cfg);

  // 1) Ensure canonical preheaders for all natural loops.
  loop {
    let dom = Dom::calculate(cfg);
    let loops = find_loops(cfg, &dom);
    if loops.is_empty() {
      return result;
    }

    let mut headers: Vec<u32> = loops.keys().copied().collect();
    headers.sort_unstable();

    let mut changed = false;
    for header in headers {
      let nodes = loops
        .get(&header)
        .expect("loop header missing from find_loops map");
      let Some((_preheader, created)) =
        ensure_loop_preheader(cfg, header, nodes, &mut next_label, &mut next_var)
      else {
        continue;
      };
      if created {
        result.mark_cfg_changed();
        changed = true;
      }
    }

    if !changed {
      break;
    }
  }

  // 2) Hoist invariant pure instructions into the corresponding loop preheaders.
  let dom = Dom::calculate(cfg);
  let dominates = dom.dominates_graph();
  let loops = find_loops(cfg, &dom);

  let mut def_blocks = build_def_blocks(cfg);
  let rpo = cfg.reverse_postorder();

  let mut headers: Vec<u32> = loops.keys().copied().collect();
  headers.sort_unstable();

  for header in headers {
    let nodes = &loops[&header];
    let Some(preheader) = find_canonical_preheader(cfg, header, nodes) else {
      continue;
    };

    // Deterministic traversal order through the loop body.
    for label in rpo.iter().copied().filter(|l| nodes.contains(l)) {
      let mut hoisted_insts = Vec::<Inst>::new();

      {
        // Skip the header's phi nodes; they are not movable.
        let bb = cfg.bblocks.get_mut(label);
        let mut idx = 0usize;
        while idx < bb.len() && bb[idx].t == InstTyp::Phi {
          idx += 1;
        }

        while idx < bb.len() {
          if !is_hoist_candidate(&bb[idx]) {
            idx += 1;
            continue;
          }

          if !operands_dominate_preheader(&bb[idx], preheader, &dominates, &def_blocks, cfg.entry) {
            idx += 1;
            continue;
          }

          let inst = bb.remove(idx);
          if !inst.tgts.is_empty() {
            for &tgt in &inst.tgts {
              def_blocks.insert(tgt, preheader);
            }
          }
          hoisted_insts.push(inst);
        }
      }

      if !hoisted_insts.is_empty() {
        cfg.bblocks.get_mut(preheader).extend(hoisted_insts);
        result.mark_changed();
      }
    }
  }

  result
}
