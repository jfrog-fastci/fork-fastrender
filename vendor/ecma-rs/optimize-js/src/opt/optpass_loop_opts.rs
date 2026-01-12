use crate::analysis::find_loops::find_loops;
use crate::analysis::range::{analyze_ranges, Bound, IntRange as RangeIntRange, RangeResult};
use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use crate::opt::PassResult;
use ahash::{HashMap, HashMapExt, HashSet};
use parse_js::num::JsNumber;
use std::collections::BTreeSet;

const MAX_FULL_UNROLL_TRIP_COUNT: usize = 8;

fn next_unused_label(cfg: &Cfg) -> u32 {
  let max = cfg.graph.labels().max().unwrap_or(cfg.entry);
  max.checked_add(1).expect("label overflow in loop opts")
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
  max.checked_add(1).expect("var overflow in loop opts")
}

fn alloc_label(next: &mut u32) -> u32 {
  let out = *next;
  *next = next.checked_add(1).expect("label overflow in loop opts");
  out
}

fn alloc_var(next: &mut u32) -> u32 {
  let out = *next;
  *next = next.checked_add(1).expect("var overflow in loop opts");
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

fn maybe_i64_const(c: &Const) -> Option<i64> {
  let Const::Num(JsNumber(n)) = c else {
    return None;
  };
  if !n.is_finite() || n.trunc() != *n {
    return None;
  }
  if *n < i64::MIN as f64 || *n > i64::MAX as f64 {
    return None;
  }
  let as_i64 = *n as i64;
  if as_i64 as f64 != *n {
    return None;
  }
  Some(as_i64)
}

fn i64_to_const(i: i64) -> Option<Const> {
  let f = i as f64;
  if !f.is_finite() || f.trunc() != f {
    return None;
  }
  if f < i64::MIN as f64 || f > i64::MAX as f64 {
    return None;
  }
  if f as i64 != i {
    return None;
  }
  Some(Const::Num(JsNumber(f)))
}

fn singleton_i64_from_range(r: RangeIntRange) -> Option<i64> {
  match r {
    RangeIntRange::Interval {
      lo: Bound::I64(a),
      hi: Bound::I64(b),
    } if a == b => Some(a),
    _ => None,
  }
}

fn arg_i64_on_edge(range: &RangeResult, pred: u32, succ: u32, arg: &Arg) -> Option<i64> {
  match arg {
    Arg::Const(c) => maybe_i64_const(c),
    Arg::Var(v) => singleton_i64_from_range(range.range_of_var_on_edge(pred, succ, *v)),
    _ => None,
  }
}

fn arg_i64_at_entry(range: &RangeResult, label: u32, arg: &Arg) -> Option<i64> {
  match arg {
    Arg::Const(c) => maybe_i64_const(c),
    Arg::Var(v) => singleton_i64_from_range(range.range_of_var_at_entry(label, *v)),
    _ => None,
  }
}

fn invert_cmp(op: BinOp) -> Option<BinOp> {
  match op {
    BinOp::Lt => Some(BinOp::Geq),
    BinOp::Leq => Some(BinOp::Gt),
    BinOp::Gt => Some(BinOp::Leq),
    BinOp::Geq => Some(BinOp::Lt),
    _ => None,
  }
}

#[derive(Debug, Clone)]
struct InductionVarInfo {
  header: u32,
  var: u32,
  init: Arg,
  latch: u32,
  preheader: u32,
  step: i64,
  update_inst_idx: usize,
}

fn find_induction_var(
  cfg: &Cfg,
  header: u32,
  loop_nodes: &HashSet<u32>,
) -> Option<InductionVarInfo> {
  // Require a single outside predecessor and a single latch for determinism/conservatism.
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
  let preheader = outside_preds[0];

  let mut latches: Vec<u32> = cfg
    .graph
    .parents_sorted(header)
    .into_iter()
    .filter(|p| loop_nodes.contains(p))
    .collect();
  latches.sort_unstable();
  latches.dedup();
  if latches.len() != 1 {
    return None;
  }
  let latch = latches[0];

  if cfg.graph.children_sorted(preheader) != vec![header] {
    return None;
  }

  let header_bb = cfg.bblocks.get(header);
  // Consider each phi in order for determinism.
  for (_phi_idx, phi) in header_bb.iter().enumerate() {
    if phi.t != InstTyp::Phi {
      break;
    }
    debug_assert_eq!(phi.labels.len(), phi.args.len());
    if phi.labels.len() != 2 {
      continue;
    }
    let mut init: Option<Arg> = None;
    let mut next: Option<Arg> = None;
    for (&lbl, arg) in phi.labels.iter().zip(phi.args.iter()) {
      if lbl == preheader {
        init = Some(arg.clone());
      } else if lbl == latch {
        next = Some(arg.clone());
      }
    }
    let (Some(init), Some(next)) = (init, next) else {
      continue;
    };
    let Arg::Var(next_var) = next else {
      continue;
    };

    // Find the update instruction in the latch that defines `next_var`.
    let latch_bb = cfg.bblocks.get(latch);
    let update_inst_idx = latch_bb
      .iter()
      .position(|inst| inst.tgts.first() == Some(&next_var))?;
    let update_inst = &latch_bb[update_inst_idx];
    if update_inst.t != InstTyp::Bin {
      continue;
    }
    let (_tgt, left, op, right) = update_inst.as_bin();

    let (phi_var, step): (u32, i64) = match op {
      BinOp::Add => match (left, right) {
        (Arg::Var(v), Arg::Const(c)) | (Arg::Const(c), Arg::Var(v)) => {
          let Some(k) = maybe_i64_const(c) else {
            continue;
          };
          (*v, k)
        }
        _ => continue,
      },
      BinOp::Sub => match (left, right) {
        (Arg::Var(v), Arg::Const(c)) => {
          let Some(k) = maybe_i64_const(c) else {
            continue;
          };
          (*v, k.checked_neg()?)
        }
        _ => continue,
      },
      _ => continue,
    };

    // Ensure the update uses the phi value (classic i = phi(..., i + step)).
    if phi_var != phi.tgts[0] {
      continue;
    }

    if step == 0 {
      continue;
    }

    return Some(InductionVarInfo {
      header,
      var: phi_var,
      init,
      latch,
      preheader,
      step,
      update_inst_idx,
    });
  }

  None
}

#[derive(Debug, Clone)]
struct CountedLoopInfo {
  header: u32,
  preheader: u32,
  exit: u32,
  body: u32,
  induction: InductionVarInfo,
  init_i64: i64,
  trip_count: usize,
}

fn infer_counted_loop(
  cfg: &Cfg,
  header: u32,
  loop_nodes: &HashSet<u32>,
  range: &RangeResult,
) -> Option<CountedLoopInfo> {
  // Currently only support the simplest 2-block natural loop:
  //   preheader -> header -> body(latch) -> header
  //             \-> exit
  if loop_nodes.len() != 2 {
    return None;
  }

  let induction = find_induction_var(cfg, header, loop_nodes)?;
  let preheader = induction.preheader;
  let latch = induction.latch;

  if !(loop_nodes.contains(&header) && loop_nodes.contains(&latch)) {
    return None;
  }

  let header_bb = cfg.bblocks.get(header);
  let term = header_bb.last()?;
  if term.t != InstTyp::CondGoto {
    return None;
  }
  let (cond, t, f) = term.as_cond_goto();
  let inside = loop_nodes.contains(&t) as u8 + loop_nodes.contains(&f) as u8;
  if inside != 1 {
    return None;
  }
  let (body, exit, continue_when_true) = if loop_nodes.contains(&t) {
    (t, f, true)
  } else {
    (f, t, false)
  };

  if body != latch {
    return None;
  }

  if cfg.graph.children_sorted(latch) != vec![header] {
    return None;
  }

  // Locate comparison defining the branch condition.
  let cond_var = cond.maybe_var()?;
  let cmp_inst = header_bb
    .iter()
    .rev()
    .skip(1) // skip CondGoto itself
    .find(|inst| inst.tgts.first() == Some(&cond_var))?;
  if cmp_inst.t != InstTyp::Bin {
    return None;
  }
  let (_cmp_tgt, left, op, right) = cmp_inst.as_bin();
  let op = match op {
    BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq => op,
    _ => return None,
  };

  // Canonicalize to `i <op> bound`.
  let (mut cmp_op, bound_arg) = match (left, right) {
    (Arg::Var(v), bound) if *v == induction.var => (op, bound.clone()),
    (bound, Arg::Var(v)) if *v == induction.var => {
      let flipped = match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Leq => BinOp::Geq,
        BinOp::Gt => BinOp::Lt,
        BinOp::Geq => BinOp::Leq,
        _ => return None,
      };
      (flipped, bound.clone())
    }
    _ => return None,
  };

  if !continue_when_true {
    cmp_op = invert_cmp(cmp_op)?;
  }

  let init_i64 = arg_i64_on_edge(range, preheader, header, &induction.init)?;
  let bound_i64 = arg_i64_at_entry(range, header, &bound_arg)?;
  let step = induction.step;

  let trip_count_i128: i128 = match cmp_op {
    BinOp::Lt => {
      if step <= 0 {
        return None;
      }
      if (init_i64 as i128) >= (bound_i64 as i128) {
        0
      } else {
        let diff = (bound_i64 as i128) - (init_i64 as i128);
        let step = step as i128;
        (diff + step - 1) / step
      }
    }
    BinOp::Leq => {
      if step <= 0 {
        return None;
      }
      if (init_i64 as i128) > (bound_i64 as i128) {
        0
      } else {
        let diff = (bound_i64 as i128) - (init_i64 as i128);
        let step = step as i128;
        diff / step + 1
      }
    }
    BinOp::Gt => {
      if step >= 0 {
        return None;
      }
      if (init_i64 as i128) <= (bound_i64 as i128) {
        0
      } else {
        let diff = (init_i64 as i128) - (bound_i64 as i128);
        let step = (-step) as i128;
        (diff + step - 1) / step
      }
    }
    BinOp::Geq => {
      if step >= 0 {
        return None;
      }
      if (init_i64 as i128) < (bound_i64 as i128) {
        0
      } else {
        let diff = (init_i64 as i128) - (bound_i64 as i128);
        let step = (-step) as i128;
        diff / step + 1
      }
    }
    _ => return None,
  };

  let trip_count: usize = trip_count_i128.try_into().ok()?;
  if trip_count > MAX_FULL_UNROLL_TRIP_COUNT {
    return None;
  }

  Some(CountedLoopInfo {
    header,
    preheader,
    exit,
    body,
    induction,
    init_i64,
    trip_count,
  })
}

