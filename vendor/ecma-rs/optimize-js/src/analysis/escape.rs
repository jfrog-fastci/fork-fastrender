use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, InstTyp};
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
  /// This includes values returned via [`InstTyp::Return`] or thrown via [`InstTyp::Throw`]. We do
  /// not model `try`/`catch` explicitly in the CFG, so both are treated as escaping to the caller.
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

#[derive(Default)]
struct LocalAllocFlowFacts {
  /// Allocation-defining SSA temps (allocation id = temp).
  alloc_vars: BTreeSet<u32>,
  /// SSA temps that may refer to non-local objects (e.g. parameters, global loads, unknown call
  /// results, or the result of `GetProp`).
  ///
  /// This is used to conservatively treat `PropAssign` into such values as an escape sink even when
  /// they may also alias a local allocation (e.g. via `Phi`).
  external_defs: BTreeSet<u32>,
  /// `tgt = src`
  var_assigns: Vec<(u32, Arg)>,
  /// `tgt = phi(args...)`
  phis: Vec<(u32, Vec<Arg>)>,
  /// `tgt = __optimize_js_array/object/regex(...args)`
  ///
  /// The args are treated as values stored into the newly allocated container.
  alloc_inits: Vec<(u32, Vec<Arg>)>,
  /// `obj[prop] = val` (prop ignored; field-insensitive)
  prop_assigns: Vec<(Arg, Arg)>,
  /// `tgt = obj[prop]` (prop ignored; field-insensitive)
  getprops: Vec<(u32, Arg)>,
}

fn collect_local_alloc_flow_facts(cfg: &Cfg) -> LocalAllocFlowFacts {
  let mut facts = LocalAllocFlowFacts::default();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::Call => {
          let (tgt, callee, _this, args, _spreads) = inst.as_call();
          let Some(tgt) = tgt else {
            continue;
          };
          if !is_internal_alloc_builder(callee) {
            facts.external_defs.insert(tgt);
            continue;
          }
          facts.alloc_vars.insert(tgt);
          facts.alloc_inits.push((tgt, args.to_vec()));
        }
        InstTyp::VarAssign => {
          let (tgt, arg) = inst.as_var_assign();
          facts.var_assigns.push((tgt, arg.clone()));
        }
        InstTyp::Phi => {
          let tgt = inst.tgts[0];
          facts.phis.push((tgt, inst.args.clone()));
        }
        InstTyp::ForeignLoad => {
          let (tgt, _) = inst.as_foreign_load();
          facts.external_defs.insert(tgt);
        }
        InstTyp::UnknownLoad => {
          let (tgt, _) = inst.as_unknown_load();
          facts.external_defs.insert(tgt);
        }
        InstTyp::PropAssign => {
          let (obj, _prop, val) = inst.as_prop_assign();
          facts.prop_assigns.push((obj.clone(), val.clone()));
        }
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let (tgt, obj, _op, _prop) = inst.as_bin();
          facts.external_defs.insert(tgt);
          facts.getprops.push((tgt, obj.clone()));
        }
        _ => {}
      }
    }
  }

  facts
}

type VarAllocs = BTreeMap<u32, BTreeSet<u32>>;

fn allocs_for_arg(var_allocs: &VarAllocs, arg: &Arg) -> BTreeSet<u32> {
  match arg {
    Arg::Var(v) => var_allocs.get(v).cloned().unwrap_or_default(),
    _ => BTreeSet::new(),
  }
}

fn ext_for_arg(var_ext: &BTreeMap<u32, EscapeState>, arg: &Arg) -> EscapeState {
  match arg {
    Arg::Var(v) => var_ext.get(v).copied().unwrap_or(EscapeState::NoEscape),
    // Builtins and nested functions are not local allocations.
    Arg::Builtin(_) | Arg::Fn(_) => EscapeState::GlobalEscape,
    Arg::Const(_) => EscapeState::NoEscape,
  }
}

fn join_escape(states: &mut BTreeMap<u32, EscapeState>, alloc: u32, esc: EscapeState) {
  let entry = states.entry(alloc).or_insert(EscapeState::NoEscape);
  let next = entry.join(esc);
  if next != *entry {
    *entry = next;
  }
}

