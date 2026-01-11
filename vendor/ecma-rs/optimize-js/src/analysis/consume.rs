use crate::analysis::liveness::calculate_live_outs;
use crate::analysis::ownership::OwnershipResult;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, ArgUseMode, InPlaceHint, InstTyp, OwnershipState};
use ahash::{HashMap, HashSet};

fn is_consume_site(inst: &crate::il::inst::Inst, arg_idx: usize) -> bool {
  match inst.t {
    InstTyp::VarAssign => arg_idx == 0,
    InstTyp::PropAssign => arg_idx == 2,
    InstTyp::Call => arg_idx >= 1, // this + call args; callee is always borrowed
    InstTyp::Return | InstTyp::Throw => arg_idx == 0,
    InstTyp::ForeignStore | InstTyp::UnknownStore => arg_idx == 0,
    _ => false,
  }
}

fn ownership_of_var(ownership: &OwnershipResult, var: u32) -> OwnershipState {
  ownership
    .get(&var)
    .copied()
    .unwrap_or(OwnershipState::Unknown)
}
 
fn should_consume_var(var: u32, live_out: &HashSet<u32>, ownership: &OwnershipResult) -> bool {
  ownership_of_var(ownership, var) == OwnershipState::Owned && !live_out.contains(&var)
}
 
pub fn annotate_cfg_consumption(cfg: &mut Cfg, ownership: &OwnershipResult) {
  let live_outs = calculate_live_outs(cfg, &HashMap::default(), &HashSet::default());
  let empty_live_out: HashSet<u32> = HashSet::default();

  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  
  for label in labels {
    let insts_len = cfg.bblocks.get(label).len();
    for inst_idx in 0..insts_len {
      let live_out = live_outs.get(&(label, inst_idx)).unwrap_or(&empty_live_out);
  
      let inst = &mut cfg.bblocks.get_mut(label)[inst_idx];
      inst.meta.in_place_hint = None;
 
      if inst.args.is_empty() {
        inst.meta.arg_use_modes.clear();
        continue;
      }

      let mut modes = vec![ArgUseMode::Borrow; inst.args.len()];
      let mut any_consume = false;

      for (idx, arg) in inst.args.iter().enumerate() {
        if !is_consume_site(inst, idx) {
          continue;
        }
        let Arg::Var(var) = arg else {
          continue;
        };
        if should_consume_var(*var, &live_out, ownership) {
          modes[idx] = ArgUseMode::Consume;
          any_consume = true;
        }
      }

      if inst.t == InstTyp::VarAssign && modes.get(0) == Some(&ArgUseMode::Consume) {
        if let (Some(Arg::Var(src)), Some(&tgt)) = (inst.args.get(0), inst.tgts.get(0)) {
          inst.meta.in_place_hint = Some(InPlaceHint::MoveNoClone { src: *src, tgt });
        }
      }
      if any_consume {
        inst.meta.arg_use_modes = modes;
      } else {
        inst.meta.arg_use_modes.clear();
      }
      debug_assert!(
        inst.meta.arg_use_modes.is_empty() || inst.meta.arg_use_modes.len() == inst.args.len(),
        "InstMeta.arg_use_modes must be empty (all Borrow) or aligned with Inst.args"
      );
    }
  }
}
 
#[cfg(test)]
mod tests {
  use super::*;
  use crate::analysis::ownership;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
  use crate::il::inst::{Arg, Const, Inst};
 
  fn cfg_with_blocks(blocks: &[(u32, Vec<Inst>)], edges: &[(u32, u32)]) -> Cfg {
    let labels: Vec<u32> = blocks.iter().map(|(label, _)| *label).collect();
    let mut graph = CfgGraph::default();
    for &(from, to) in edges {
      graph.connect(from, to);
    }
    for &label in &labels {
      if !graph.contains(label) {
        graph.connect(label, label);
        graph.disconnect(label, label);
      }
    }
    let mut bblocks = CfgBBlocks::default();
    for (label, insts) in blocks.iter() {
      bblocks.add(*label, insts.clone());
    }
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }
 