fn remap_meta_vars(
  meta: &mut crate::il::meta::InstMeta,
  var_map: &HashMap<u32, u32>,
  replaced_with_const: &HashSet<u32>,
) {
  if let Some(hint) = meta.in_place_hint.as_mut() {
    use crate::il::meta::InPlaceHint;
    match hint {
      InPlaceHint::MoveNoClone { src, tgt } => {
        if replaced_with_const.contains(src) || replaced_with_const.contains(tgt) {
          meta.in_place_hint = None;
        } else {
          if let Some(n) = var_map.get(src).copied() {
            *src = n;
          }
          if let Some(n) = var_map.get(tgt).copied() {
            *tgt = n;
          }
        }
      }
    }
  }

  if let Some(narrow) = meta.nullability_narrowing.as_mut() {
    if replaced_with_const.contains(&narrow.var) {
      meta.nullability_narrowing = None;
    } else if let Some(n) = var_map.get(&narrow.var).copied() {
      narrow.var = n;
    }
  }
}

fn remap_inst(
  inst: &mut Inst,
  var_map: &HashMap<u32, u32>,
  replaced_with_const: &HashMap<u32, Arg>,
) {
  for tgt in inst.tgts.iter_mut() {
    if let Some(n) = var_map.get(tgt).copied() {
      *tgt = n;
    }
  }
  for arg in inst.args.iter_mut() {
    if let Arg::Var(v) = arg {
      if let Some(repl) = replaced_with_const.get(v) {
        *arg = repl.clone();
      } else if let Some(n) = var_map.get(v).copied() {
        *arg = Arg::Var(n);
      }
    }
  }

  let mut replaced = HashSet::default();
  replaced.extend(replaced_with_const.keys().copied());
  remap_meta_vars(&mut inst.meta, var_map, &replaced);
}

