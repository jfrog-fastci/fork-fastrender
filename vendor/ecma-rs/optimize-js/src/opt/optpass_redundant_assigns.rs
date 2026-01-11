use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, InstTyp, ValueTypeSummary};
use crate::opt::PassResult;
use ahash::HashMap;
use ahash::HashMapExt;

// VarAssigns are always useless in strict SSA. However, dominator-based value numbering doesn't manage to detect and remove all such insts, with one reason being that DVNT only traverses domtree children.
// My theory for correctness:
// - Strict SSA requires all defs to dominate all their uses.
// - Targets are only assigned in one place globally.
pub fn optpass_redundant_assigns(cfg: &mut Cfg) -> PassResult {
  let mut result = PassResult::default();
  let mut tgt_to_arg = HashMap::new();
  // When typed IL metadata is available we want to preserve it through copy
  // propagation. A VarAssign like `%x = %y` is eliminated by this pass, so any
  // type summary on `%x` needs to be transferred to `%y`'s defining instruction.
  let mut propagate_types = HashMap::<u32, ValueTypeSummary>::new();
  for (_, bblock) in cfg.bblocks.all_mut() {
    let mut to_delete = Vec::new();
    for (i, inst) in bblock.iter().enumerate() {
      if inst.t != InstTyp::VarAssign {
        continue;
      }
      // Typed identifier reads materialize as `VarAssign` copies so they can carry
      // per-expression type metadata (flow narrowing, parameter types, etc). Keep
      // those around; they are not redundant from the perspective of downstream
      // analyses.
      if inst.meta.preserve_var_assign {
        continue;
      }
      let (tgt, value) = inst.as_var_assign();
      if !inst.value_type.is_unknown() {
        if let Arg::Var(rhs) = value {
          propagate_types
            .entry(*rhs)
            .and_modify(|existing| *existing |= inst.value_type)
            .or_insert(inst.value_type);
        }
      }
      to_delete.push(i);
      assert!(tgt_to_arg.insert(tgt, value.clone()).is_none());
    }
    for i in to_delete.into_iter().rev() {
      bblock.remove(i);
      result.mark_changed();
    }
  }
  for (_, bblock) in cfg.bblocks.all_mut() {
    for inst in bblock.iter_mut() {
      for arg in inst.args.iter_mut() {
        let Arg::Var(t) = arg else {
          continue;
        };
        let Some(new_arg) = tgt_to_arg.get(t) else {
          continue;
        };
        *arg = new_arg.clone();
      }
    }
  }
  if !propagate_types.is_empty() {
    for (_, bblock) in cfg.bblocks.all_mut() {
      for inst in bblock.iter_mut() {
        for tgt in inst.tgts.iter() {
          if let Some(extra) = propagate_types.get(tgt).copied() {
            inst.value_type |= extra;
          }
        }
      }
    }
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{CfgBBlocks, CfgGraph};
  use crate::il::inst::{BinOp, Const, Inst};
  use parse_js::num::JsNumber;

  #[test]
  fn preserves_marked_var_assigns() {
    let mut graph = CfgGraph::default();
    graph.ensure_label(0);
    let mut bblocks = CfgBBlocks::default();
    let mut preserved = Inst::var_assign(3, Arg::Var(0));
    preserved.meta.preserve_var_assign = true;
    bblocks.add(
      0,
      vec![
        Inst::var_assign(1, Arg::Var(0)),
        Inst::bin(
          2,
          Arg::Var(1),
          BinOp::Add,
          Arg::Const(Const::Num(JsNumber(1.0))),
        ),
        preserved,
        Inst::bin(
          4,
          Arg::Var(3),
          BinOp::Add,
          Arg::Const(Const::Num(JsNumber(1.0))),
        ),
      ],
    );
    let mut cfg = Cfg {
      graph,
      bblocks,
      entry: 0,
    };

    let result = optpass_redundant_assigns(&mut cfg);
    assert!(result.changed);

    let block = cfg.bblocks.get(0);
    assert_eq!(block.len(), 3, "expected one redundant VarAssign to be removed");

    let first_bin = &block[0];
    assert_eq!(first_bin.args.get(0), Some(&Arg::Var(0)));

    let preserved_assign = &block[1];
    assert_eq!(preserved_assign.t, InstTyp::VarAssign);
    assert!(preserved_assign.meta.preserve_var_assign);

    let second_bin = &block[2];
    assert_eq!(second_bin.args.get(0), Some(&Arg::Var(3)));
  }
}
