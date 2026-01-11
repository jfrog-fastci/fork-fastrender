use super::alias::{self, AbstractLoc};
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, InstTyp};
use std::collections::{BTreeMap, BTreeSet};

/// Escape classification for allocations local to a function.
///
/// This analysis is intraprocedural and conservative. It focuses on allocations created by the
/// internal literal builtins (`__optimize_js_array`, `__optimize_js_object`, `__optimize_js_regex`)
/// and determines whether they remain local to the function or may become reachable outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EscapeState {
  /// No observed escape.
  NoEscape,
  /// The allocation becomes reachable from an input/parameter object.
  ///
  /// The payload is currently the SSA/temp variable representing that parameter object.
  ArgEscape(u32),
  /// Escapes by being returned from the current function.
  ///
  /// Note: `optimize-js` IR does not currently model return values explicitly; this is kept for
  /// forward compatibility.
  ReturnEscape,
  /// Escapes to global/outer scope storage (e.g. `ForeignStore`/`UnknownStore`).
  GlobalEscape,
  /// Escapes in an unknown way.
  Unknown,
}

impl EscapeState {
  /// Deterministic, conservative join ("worse wins").
  pub fn join(self, other: Self) -> Self {
    use EscapeState::*;
    match (self, other) {
      (Unknown, _) | (_, Unknown) => Unknown,
      (GlobalEscape, _) | (_, GlobalEscape) => GlobalEscape,
      (ReturnEscape, ReturnEscape) => ReturnEscape,
      (ReturnEscape, NoEscape) | (NoEscape, ReturnEscape) => ReturnEscape,
      (ReturnEscape, ArgEscape(_)) | (ArgEscape(_), ReturnEscape) => Unknown,
      (ArgEscape(a), ArgEscape(b)) => {
        if a == b {
          ArgEscape(a)
        } else {
          Unknown
        }
      }
      (ArgEscape(_), NoEscape) | (NoEscape, ArgEscape(_)) => {
        if self == NoEscape {
          other
        } else {
          self
        }
      }
      (NoEscape, NoEscape) => NoEscape,
    }
  }

  pub fn escapes(self) -> bool {
    self != EscapeState::NoEscape
  }
}

/// Escape results keyed by SSA/temp variable ID.
///
/// For this initial pass we primarily populate entries for allocation-defining temps and any temps
/// that may alias an escaping allocation.
pub type EscapeResult = BTreeMap<u32, EscapeState>;

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.sort_unstable();
  labels.dedup();
  labels
}

fn is_internal_alloc_builder(callee: &Arg) -> bool {
  let Arg::Builtin(name) = callee else {
    return false;
  };
  matches!(
    name.as_str(),
    "__optimize_js_array" | "__optimize_js_object" | "__optimize_js_regex"
  )
}

fn collect_param_vars(cfg: &Cfg) -> BTreeSet<u32> {
  let mut defs = BTreeSet::<u32>::new();
  let mut uses = BTreeSet::<u32>::new();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      defs.extend(inst.tgts.iter().copied());
      for arg in inst.args.iter() {
        if let Arg::Var(v) = arg {
          uses.insert(*v);
        }
      }
    }
  }

  uses.into_iter().filter(|v| !defs.contains(v)).collect()
}

fn collect_alloc_sites(cfg: &Cfg) -> (BTreeSet<u32>, BTreeMap<AbstractLoc, u32>) {
  let mut alloc_vars = BTreeSet::<u32>::new();
  let mut loc_to_var = BTreeMap::<AbstractLoc, u32>::new();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for (idx, inst) in block.iter().enumerate() {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (tgt, callee, _this, _args, _spreads) = inst.as_call();
      let Some(tgt) = tgt else {
        continue;
      };
      if !is_internal_alloc_builder(callee) {
        continue;
      }
      alloc_vars.insert(tgt);
      loc_to_var.insert(
        AbstractLoc::Alloc {
          block: label,
          inst_idx: idx as u32,
        },
        tgt,
      );
    }
  }

  (alloc_vars, loc_to_var)
}

fn allocs_for_arg(
  aliases: &alias::AliasResult,
  loc_to_var: &BTreeMap<AbstractLoc, u32>,
  arg: &Arg,
) -> BTreeSet<u32> {
  let Arg::Var(v) = arg else {
    return BTreeSet::new();
  };
  let Some(pts) = aliases.points_to.get(v) else {
    // We treat missing points-to info as "not one of our local allocations" rather than `Top`.
    return BTreeSet::new();
  };
  let mut out = BTreeSet::new();
  for loc in pts.iter() {
    if let Some(var) = loc_to_var.get(loc) {
      out.insert(*var);
    }
  }
  out
}

fn join_escape(states: &mut BTreeMap<u32, EscapeState>, alloc: u32, esc: EscapeState) {
  let entry = states.entry(alloc).or_insert(EscapeState::NoEscape);
  let next = entry.join(esc);
  if next != *entry {
    *entry = next;
  }
}