fn replace_var_in_cfg(cfg: &mut Cfg, var: u32, replacement: Arg) {
  for (_, bb) in cfg.bblocks.all_mut() {
    for inst in bb.iter_mut() {
      for arg in inst.args.iter_mut() {
        if matches!(arg, Arg::Var(v) if *v == var) {
          *arg = replacement.clone();
        }
      }

      // Also clear/adjust metadata that refers to this var explicitly.
      if let Some(hint) = inst.meta.in_place_hint.as_mut() {
        use crate::il::meta::InPlaceHint;
        match hint {
          InPlaceHint::MoveNoClone { src, tgt } => {
            if *src == var || *tgt == var {
              inst.meta.in_place_hint = None;
            }
          }
        }
      }
      if let Some(narrow) = inst.meta.nullability_narrowing.as_ref() {
        if narrow.var == var {
          inst.meta.nullability_narrowing = None;
        }
      }
    }
  }
}

fn unroll_counted_loop(
  cfg: &mut Cfg,
  info: &CountedLoopInfo,
  next_label: &mut u32,
  next_var: &mut u32,
) -> Option<()> {
  if info.trip_count == 0 {
    // TODO: Could simplify loop to direct jump to exit, but keep conservative for now.
    return None;
  }

  // Require that the header has exactly one phi node (the induction variable).
  {
    let header_bb = cfg.bblocks.get(info.header);
    let phi_count = header_bb.iter().take_while(|inst| inst.t == InstTyp::Phi).count();
    if phi_count != 1 {
      return None;
    }
  }

  let body_bb = cfg.bblocks.get(info.body);

  // Do not unroll bodies that contain SSA or control-flow instructions; we'd need to clone blocks.
  if body_bb.iter().any(|inst| inst.t == InstTyp::Phi || inst.t == InstTyp::CondGoto) {
    return None;
  }

  // Exclude the induction update instruction.
  let mut body_insts = Vec::<Inst>::new();
  for (idx, inst) in body_bb.iter().enumerate() {
    if idx == info.induction.update_inst_idx {
      continue;
    }
    body_insts.push(inst.clone());
  }

  // Emit all unrolled iterations into a fresh block for simplicity.
  let unrolled_label = alloc_label(next_label);
  cfg.graph.ensure_label(unrolled_label);

  let mut out_insts = Vec::<Inst>::new();
  for iter in 0..info.trip_count {
    let iter_i = (info.init_i64 as i128) + (info.induction.step as i128) * (iter as i128);
    let iter_i: i64 = iter_i.try_into().ok()?;
    let iter_const = i64_to_const(iter_i)?;
    let iter_arg = Arg::Const(iter_const);

    // Allocate fresh SSA vars for each target defined in the body (per-iteration, no cross-iter uses).
    let mut var_map = HashMap::<u32, u32>::new();
    for inst in &body_insts {
      for &tgt in &inst.tgts {
        var_map.entry(tgt).or_insert_with(|| alloc_var(next_var));
      }
    }

    let mut replaced_with_const = HashMap::<u32, Arg>::new();
    replaced_with_const.insert(info.induction.var, iter_arg);

    for inst in &body_insts {
      let mut cloned = inst.clone();
      remap_inst(&mut cloned, &var_map, &replaced_with_const);
      out_insts.push(cloned);
    }
  }

  // Replace uses of the induction variable after the loop with the known final value.
  let final_i = (info.init_i64 as i128) + (info.induction.step as i128) * (info.trip_count as i128);
  let final_i: i64 = final_i.try_into().ok()?;
  let final_const = i64_to_const(final_i)?;
  replace_var_in_cfg(cfg, info.induction.var, Arg::Const(final_const));

  cfg.bblocks.add(unrolled_label, out_insts);

  // Rewire preheader -> header to preheader -> unrolled_label.
  rewrite_edge(cfg, info.preheader, info.header, unrolled_label);
  cfg.graph.connect(unrolled_label, info.exit);

  // Fix phi nodes in exit block: incoming edge now comes from `unrolled_label` instead of `header`.
  {
    let exit_bb = cfg.bblocks.get_mut(info.exit);
    for inst in exit_bb.iter_mut() {
      if inst.t != InstTyp::Phi {
        break;
      }
      rewrite_phi_incoming_label(inst, info.header, unrolled_label);
    }
  }

  // Disconnect and delete the old loop blocks.
  cfg.graph.disconnect(info.header, info.body);
  cfg.graph.disconnect(info.header, info.exit);
  cfg.graph.disconnect(info.body, info.header);

  cfg.pop(info.header);
  cfg.pop(info.body);

  Some(())
}

