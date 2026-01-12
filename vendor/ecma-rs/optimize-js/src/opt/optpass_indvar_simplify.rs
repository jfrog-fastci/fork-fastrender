use crate::analysis::loop_canon::find_counted_loops;
use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use crate::opt::PassResult;
use crate::util::counter::Counter;
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use parse_js::num::JsNumber;
use std::collections::BTreeMap;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StrengthKey {
  stride: i64,
  base: Option<Arg>,
}

#[derive(Clone, Copy)]
struct CandidateLoc {
  label: u32,
  inst_idx: usize,
  tgt: u32,
}

fn cfg_defs(cfg: &Cfg) -> HashMap<u32, (u32, usize)> {
  let mut defs = HashMap::<u32, (u32, usize)>::new();
  for (label, block) in cfg.bblocks.all() {
    for (idx, inst) in block.iter().enumerate() {
      for &tgt in &inst.tgts {
        defs.insert(tgt, (label, idx));
      }
    }
  }
  defs
}

fn maybe_i64_from_const(c: &Const) -> Option<i64> {
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

fn maybe_i64_from_arg(cfg: &Cfg, defs: &HashMap<u32, (u32, usize)>, arg: &Arg) -> Option<i64> {
  let mut cur = arg.clone();
  for _ in 0..8 {
    match cur {
      Arg::Const(c) => return maybe_i64_from_const(&c),
      Arg::Var(v) => {
        let Some(&(label, idx)) = defs.get(&v) else {
          return None;
        };
        let inst = &cfg.bblocks.get(label)[idx];
        if inst.t != InstTyp::VarAssign {
          return None;
        }
        cur = inst.args[0].clone();
      }
      _ => return None,
    }
  }
  None
}

fn safe_linear_expr_range(init: i64, trip_count: u64, stride: i64, base: i64) -> bool {
  // Avoid changing numeric semantics (overflow / float rounding) unless the linear expression
  // remains within JS' safe integer range.
  const MAX_SAFE_INT: i128 = 9_007_199_254_740_991; // 2^53 - 1

  if trip_count == 0 {
    return true;
  }
  let init_i = init as i128;
  let Some(last_i) = init_i
    .checked_add(trip_count as i128)
    .and_then(|v| v.checked_sub(1))
  else {
    return false;
  };
  let stride = stride as i128;
  let base = base as i128;

  let Some(v0) = init_i.checked_mul(stride).and_then(|v| base.checked_add(v)) else {
    return false;
  };
  let Some(v1) = last_i.checked_mul(stride).and_then(|v| base.checked_add(v)) else {
    return false;
  };
  let Some(a0) = v0.checked_abs() else {
    return false;
  };
  let Some(a1) = v1.checked_abs() else {
    return false;
  };
  a0.max(a1) <= MAX_SAFE_INT
}

fn is_const_i64_arg(arg: &Arg) -> Option<i64> {
  match arg {
    Arg::Const(c) => maybe_i64_from_const(c),
    _ => None,
  }
}

fn is_mul_by_indvar_and_const_stride(inst: &Inst, indvar: u32) -> Option<i64> {
  if inst.t != InstTyp::Bin || inst.bin_op != BinOp::Mul {
    return None;
  }
  let (_tgt, left, _op, right) = inst.as_bin();
  let (var, cst) = match (left, right) {
    (Arg::Var(v), Arg::Const(c)) | (Arg::Const(c), Arg::Var(v)) => (*v, c),
    _ => return None,
  };
  if var != indvar {
    return None;
  }
  maybe_i64_from_const(cst)
}

fn block_use_counts(block: &[Inst], counts: &mut HashMap<u32, u32>) {
  for inst in block {
    for arg in &inst.args {
      if let Arg::Var(v) = arg {
        *counts.entry(*v).or_insert(0) += 1;
      }
    }
  }
}

pub fn optpass_indvar_simplify(cfg: &mut Cfg, dom: &Dom, c_temp: &mut Counter) -> PassResult {
  let mut result = PassResult::default();

  let loops = find_counted_loops(cfg, dom);
  if loops.is_empty() {
    return result;
  }
  let defs = cfg_defs(cfg);

  for l in loops {
    let Some(trip_count) = l.trip_count else {
      continue;
    };
    if trip_count < 2 {
      continue;
    }
    // Hard cap per loop so we don't accidentally explode SSA with many accumulators.
    const MAX_ACCUMULATORS_PER_LOOP: usize = 8;

    let Some(init_i) = maybe_i64_from_arg(cfg, &defs, &l.indvar_init) else {
      continue;
    };

    let mut use_counts = HashMap::<u32, u32>::new();
    for &label in &l.nodes {
      let Some(block) = cfg.bblocks.maybe_get(label) else {
        continue;
      };
      block_use_counts(block, &mut use_counts);
    }

    // First, collect mul defs (%t = i * stride).
    let mut mul_defs = HashMap::<u32, (i64, CandidateLoc)>::new();
    for &label in &l.nodes {
      let Some(block) = cfg.bblocks.maybe_get(label) else {
        continue;
      };
      for (idx, inst) in block.iter().enumerate() {
        let Some(stride) = is_mul_by_indvar_and_const_stride(inst, l.indvar) else {
          continue;
        };
        // Skip trivial cases; DVN can fold const cases, and stride=±1 is cheaper to handle via
        // algebraic simplification rather than an accumulator.
        if stride == 0 || stride == 1 || stride == -1 {
          continue;
        }
        let tgt = inst.tgts.get(0).copied().unwrap_or(u32::MAX);
        if tgt == u32::MAX {
          continue;
        }
        mul_defs.insert(
          tgt,
          (
            stride,
            CandidateLoc {
              label,
              inst_idx: idx,
              tgt,
            },
          ),
        );
      }
    }

    if mul_defs.is_empty() {
      continue;
    }

    // Next, prefer folding `base + (i*stride)` into a single accumulator when the mul is single-use.
    let mut consumed_mul = HashSet::<u32>::new();
    let mut candidates = BTreeMap::<StrengthKey, Vec<CandidateLoc>>::new();
    for &label in &l.nodes {
      let Some(block) = cfg.bblocks.maybe_get(label) else {
        continue;
      };
      for (idx, inst) in block.iter().enumerate() {
        if inst.t != InstTyp::Bin || inst.bin_op != BinOp::Add {
          continue;
        }
        let tgt = inst.tgts.get(0).copied().unwrap_or(u32::MAX);
        if tgt == u32::MAX {
          continue;
        }
        let left = &inst.args[0];
        let right = &inst.args[1];
        let (mul_tgt, base) = match (left, right) {
          (Arg::Var(m), b) | (b, Arg::Var(m)) => (*m, b),
          _ => continue,
        };
        let Some((stride, _mul_loc)) = mul_defs.get(&mul_tgt).copied() else {
          continue;
        };
        if use_counts.get(&mul_tgt).copied().unwrap_or(0) != 1 {
          continue;
        }
        let Some(_base_i64) = is_const_i64_arg(base) else {
          continue;
        };
        consumed_mul.insert(mul_tgt);
        candidates
          .entry(StrengthKey {
            stride,
            base: Some(base.clone()),
          })
          .or_default()
          .push(CandidateLoc {
            label,
            inst_idx: idx,
            tgt,
          });
      }
    }

    // Remaining `i*stride` multiplies become their own accumulator.
    for (mul_tgt, (stride, loc)) in mul_defs {
      if consumed_mul.contains(&mul_tgt) {
        continue;
      }
      candidates
        .entry(StrengthKey { stride, base: None })
        .or_default()
        .push(loc);
    }

    if candidates.is_empty() {
      continue;
    }

    // Deterministic processing + cap.
    let mut created = 0usize;
    for (key, locs) in candidates {
      if created >= MAX_ACCUMULATORS_PER_LOOP {
        break;
      }

      let stride = key.stride;
      let base_i64 = key.base.as_ref().and_then(is_const_i64_arg).unwrap_or(0);

      if !safe_linear_expr_range(init_i, trip_count, stride, base_i64) {
        continue;
      }

      // Build the accumulator:
      //   preheader: acc_init = base + (init_i * stride)
      //   header:    acc = phi { preheader: acc_init, latch: acc_next }
      //   latch:     acc_next = acc + stride
      let acc_init = c_temp.bump();
      let acc_phi = c_temp.bump();
      let acc_next = c_temp.bump();

      {
        let pre = cfg.bblocks.get_mut(l.preheader);
        if let Some(base) = &key.base {
          let mul_init = c_temp.bump();
          pre.push(Inst::bin(
            mul_init,
            l.indvar_init.clone(),
            BinOp::Mul,
            Arg::Const(Const::Num(JsNumber(stride as f64))),
          ));
          pre.push(Inst::bin(
            acc_init,
            base.clone(),
            BinOp::Add,
            Arg::Var(mul_init),
          ));
        } else {
          pre.push(Inst::bin(
            acc_init,
            l.indvar_init.clone(),
            BinOp::Mul,
            Arg::Const(Const::Num(JsNumber(stride as f64))),
          ));
        }
      }

      {
        let header = cfg.bblocks.get_mut(l.header);
        let phi_end = header
          .iter()
          .position(|inst| inst.t != InstTyp::Phi)
          .unwrap_or(header.len());
        let mut phi = Inst::phi_empty(acc_phi);
        let mut entries = vec![
          (l.preheader, Arg::Var(acc_init)),
          (l.latch, Arg::Var(acc_next)),
        ];
        entries.sort_by_key(|(lbl, _)| *lbl);
        for (lbl, arg) in entries {
          phi.insert_phi(lbl, arg);
        }
        header.insert(phi_end, phi);
      }

      {
        let latch = cfg.bblocks.get_mut(l.latch);
        latch.push(Inst::bin(
          acc_next,
          Arg::Var(acc_phi),
          BinOp::Add,
          Arg::Const(Const::Num(JsNumber(stride as f64))),
        ));
      }

      // Rewrite candidate instructions to copies from the accumulator.
      for loc in locs {
        let block = cfg.bblocks.get_mut(loc.label);
        let inst = &mut block[loc.inst_idx];
        debug_assert_eq!(inst.tgts.get(0).copied(), Some(loc.tgt));
        *inst = Inst::var_assign(loc.tgt, Arg::Var(acc_phi));
      }

      created += 1;
      result.mark_changed();
    }
  }

  result
}