pub fn analyze_cfg_escapes(cfg: &Cfg) -> EscapeResult {
  let param_vars = collect_param_vars(cfg);
  let facts = collect_local_alloc_flow_facts(cfg);
  let alloc_vars = facts.alloc_vars.clone();

  // Infer which local allocations may flow through each SSA variable.
  //
  // This is field-insensitive: `GetProp` may read any allocation ever stored into the receiver.
  // This is necessary because alias analysis currently returns `Top` for `GetProp`, which would
  // otherwise let escapes slip through (e.g. store allocation into a local object, read it back,
  // then pass the loaded value to an unknown call).
  let mut var_allocs: VarAllocs = VarAllocs::new();
  for &alloc in alloc_vars.iter() {
    var_allocs.insert(alloc, BTreeSet::from([alloc]));
  }
  let mut var_ext: BTreeMap<u32, EscapeState> = BTreeMap::new();
  for &param in param_vars.iter() {
    var_ext.insert(param, EscapeState::ArgEscape(param));
  }
  for &v in facts.external_defs.iter() {
    join_escape(&mut var_ext, v, EscapeState::GlobalEscape);
  }
  let mut stored_into: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();

  let mut changed = true;
  while changed {
    changed = false;

    for (tgt, arg) in facts.var_assigns.iter() {
      let src_allocs = allocs_for_arg(&var_allocs, arg);
      if src_allocs.is_empty() {
        // Still propagate externalness through copies.
        let src_ext = ext_for_arg(&var_ext, arg);
        if src_ext != EscapeState::NoEscape {
          let entry = var_ext.entry(*tgt).or_insert(EscapeState::NoEscape);
          let next = entry.join(src_ext);
          if next != *entry {
            *entry = next;
            changed = true;
          }
        }
        continue;
      }
      let entry = var_allocs.entry(*tgt).or_default();
      let before = entry.len();
      entry.extend(src_allocs);
      if entry.len() != before {
        changed = true;
      }

      let src_ext = ext_for_arg(&var_ext, arg);
      if src_ext != EscapeState::NoEscape {
        let entry = var_ext.entry(*tgt).or_insert(EscapeState::NoEscape);
        let next = entry.join(src_ext);
        if next != *entry {
          *entry = next;
          changed = true;
        }
      }
    }

    for (tgt, args) in facts.phis.iter() {
      let mut merged = BTreeSet::new();
      let mut merged_ext = EscapeState::NoEscape;
      for arg in args.iter() {
        merged.extend(allocs_for_arg(&var_allocs, arg));
        merged_ext = merged_ext.join(ext_for_arg(&var_ext, arg));
      }
      if !merged.is_empty() {
        let entry = var_allocs.entry(*tgt).or_default();
        let before = entry.len();
        entry.extend(merged);
        if entry.len() != before {
          changed = true;
        }
      }
      if merged_ext != EscapeState::NoEscape {
        let entry = var_ext.entry(*tgt).or_insert(EscapeState::NoEscape);
        let next = entry.join(merged_ext);
        if next != *entry {
          *entry = next;
          changed = true;
        }
      }
    }

    // Container initialization: treat internal literal builder args as stored into the result.
    for (container, args) in facts.alloc_inits.iter() {
      let entry = stored_into.entry(*container).or_default();
      let before = entry.len();
      for arg in args.iter() {
        entry.extend(allocs_for_arg(&var_allocs, arg));
      }
      if entry.len() != before {
        changed = true;
      }
    }

    // Property stores: `obj[prop] = val` stores `val` into any possible container allocation.
    for (obj, val) in facts.prop_assigns.iter() {
      let value_allocs = allocs_for_arg(&var_allocs, val);
      if value_allocs.is_empty() {
        continue;
      }
      let container_allocs = allocs_for_arg(&var_allocs, obj);
      if container_allocs.is_empty() {
        continue;
      }
      for container in container_allocs {
        let entry = stored_into.entry(container).or_default();
        let before = entry.len();
        entry.extend(value_allocs.iter().copied());
        if entry.len() != before {
          changed = true;
        }
      }
    }

    // Property loads: `tgt = obj[prop]` may read any allocation stored into the receiver.
    for (tgt, obj) in facts.getprops.iter() {
      let container_allocs = allocs_for_arg(&var_allocs, obj);
      if container_allocs.is_empty() {
        continue;
      }
      let mut loaded = BTreeSet::new();
      for container in container_allocs {
        if let Some(values) = stored_into.get(&container) {
          loaded.extend(values.iter().copied());
        }
      }
      if loaded.is_empty() {
        continue;
      }
      let entry = var_allocs.entry(*tgt).or_default();
      let before = entry.len();
      entry.extend(loaded);
      if entry.len() != before {
        changed = true;
      }
    }
  }

  let mut alloc_states: BTreeMap<u32, EscapeState> =
    alloc_vars.iter().copied().map(|v| (v, EscapeState::NoEscape)).collect();

  // Find escape sinks and record direct escapes.
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::Return | InstTyp::Throw => {
          let Some(arg) = inst.args.get(0) else {
            continue;
          };
          for alloc in allocs_for_arg(&var_allocs, arg) {
            join_escape(&mut alloc_states, alloc, EscapeState::ReturnEscape);
          }
        }
        InstTyp::ForeignStore | InstTyp::UnknownStore => {
          for alloc in allocs_for_arg(&var_allocs, &inst.args[0]) {
            join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
          }
        }
        InstTyp::Call => {
          let (_tgt, callee, _this, _args, _spreads) = inst.as_call();
          if is_internal_alloc_builder(callee) {
            continue;
          }
          // Unknown/impure call: conservatively treat any allocation passed as escaping.
          for arg in inst.args.iter() {
            for alloc in allocs_for_arg(&var_allocs, arg) {
              join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
            }
          }
        }
        InstTyp::PropAssign => {
          let (obj, _prop, val) = inst.as_prop_assign();
          let value_allocs = allocs_for_arg(&var_allocs, val);
          if value_allocs.is_empty() {
            continue;
          }
          let obj_ext = ext_for_arg(&var_ext, obj);
          if obj_ext != EscapeState::NoEscape {
            for value in value_allocs {
              join_escape(&mut alloc_states, value, obj_ext);
            }
            continue;
          }

          let container_allocs = allocs_for_arg(&var_allocs, obj);
          if container_allocs.is_empty() {
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

  for (&var, allocs) in var_allocs.iter() {
    let mut state = EscapeState::NoEscape;
    for alloc in allocs.iter() {
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
  use crate::il::inst::{BinOp, Const, Inst};
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
  fn allocation_returned_is_return_escape() {
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
          Inst::ret(Arg::Var(0)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::ReturnEscape);
  }

  #[test]
  fn allocation_thrown_is_return_escape() {
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
          Inst::throw(Arg::Var(0)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::ReturnEscape);
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
  fn getprop_loaded_allocation_escapes_when_passed_to_call() {
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
          Inst::bin(
            2,
            Arg::Var(0),
            BinOp::GetProp,
            Arg::Const(Const::Str("x".to_string())),
          ),
          Inst::unknown_load(3, "f".to_string()),
          Inst::call(
            4,
            Arg::Var(3),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(2)],
            vec![],
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::NoEscape);
    assert_eq!(escape_of(&escape, 1), EscapeState::GlobalEscape);
  }

  #[test]
  fn prop_assign_to_phi_of_local_and_param_is_arg_escape() {
    let mut phi = Inst::phi_empty(1);
    phi.insert_phi(0, Arg::Var(0));
    phi.insert_phi(1, Arg::Var(99));

    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          )],
        ),
        (1, vec![]),
        (
          2,
          vec![
            phi,
            Inst::call(
              2,
              Arg::Builtin("__optimize_js_object".to_string()),
              Arg::Const(Const::Undefined),
              vec![],
              vec![],
            ),
            Inst::prop_assign(
              Arg::Var(1),
              Arg::Const(Const::Str("x".to_string())),
              Arg::Var(2),
            ),
          ],
        ),
      ],
      &[(0, 2), (1, 2)],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 2), EscapeState::ArgEscape(99));
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