fn strength_reduce_mul_by_const(
  cfg: &mut Cfg,
  loop_nodes: &HashSet<u32>,
  induction: &InductionVarInfo,
  next_var: &mut u32,
) -> PassResult {
  let mut result = PassResult::default();

  let preheader = induction.preheader;
  let header = induction.header;
  let latch = induction.latch;

  // Require a simple latch (single successor back to the header).
  if cfg.graph.children_sorted(latch) != vec![header] {
    return result;
  }

  // Collect unique multipliers (deterministic ordering via BTreeSet).
  let mut multipliers = BTreeSet::<i64>::new();
  let mut loop_labels: Vec<u32> = loop_nodes.iter().copied().collect();
  loop_labels.sort_unstable();
  for label in loop_labels.iter().copied() {
    let bb = cfg.bblocks.get(label);
    for inst in bb {
      if inst.t != InstTyp::Bin || inst.bin_op != BinOp::Mul {
        continue;
      }
      let (_tgt, left, _op, right) = inst.as_bin();
      let k = match (left, right) {
        (Arg::Var(v), Arg::Const(c)) | (Arg::Const(c), Arg::Var(v)) if *v == induction.var => {
          maybe_i64_const(c)
        }
        _ => None,
      };
      let Some(k) = k else { continue };
      if k == 0 || k == 1 {
        continue;
      }
      multipliers.insert(k);
    }
  }

  if multipliers.is_empty() {
    return result;
  }

  // Track replacements of `mul_tgt -> sr_phi_var`.
  let mut replace_vars = HashMap::<u32, u32>::new();

  for &k in multipliers.iter() {
    let Some(k_const) = i64_to_const(k) else {
      continue;
    };
    let step_k_i128 = (induction.step as i128) * (k as i128);
    let Ok(step_k_i64) = i64::try_from(step_k_i128) else {
      continue;
    };
    let Some(step_k_const) = i64_to_const(step_k_i64) else {
      continue;
    };

    // Create vars: init, phi, next.
    let sr_init = alloc_var(next_var);
    let sr_phi = alloc_var(next_var);
    let sr_next = alloc_var(next_var);

    // Preheader init: sr_init = init * k
    let init_inst = match &induction.init {
      Arg::Const(c) => {
        let Some(init_i) = maybe_i64_const(c) else {
          continue;
        };
        let init_k_i128 = (init_i as i128) * (k as i128);
        let Ok(init_k_i64) = i64::try_from(init_k_i128) else {
          continue;
        };
        let Some(init_k_const) = i64_to_const(init_k_i64) else {
          continue;
        };
        Inst::var_assign(sr_init, Arg::Const(init_k_const))
      }
      other => Inst::bin(sr_init, other.clone(), BinOp::Mul, Arg::Const(k_const.clone())),
    };

    cfg.bblocks.get_mut(preheader).push(init_inst);

    // Insert phi in header (after existing phi nodes).
    {
      let header_bb = cfg.bblocks.get_mut(header);
      let phi_end = header_bb
        .iter()
        .position(|inst| inst.t != InstTyp::Phi)
        .unwrap_or(header_bb.len());
      let mut phi = Inst::phi_empty(sr_phi);
      // Deterministic incoming order.
      if preheader < latch {
        phi.insert_phi(preheader, Arg::Var(sr_init));
        phi.insert_phi(latch, Arg::Var(sr_next));
      } else {
        phi.insert_phi(latch, Arg::Var(sr_next));
        phi.insert_phi(preheader, Arg::Var(sr_init));
      }
      header_bb.insert(phi_end, phi);
    }

    // Insert update in latch: sr_next = sr_phi + (step*k)
    {
      let latch_bb = cfg.bblocks.get_mut(latch);
      latch_bb.push(Inst::bin(
        sr_next,
        Arg::Var(sr_phi),
        BinOp::Add,
        Arg::Const(step_k_const),
      ));
    }

    // Mark mul instructions for replacement.
    for label in loop_labels.iter().copied() {
      let bb = cfg.bblocks.get(label);
      for inst in bb {
        if inst.t != InstTyp::Bin || inst.bin_op != BinOp::Mul {
          continue;
        }
        let (tgt, left, _op, right) = inst.as_bin();
        let matched = match (left, right) {
          (Arg::Var(v), Arg::Const(c)) | (Arg::Const(c), Arg::Var(v)) if *v == induction.var => {
            maybe_i64_const(c) == Some(k)
          }
          _ => false,
        };
        if matched {
          replace_vars.insert(tgt, sr_phi);
        }
      }
    }
  }

  if replace_vars.is_empty() {
    return result;
  }

  let mut phi_meta_sources: Vec<(u32, crate::il::meta::InstMeta)> = Vec::new();

  // Apply replacements and remove the now-redundant mul instructions.
  for label in loop_labels.iter().copied() {
    let bb = cfg.bblocks.get_mut(label);
    let mut idx = 0usize;
    while idx < bb.len() {
      let mut remove = false;
      if bb[idx].t == InstTyp::Bin && bb[idx].bin_op == BinOp::Mul {
        let tgt = bb[idx].tgts[0];
        if replace_vars.contains_key(&tgt) {
          remove = true;
        }
      }

      if remove {
        let removed = bb.remove(idx);
        let tgt = removed.tgts[0];
        if let Some(&sr_phi) = replace_vars.get(&tgt) {
          phi_meta_sources.push((sr_phi, removed.meta.clone()));
        }
        result.mark_changed();
        continue;
      }

      // Rewrite uses.
      for arg in bb[idx].args.iter_mut() {
        if let Arg::Var(v) = arg {
          if let Some(&sr) = replace_vars.get(v) {
            *arg = Arg::Var(sr);
          }
        }
      }
      idx += 1;
    }
  }

  // Best-effort metadata preservation: copy result-value metadata from the first rewritten mul
  // onto the derived induction variable phi.
  for (sr_phi, src_meta) in phi_meta_sources {
    let header_bb = cfg.bblocks.get_mut(header);
    let Some(phi_inst) = header_bb
      .iter_mut()
      .find(|inst| inst.t == InstTyp::Phi && inst.tgts.first() == Some(&sr_phi))
    else {
      continue;
    };
    if phi_inst.meta.type_id.is_none()
      && phi_inst.meta.type_summary.is_none()
      && phi_inst.meta.hir_expr.is_none()
    {
      phi_inst.meta.copy_result_var_metadata_from(&src_meta);
    }
  }

  result
}

