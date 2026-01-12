use crate::analysis::loop_canon::find_counted_loops;
use crate::cfg::cfg::{Cfg, Terminator};
use crate::dom::Dom;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use crate::opt::PassResult;
use crate::util::counter::Counter;
use ahash::{HashMap, HashMapExt};
use parse_js::num::JsNumber;

const FULL_UNROLL_MAX_TRIP_COUNT: u64 = 16;
const FULL_UNROLL_MAX_TOTAL_CLONED_INSTS: usize = 256;
const PARTIAL_UNROLL_MAX_BODY_INSTS: usize = 16;

fn is_unrollable_inst(inst: &Inst) -> bool {
  matches!(inst.t, InstTyp::Bin | InstTyp::Un | InstTyp::VarAssign)
}

fn block_is_unrollable(block: &[Inst]) -> bool {
  block.iter().all(is_unrollable_inst)
}

fn clone_with_renames(
  inst: &Inst,
  map: &HashMap<u32, Arg>,
  c_temp: &mut Counter,
  local_defs: &mut HashMap<u32, Arg>,
) -> Inst {
  let mut out = inst.clone();
  for arg in out.args.iter_mut() {
    if let Arg::Var(v) = arg {
      if let Some(repl) = local_defs.get(v).or_else(|| map.get(v)) {
        *arg = repl.clone();
      }
    }
  }
  if let Some(orig_tgt) = out.tgts.get(0).copied() {
    let new_tgt = c_temp.bump();
    out.tgts[0] = new_tgt;
    local_defs.insert(orig_tgt, Arg::Var(new_tgt));
  }
  out
}

fn full_unroll_simple_loop(
  cfg: &mut Cfg,
  header: u32,
  body: u32,
  latch: u32,
  exit: u32,
  header_phis: &[crate::analysis::loop_canon::LoopPhi],
  trip_count: u64,
  c_temp: &mut Counter,
) {
  let body_block = cfg.bblocks.get(body).clone();
  let latch_block = cfg.bblocks.get(latch).clone();
  let template: Vec<Inst> = body_block
    .into_iter()
    .chain(latch_block.into_iter())
    .collect();

  let mut out_header = Vec::<Inst>::new();
  // Convert loop header phis into initial assignments so iteration 0 can reuse the original
  // body/latch instructions without substitution.
  for phi in header_phis {
    out_header.push(Inst::var_assign(phi.tgt, phi.preheader_arg.clone()));
  }

  // Values for header phi targets at the start of the current iteration.
  let mut cur_vals = HashMap::<u32, Arg>::new();
  for phi in header_phis {
    cur_vals.insert(phi.tgt, Arg::Var(phi.tgt));
  }

  // Execute `trip_count` iterations back-to-back.
  for iter_idx in 0..trip_count {
    if iter_idx == 0 {
      out_header.extend(template.iter().cloned());
      // Update for the next iteration using the original SSA names.
      for phi in header_phis {
        let next = match &phi.latch_arg {
          Arg::Var(v) => Arg::Var(*v),
          other => other.clone(),
        };
        cur_vals.insert(phi.tgt, next);
      }
      continue;
    }

    let mut local_defs = HashMap::<u32, Arg>::new();
    for (k, v) in cur_vals.iter() {
      local_defs.insert(*k, v.clone());
    }

    for inst in &template {
      let cloned = clone_with_renames(inst, &HashMap::new(), c_temp, &mut local_defs);
      out_header.push(cloned);
    }

    // Advance cur_vals for the next iteration.
    for phi in header_phis {
      let next = match &phi.latch_arg {
        Arg::Var(v) => local_defs.get(v).cloned().unwrap_or_else(|| Arg::Var(*v)),
        other => other.clone(),
      };
      cur_vals.insert(phi.tgt, next);
    }
  }

  // Rewrite uses of header phi targets outside the header block to the final values after unrolling.
  for label in cfg.graph.labels_sorted() {
    if label == header || label == body || label == latch {
      continue;
    }
    let block = cfg.bblocks.get_mut(label);
    for inst in block.iter_mut() {
      for arg in inst.args.iter_mut() {
        if let Arg::Var(v) = arg {
          if let Some(repl) = cur_vals.get(v) {
            *arg = repl.clone();
          }
        }
      }
    }
  }

  // Replace header block contents and rewrite control flow to a straight-line goto to exit.
  *cfg.bblocks.get_mut(header) = out_header;

  // Disconnect and remove loop blocks.
  cfg.graph.disconnect(header, body);
  cfg.graph.disconnect(body, latch);
  cfg.graph.disconnect(latch, header);

  cfg.pop(body);
  cfg.pop(latch);

  // Header now has a single successor (exit).
  debug_assert_eq!(cfg.terminator(header), Terminator::Goto(exit));
}