pub fn analyze_cfg_escapes(cfg: &Cfg) -> EscapeResult {
  let aliases = alias::calculate_alias(cfg);
  let param_vars = collect_param_vars(cfg);
  let (alloc_vars, loc_to_var) = collect_alloc_sites(cfg);

  let mut alloc_states: BTreeMap<u32, EscapeState> =
    alloc_vars.iter().copied().map(|v| (v, EscapeState::NoEscape)).collect();
  let mut stored_into: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::ForeignStore | InstTyp::UnknownStore => {
          for alloc in allocs_for_arg(&aliases, &loc_to_var, &inst.args[0]) {
            join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
          }
        }
        InstTyp::Call => {
          let (_tgt, callee, _this, args, _spreads) = inst.as_call();
          if is_internal_alloc_builder(callee) {
            // Treat allocation builtins as container initialization.
            if let Some(&container) = inst.tgts.get(0) {
              for arg in args.iter() {
                for value_alloc in allocs_for_arg(&aliases, &loc_to_var, arg) {
                  stored_into.entry(container).or_default().insert(value_alloc);
                }
              }
            }
          } else {
            // Unknown/impure call: conservatively treat any allocation passed as escaping.
            for arg in inst.args.iter() {
              for alloc in allocs_for_arg(&aliases, &loc_to_var, arg) {
                join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
              }
            }
          }
        }
        InstTyp::PropAssign => {
          let (obj, _prop, val) = inst.as_prop_assign();
          let value_allocs = allocs_for_arg(&aliases, &loc_to_var, val);
          if value_allocs.is_empty() {
            continue;
          }

          if let Arg::Var(obj_var) = obj {
            if param_vars.contains(obj_var) {
              for value in value_allocs {
                join_escape(&mut alloc_states, value, EscapeState::ArgEscape(*obj_var));
              }
              continue;
            }
          }

          let container_allocs = allocs_for_arg(&aliases, &loc_to_var, obj);
          if !container_allocs.is_empty() {
            for container in container_allocs {
              stored_into
                .entry(container)
                .or_default()
                .extend(value_allocs.iter().copied());
            }
          } else {
            // Receiver is not a local allocation we can reason about; conservatively treat as
            // escape to global/unknown memory.
            for value in value_allocs {
              join_escape(&mut alloc_states, value, EscapeState::GlobalEscape);
            }
          }
        }
        _ => {}
      }
    }
  }

  // Propagate escapes through container edges: if a container allocation escapes, any allocations
  // stored into it are also reachable outside the function.
  let mut changed = true;
  while changed {
    changed = false;
    for (container, values) in stored_into.iter() {
      let container_state = alloc_states.get(container).copied().unwrap_or(EscapeState::NoEscape);
      if container_state == EscapeState::NoEscape {
        continue;
      }
      for &value in values {
        let entry = alloc_states.entry(value).or_insert(EscapeState::NoEscape);
        let next = entry.join(container_state);
        if next != *entry {
          *entry = next;
          changed = true;
        }
      }
    }
  }

  // Materialize per-variable escape information for any temp that may alias an escaping allocation.
  let mut out: EscapeResult = EscapeResult::new();

  for &alloc in alloc_vars.iter() {
    out.insert(
      alloc,
      alloc_states.get(&alloc).copied().unwrap_or(EscapeState::NoEscape),
    );
  }

  for (var, pts) in aliases.points_to_sorted() {
    let mut state = EscapeState::NoEscape;
    for loc in pts.iter() {
      let Some(alloc) = loc_to_var.get(loc) else {
        continue;
      };
      let alloc_state = alloc_states
        .get(alloc)
        .copied()
        .unwrap_or(EscapeState::NoEscape);
      state = state.join(alloc_state);
      if state == EscapeState::Unknown {
        break;
      }
    }
    if state != EscapeState::NoEscape {
      out.insert(var, state);
    }
  }

  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
  use crate::il::inst::{Const, Inst};
  use crate::symbol::semantics::SymbolId;
  use parse_js::num::JsNumber;

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

  fn escape_of(result: &EscapeResult, var: u32) -> EscapeState {
    result.get(&var).copied().unwrap_or(EscapeState::NoEscape)
  }

  #[test]
  fn local_allocation_no_escape() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(0),
            Arg::Const(Const::Str("x".to_string())),
            Arg::Const(Const::Num(JsNumber(1.0))),
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::NoEscape);
  }

  #[test]
  fn allocation_passed_to_unknown_call_escapes() {
    let cfg = cfg_with_blocks(
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
          Inst::unknown_load(1, "f".to_string()),
          Inst::call(
            2,
            Arg::Var(1),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(0)],
            vec![],
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::GlobalEscape);
  }

  #[test]
  fn allocation_stored_to_foreign_store_escapes() {
    let cfg = cfg_with_blocks(
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
          Inst::var_assign(1, Arg::Var(0)),
          Inst::foreign_store(SymbolId(123), Arg::Var(1)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::GlobalEscape);
  }

  #[test]
  fn allocation_stored_into_parameter_object_is_arg_escape() {
    // %99 has no definition, so the analysis treats it as a parameter/input.
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(99),
            Arg::Const(Const::Str("x".to_string())),
            Arg::Var(0),
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::ArgEscape(99));
  }

  #[test]
  fn stored_value_escapes_with_container() {
    let cfg = cfg_with_blocks(
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
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(0),
            Arg::Const(Const::Str("x".to_string())),
            Arg::Var(1),
          ),
          Inst::foreign_store(SymbolId(1), Arg::Var(0)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::GlobalEscape);
    assert_eq!(escape_of(&escape, 1), EscapeState::GlobalEscape);
  }

  #[test]
  fn builtin_initializers_propagate_escape() {
    let cfg = cfg_with_blocks(
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
          Inst::foreign_store(SymbolId(2), Arg::Var(1)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 1), EscapeState::GlobalEscape);
    assert_eq!(escape_of(&escape, 0), EscapeState::GlobalEscape);
  }
}