pub fn optpass_loop_opts(cfg: &mut Cfg) -> PassResult {
  let mut result = PassResult::default();

  let mut next_label = next_unused_label(cfg);
  let mut next_var = next_unused_var(cfg);

  // Conservative fixpoint: keep applying at most one CFG-changing unroll at a time, then restart.
  loop {
    let dom = Dom::calculate(cfg);
    let loops = find_loops(cfg, &dom);
    if loops.is_empty() {
      break;
    }

    let range = analyze_ranges(cfg);

    let mut headers: Vec<u32> = loops.keys().copied().collect();
    headers.sort_unstable();

    let mut progress = false;

    // Attempt full unrolling first.
    for header in headers.iter().copied() {
      let loop_nodes = loops.get(&header).expect("loop header missing from map");

      let Some(counted) = infer_counted_loop(cfg, header, loop_nodes, &range) else {
        continue;
      };

      if unroll_counted_loop(cfg, &counted, &mut next_label, &mut next_var).is_some() {
        result.mark_cfg_changed();
        progress = true;
        break;
      }
    }

    if progress {
      continue;
    }

    // Strength reduction for remaining loops (does not change CFG).
    for header in headers.iter().copied() {
      let loop_nodes = loops.get(&header).expect("loop header missing from map");
      let Some(induction) = find_induction_var(cfg, header, loop_nodes) else {
        continue;
      };

      // Only handle loops with a canonical preheader as detected by `find_induction_var`.
      let sr = strength_reduce_mul_by_const(cfg, loop_nodes, &induction, &mut next_var);
      if sr.changed {
        result.merge(sr);
      }
    }

    break;
  }

  result
}