fn partial_unroll_by_2(
  cfg: &mut Cfg,
  header: u32,
  body: u32,
  latch: u32,
  header_phis: &[crate::analysis::loop_canon::LoopPhi],
  indvar: u32,
  indvar_latch_var: u32,
  cmp_op: BinOp,
  bound: &Arg,
  c_temp: &mut Counter,
  c_label: &mut Counter,
) {
  // Clone the original body template before mutating it.
  let body_template = cfg.bblocks.get(body).clone();

  // Move the induction step instruction out of the latch so it can be used for the second-iteration
  // guard. This only supports the simplest latch shape (one instruction: i_next = i + 1).
  let step_inst = cfg.bblocks.get_mut(latch).pop().expect("latch insts");
  debug_assert_eq!(step_inst.tgts.get(0).copied(), Some(indvar_latch_var));

  // Create the second-iteration block.
  let body2 = c_label.bump();
  cfg.graph.ensure_label(body2);

  // Extend the first body with: i1 = i + 1; cond2 = (i1 < bound); if cond2 goto body2 else latch
  {
    let body_block = cfg.bblocks.get_mut(body);
    body_block.push(step_inst);
    let cond2 = c_temp.bump();
    body_block.push(Inst::bin(
      cond2,
      Arg::Var(indvar_latch_var),
      cmp_op,
      bound.clone(),
    ));
    body_block.push(Inst::cond_goto(Arg::Var(cond2), body2, latch));
  }
  cfg.graph.connect(body, body2);

  // Build the second iteration body by cloning the original body template with phi vars substituted
  // to their post-first-iteration values.
  let mut entry_map = HashMap::<u32, Arg>::new();
  for phi in header_phis {
    if phi.tgt == indvar {
      entry_map.insert(phi.tgt, Arg::Var(indvar_latch_var));
    } else {
      entry_map.insert(phi.tgt, phi.latch_arg.clone());
    }
  }

  let mut body2_insts = Vec::<Inst>::new();
  let mut local_defs = HashMap::<u32, Arg>::new();
  for (k, v) in entry_map.iter() {
    local_defs.insert(*k, v.clone());
  }
  for inst in &body_template {
    let cloned = clone_with_renames(inst, &HashMap::new(), c_temp, &mut local_defs);
    body2_insts.push(cloned);
  }
  let i2 = c_temp.bump();
  body2_insts.push(Inst::bin(
    i2,
    Arg::Var(indvar_latch_var),
    BinOp::Add,
    Arg::Const(Const::Num(JsNumber(1.0))),
  ));
  cfg.bblocks.add(body2, body2_insts);
  cfg.graph.connect(body2, latch);

  // Insert latch phi nodes that merge {one-iter, two-iter} values, and update header phis to use them.
  let mut next_map = HashMap::<u32, u32>::new();
  let mut latch_phis = Vec::<Inst>::new();
  for phi in header_phis {
    let next = c_temp.bump();
    next_map.insert(phi.tgt, next);

    let from_body = if phi.tgt == indvar {
      Arg::Var(indvar_latch_var)
    } else {
      phi.latch_arg.clone()
    };

    let from_body2 = if phi.tgt == indvar {
      Arg::Var(i2)
    } else {
      match &phi.latch_arg {
        Arg::Var(v) => local_defs.get(v).cloned().unwrap_or_else(|| Arg::Var(*v)),
        other => other.clone(),
      }
    };

    let mut phi_inst = Inst::phi_empty(next);
    let mut entries = vec![(body, from_body), (body2, from_body2)];
    entries.sort_by_key(|(lbl, _)| *lbl);
    for (lbl, arg) in entries {
      phi_inst.insert_phi(lbl, arg);
    }
    latch_phis.push(phi_inst);
  }
  *cfg.bblocks.get_mut(latch) = latch_phis;

  {
    let header_block = cfg.bblocks.get_mut(header);
    for inst in header_block.iter_mut() {
      if inst.t != InstTyp::Phi {
        break;
      }
      let tgt = inst.tgts[0];
      let Some(&next_var) = next_map.get(&tgt) else {
        continue;
      };
      for (lbl, arg) in inst.labels.iter().copied().zip(inst.args.iter_mut()) {
        if lbl == latch {
          *arg = Arg::Var(next_var);
        }
      }
    }
  }
}