  fn mode_at(inst: &crate::il::inst::Inst, idx: usize) -> ArgUseMode {
    inst
      .meta
      .arg_use_modes
      .get(idx)
      .copied()
      .unwrap_or(ArgUseMode::Borrow)
  }
 
  #[test]
  fn call_last_use_consumes_arg() {
    let mut cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::call(
            1,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(0)],
            vec![],
          ),
          Inst::ret(None),
        ],
      )],
      &[],
    );
 
    let ownership = ownership::analyze_cfg_ownership(&cfg);
    annotate_cfg_consumption(&mut cfg, &ownership);
 
    let call = &cfg.bblocks.get(0)[1];
    assert_eq!(mode_at(call, 2), ArgUseMode::Consume);
  }
 
  #[test]
  fn earlier_use_is_borrow_if_value_is_used_again() {
    let mut cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::call(
            1,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(0)],
            vec![],
          ),
          Inst::call(
            2,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(0)],
            vec![],
          ),
          Inst::ret(None),
        ],
      )],
      &[],
    );
 
    let ownership = ownership::analyze_cfg_ownership(&cfg);
    annotate_cfg_consumption(&mut cfg, &ownership);
 
    let call1 = &cfg.bblocks.get(0)[1];
    let call2 = &cfg.bblocks.get(0)[2];
    assert_eq!(mode_at(call1, 2), ArgUseMode::Borrow);
    assert_eq!(mode_at(call2, 2), ArgUseMode::Consume);
  }
 
  #[test]
  fn consumes_in_disjoint_terminal_branches() {
    let mut cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::call(
              0,
              Arg::Builtin("__optimize_js_object".to_string()),
              Arg::Const(Const::Undefined),
              vec![],
              vec![],
            ),
            Inst::cond_goto(Arg::Var(99), 1, 2),
          ],
        ),
        (
          1,
          vec![
            Inst::call(
              1,
              Arg::Builtin("__optimize_js_array".to_string()),
              Arg::Const(Const::Undefined),
              vec![Arg::Var(0)],
              vec![],
            ),
            Inst::ret(None),
          ],
        ),
        (
          2,
          vec![
            Inst::call(
              2,
              Arg::Builtin("__optimize_js_array".to_string()),
              Arg::Const(Const::Undefined),
              vec![Arg::Var(0)],
              vec![],
            ),
            Inst::ret(None),
          ],
        ),
      ],
      &[(0, 1), (0, 2)],
    );
 
    let ownership = ownership::analyze_cfg_ownership(&cfg);
    annotate_cfg_consumption(&mut cfg, &ownership);
 
    let call_t = &cfg.bblocks.get(1)[0];
    let call_f = &cfg.bblocks.get(2)[0];
    assert_eq!(mode_at(call_t, 2), ArgUseMode::Consume);
    assert_eq!(mode_at(call_f, 2), ArgUseMode::Consume);
  }
 
  #[test]
  fn return_consumes_owned_value() {
    let mut cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::ret(Some(Arg::Var(0))),
        ],
      )],
      &[],
    );

    let ownership = ownership::analyze_cfg_ownership(&cfg);
    annotate_cfg_consumption(&mut cfg, &ownership);

    let ret = &cfg.bblocks.get(0)[1];
    assert_eq!(mode_at(ret, 0), ArgUseMode::Consume);
  }

  #[test]
  fn throw_consumes_owned_value() {
    let mut cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::throw(Arg::Var(0)),
        ],
      )],
      &[],
    );

    let ownership = ownership::analyze_cfg_ownership(&cfg);
    annotate_cfg_consumption(&mut cfg, &ownership);

    let thr = &cfg.bblocks.get(0)[1];
    assert_eq!(mode_at(thr, 0), ArgUseMode::Consume);
  }
}