pub fn optpass_loop_unroll(
  cfg: &mut Cfg,
  dom: &Dom,
  c_temp: &mut Counter,
  c_label: &mut Counter,
) -> PassResult {
  let mut result = PassResult::default();

  let loops = find_counted_loops(cfg, dom);
  for l in loops {
    // Restrict to the simplest 3-block loop shape for now.
    if l.nodes.len() != 3 {
      continue;
    }

    if cfg.graph.parents_sorted(l.body) != vec![l.header]
      || cfg.graph.children_sorted(l.body) != vec![l.latch]
    {
      continue;
    }
    if cfg.graph.parents_sorted(l.latch) != vec![l.body]
      || cfg.graph.children_sorted(l.latch) != vec![l.header]
    {
      continue;
    }

    if cfg.terminator(l.body) != Terminator::Goto(l.latch) {
      continue;
    }
    if cfg.terminator(l.latch) != Terminator::Goto(l.header) {
      continue;
    }

    let body_block = cfg.bblocks.get(l.body);
    let latch_block = cfg.bblocks.get(l.latch);
    if !block_is_unrollable(body_block) || !block_is_unrollable(latch_block) {
      continue;
    }

    let template_len = body_block.len() + latch_block.len();

    // Full unroll when the trip count is small and compile-time known.
    if let Some(trip_count) = l.trip_count {
      if trip_count <= FULL_UNROLL_MAX_TRIP_COUNT {
        // Also keep the loop header small; we inline the whole unrolled body into it.
        if template_len * (trip_count as usize) > FULL_UNROLL_MAX_TOTAL_CLONED_INSTS {
          continue;
        }
        // Header phis must be present for SSA correctness.
        if l.header_phis.is_empty() {
          continue;
        }
        // Only unroll when the header contains just {phis, compare, cond_goto}. We drop the compare
        // and branch when linearizing the body.
        let header_block = cfg.bblocks.get(l.header);
        let phi_count = header_block
          .iter()
          .take_while(|inst| inst.t == InstTyp::Phi)
          .count();
        if phi_count != l.header_phis.len() || header_block.len() != phi_count + 2 {
          continue;
        }
        if header_block[phi_count].t != InstTyp::Bin
          || !matches!(header_block[phi_count].bin_op, BinOp::Lt | BinOp::Leq)
        {
          continue;
        }
        if header_block[phi_count + 1].t != InstTyp::CondGoto {
          continue;
        }
        let cond_var = header_block[phi_count + 1].args[0].maybe_var();
        if cond_var != header_block[phi_count].tgts.get(0).copied() {
          continue;
        }
        full_unroll_simple_loop(
          cfg,
          l.header,
          l.body,
          l.latch,
          l.exit,
          &l.header_phis,
          trip_count,
          c_temp,
        );
        result.mark_cfg_changed();
        return result;
      }
    }

    // Partial unroll by 2 for simple loops with unknown trip count.
    if template_len > PARTIAL_UNROLL_MAX_BODY_INSTS {
      continue;
    }
    // Require the latch to be exactly the induction increment so we can safely duplicate it.
    if latch_block.len() != 1 {
      continue;
    }
    let step = &latch_block[0];
    if step.t != InstTyp::Bin || step.bin_op != BinOp::Add {
      continue;
    }
    if step.tgts.get(0).copied() != Some(l.indvar_latch_var) {
      continue;
    }
    // Ensure the step uses the header induction var and constant +1.
    let one = Arg::Const(Const::Num(JsNumber(1.0)));
    let (left, right) = (&step.args[0], &step.args[1]);
    if !((left == &Arg::Var(l.indvar) && right == &one)
      || (left == &one && right == &Arg::Var(l.indvar)))
    {
      continue;
    }

    partial_unroll_by_2(
      cfg,
      l.header,
      l.body,
      l.latch,
      &l.header_phis,
      l.indvar,
      l.indvar_latch_var,
      l.cmp_op,
      &l.bound,
      c_temp,
      c_label,
    );
    result.mark_cfg_changed();
    return result;
  }

  result
}
