use crate::analysis::alias::{self, AbstractLoc};
use crate::analysis::escape::{self, EscapeResult, EscapeState};
use crate::analysis::ownership::{self, OwnershipResult};
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, FieldRef, Inst, InstTyp, OwnershipState, Purity};
use crate::opt::PassResult;
use crate::FnId;
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FieldKey {
  alloc: AbstractLoc,
  prop: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FieldState {
  /// Allocations that may be reachable from unknown code.
  ///
  /// When an allocation is in this set, any unknown call may mutate its fields even if the
  /// allocation is not passed directly as an argument (e.g. via global/foreign storage).
  escaped: BTreeSet<AbstractLoc>,
  /// Known `alloc[prop] = FnId` facts at the current program point.
  ///
  /// We only track fields that have exactly one write (the initial definition). If the same
  /// field is written again, we conservatively stop tracking it to avoid devirtualizing through
  /// overwritten properties.
  fields: BTreeMap<FieldKey, FnId>,
  /// Fields that are known to have been overwritten (multiple writes or unknown write value).
  overwritten: BTreeSet<FieldKey>,
}

impl FieldState {
  fn new() -> Self {
    Self::default()
  }
}

#[derive(Clone, Debug)]
enum VarFnDef {
  Alias(u32),
  Fn(FnId),
  Phi(Vec<Arg>),
  Unknown,
}

fn escape_of(result: &EscapeResult, var: u32) -> EscapeState {
  result.get(&var).copied().unwrap_or(EscapeState::NoEscape)
}

fn ownership_of(result: &OwnershipResult, var: u32) -> OwnershipState {
  result.get(&var).copied().unwrap_or(OwnershipState::Unknown)
}

fn prop_key(arg: &Arg) -> Option<String> {
  match arg {
    Arg::Const(Const::Str(s)) => Some(s.clone()),
    _ => None,
  }
}

fn field_ref_key(field: &FieldRef) -> Option<String> {
  match field {
    FieldRef::Prop(s) => Some(s.clone()),
    _ => None,
  }
}

fn single_alloc_for_var(alias: &alias::AliasResult, var: u32) -> Option<AbstractLoc> {
  let pts = alias.points_to.get(&var)?;
  if pts.is_top() || pts.len() != 1 {
    return None;
  }
  pts.iter().next().cloned()
}

fn var_is_safe_object(
  alias: &alias::AliasResult,
  escapes: &EscapeResult,
  ownership: &OwnershipResult,
  var: u32,
) -> Option<AbstractLoc> {
  let alloc = single_alloc_for_var(alias, var)?;
  if !matches!(alloc, AbstractLoc::Alloc { .. }) {
    return None;
  }
  // Objects reachable from a parameter are not locally trackable.
  if matches!(escape_of(escapes, var), EscapeState::ArgEscape(_)) {
    return None;
  }
  // For this pass we only need the allocation to be local and not "fully unknown" in the escape
  // lattice; in particular, we do not require `NoEscape` because passing a fresh object as `this`
  // for a method call still allows us to devirtualize the callee loaded from its fields.
  if matches!(escape_of(escapes, var), EscapeState::Unknown) {
    return None;
  }
  // Ownership is used as an additional "is this value local/trackable" signal. We accept `Shared`
  // allocations here because field tracking is per-allocation and remains sound as long as the
  // allocation does not escape.
  if matches!(ownership_of(ownership, var), OwnershipState::Unknown) {
    return None;
  }
  Some(alloc)
}

fn clear_fields_for_alloc(fields: &mut BTreeMap<FieldKey, FnId>, alloc: &AbstractLoc) {
  fields.retain(|k, _| &k.alloc != alloc);
}

fn clear_overwritten_for_alloc(overwritten: &mut BTreeSet<FieldKey>, alloc: &AbstractLoc) {
  overwritten.retain(|k| &k.alloc != alloc);
}

fn escape_alloc(state: &mut FieldState, alloc: AbstractLoc) {
  state.escaped.insert(alloc.clone());
  clear_fields_for_alloc(&mut state.fields, &alloc);
  clear_overwritten_for_alloc(&mut state.overwritten, &alloc);
}

fn escape_var(
  state: &mut FieldState,
  var: u32,
  alias: &alias::AliasResult,
  defs: &HashMap<u32, VarFnDef>,
  getprop_consts: &HashMap<u32, FnId>,
) {
  // Ignore known function values; functions are immutable and are not the heap objects whose fields
  // we track here.
  let mut memo = HashMap::<u32, Option<FnId>>::new();
  if resolve_var_fn_id(var, defs, getprop_consts, &mut memo, &mut Vec::new()).is_some() {
    return;
  }

  let Some(pts) = alias.points_to.get(&var) else {
    // Unknown points-to => Top.
    if !state.fields.is_empty() {
      // Conservatively treat all tracked allocations as escaped.
      let allocs: Vec<_> = state.fields.keys().map(|k| k.alloc.clone()).collect();
      for alloc in allocs {
        if matches!(alloc, AbstractLoc::Alloc { .. }) {
          state.escaped.insert(alloc);
        }
      }
      state.fields.clear();
    }
    return;
  };
  if pts.is_top() {
    if !state.fields.is_empty() {
      let allocs: Vec<_> = state.fields.keys().map(|k| k.alloc.clone()).collect();
      for alloc in allocs {
        if matches!(alloc, AbstractLoc::Alloc { .. }) {
          state.escaped.insert(alloc);
        }
      }
      state.fields.clear();
    }
    return;
  }
  for loc in pts.iter() {
    if matches!(loc, AbstractLoc::Alloc { .. }) {
      escape_alloc(state, loc.clone());
    }
  }
}

fn call_is_pure_alloc_marker(callee: &Arg) -> bool {
  matches!(
    callee,
    Arg::Builtin(path)
      if matches!(
        path.as_str(),
        "__optimize_js_object" | "__optimize_js_array" | "__optimize_js_regex" | "__optimize_js_template"
      )
  )
}

fn parse_object_literal_fields(args: &[Arg]) -> Option<BTreeMap<String, Arg>> {
  // `__optimize_js_object` encodes each property as a triple:
  //   marker, key, value
  // where marker is one of:
  //   - __optimize_js_object_prop
  //   - __optimize_js_object_prop_computed
  //   - __optimize_js_object_spread
  //
  // For devirtualization we only accept simple constant-key properties with the `*_prop` marker,
  // and we reject computed keys/spreads/duplicates to stay conservative.
  let mut fields = BTreeMap::<String, Arg>::new();
  for chunk in args.chunks(3) {
    if chunk.len() != 3 {
      return None;
    }
    let (marker, key, value) = (&chunk[0], &chunk[1], &chunk[2]);
    let Arg::Builtin(marker) = marker else {
      return None;
    };
    if marker != "__optimize_js_object_prop" {
      return None;
    }
    let Arg::Const(Const::Str(key)) = key else {
      return None;
    };
    // `__proto__` has special semantics in JS object literals; stay conservative.
    if key == "__proto__" {
      return None;
    }
    // Duplicate keys introduce ordering/overwrite semantics; stay conservative.
    if fields.contains_key(key) {
      return None;
    }
    fields.insert(key.clone(), value.clone());
  }
  Some(fields)
}

fn apply_object_literal_alloc(
  state: &mut FieldState,
  tgt: Option<u32>,
  callee: &Arg,
  args: &[Arg],
  spreads: &[usize],
  alias: &alias::AliasResult,
  escapes: &EscapeResult,
  ownership: &OwnershipResult,
  defs: &HashMap<u32, VarFnDef>,
  getprop_consts: &HashMap<u32, FnId>,
) {
  let Arg::Builtin(name) = callee else {
    return;
  };
  if name != "__optimize_js_object" {
    return;
  }
  let Some(tgt) = tgt else {
    return;
  };
  if !spreads.is_empty() {
    return;
  }
  let Some(alloc) = var_is_safe_object(alias, escapes, ownership, tgt) else {
    return;
  };
  let Some(fields) = parse_object_literal_fields(args) else {
    return;
  };

  for (prop, value) in fields {
    let key = FieldKey {
      alloc: alloc.clone(),
      prop,
    };
    if state.overwritten.contains(&key) || state.fields.contains_key(&key) {
      state.fields.remove(&key);
      state.overwritten.insert(key);
      continue;
    }
    let mut memo = HashMap::<u32, Option<FnId>>::new();
    if let Some(fn_id) = resolve_fn_id(&value, defs, getprop_consts, &mut memo, &mut Vec::new()) {
      state.fields.insert(key, fn_id);
    } else {
      // Unknown initializer value -> do not attempt to track it later.
      state.overwritten.insert(key);
    }
  }
}

fn build_var_fn_defs(cfg: &Cfg) -> HashMap<u32, VarFnDef> {
  let mut defs = HashMap::<u32, VarFnDef>::new();
  for label in cfg.reverse_postorder() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };
      let def = match inst.t {
        InstTyp::VarAssign => match inst.args.get(0) {
          Some(Arg::Var(src)) => VarFnDef::Alias(*src),
          Some(Arg::Fn(id)) => VarFnDef::Fn(*id),
          _ => VarFnDef::Unknown,
        },
        InstTyp::Phi => VarFnDef::Phi(inst.args.clone()),
        _ => VarFnDef::Unknown,
      };
      defs
        .entry(tgt)
        .and_modify(|existing| {
          if !matches!(*existing, VarFnDef::Unknown) {
            *existing = VarFnDef::Unknown;
          }
        })
        .or_insert(def);
    }
  }
  defs
}

fn resolve_fn_id(
  arg: &Arg,
  defs: &HashMap<u32, VarFnDef>,
  getprop_consts: &HashMap<u32, FnId>,
  memo: &mut HashMap<u32, Option<FnId>>,
  visiting: &mut Vec<u32>,
) -> Option<FnId> {
  match arg {
    Arg::Fn(id) => Some(*id),
    Arg::Var(v) => resolve_var_fn_id(*v, defs, getprop_consts, memo, visiting),
    _ => None,
  }
}

fn resolve_var_fn_id(
  var: u32,
  defs: &HashMap<u32, VarFnDef>,
  getprop_consts: &HashMap<u32, FnId>,
  memo: &mut HashMap<u32, Option<FnId>>,
  visiting: &mut Vec<u32>,
) -> Option<FnId> {
  if let Some(id) = getprop_consts.get(&var).copied() {
    return Some(id);
  }
  if let Some(cached) = memo.get(&var).copied() {
    return cached;
  }
  if visiting.contains(&var) {
    return None;
  }
  visiting.push(var);

  let out = match defs.get(&var) {
    Some(VarFnDef::Fn(id)) => Some(*id),
    Some(VarFnDef::Alias(src)) => resolve_var_fn_id(*src, defs, getprop_consts, memo, visiting),
    Some(VarFnDef::Phi(args)) => {
      let mut merged: Option<FnId> = None;
      for arg in args {
        let Some(id) = resolve_fn_id(arg, defs, getprop_consts, memo, visiting) else {
          visiting.pop();
          memo.insert(var, None);
          return None;
        };
        merged = match merged {
          None => Some(id),
          Some(prev) if prev == id => Some(prev),
          _ => {
            visiting.pop();
            memo.insert(var, None);
            return None;
          }
        };
      }
      merged
    }
    _ => None,
  };

  visiting.pop();
  memo.insert(var, out);
  out
}

fn join_field_states(cfg: &Cfg, label: u32, out_states: &BTreeMap<u32, FieldState>) -> FieldState {
  if label == cfg.entry {
    return FieldState::new();
  }
  let preds = cfg.graph.parents_sorted(label);
  let mut iter = preds.into_iter().filter_map(|pred| out_states.get(&pred));
  let Some(first) = iter.next() else {
    return FieldState::new();
  };
  let mut joined = first.clone();
  for pred in iter {
    joined
      .fields
      .retain(|k, v| pred.fields.get(k) == Some(v));
    joined.escaped.extend(pred.escaped.iter().cloned());
    joined
      .overwritten
      .extend(pred.overwritten.iter().cloned());
  }
  if !joined.overwritten.is_empty() {
    joined.fields.retain(|k, _| !joined.overwritten.contains(k));
  }
  joined
}

fn apply_prop_assign(
  state: &mut FieldState,
  obj: &Arg,
  prop: &Arg,
  val: &Arg,
  alias: &alias::AliasResult,
  escapes: &EscapeResult,
  ownership: &OwnershipResult,
  defs: &HashMap<u32, VarFnDef>,
  getprop_consts: &HashMap<u32, FnId>,
) {
  let Arg::Var(obj_var) = obj else {
    return;
  };
  let Some(alloc) = var_is_safe_object(alias, escapes, ownership, *obj_var) else {
    return;
  };

  let Some(prop) = prop_key(prop) else {
    clear_fields_for_alloc(&mut state.fields, &alloc);
    clear_overwritten_for_alloc(&mut state.overwritten, &alloc);
    return;
  };

  let key = FieldKey { alloc, prop };
  if state.overwritten.contains(&key) {
    state.fields.remove(&key);
    return;
  }
  // Conservatively disable tracking if the property is written more than once.
  if state.fields.contains_key(&key) {
    state.fields.remove(&key);
    state.overwritten.insert(key);
    return;
  }
  let mut memo = HashMap::<u32, Option<FnId>>::new();
  let fn_id = resolve_fn_id(val, defs, getprop_consts, &mut memo, &mut Vec::new());
  match fn_id {
    Some(id) => {
      state.fields.insert(key, id);
    }
    None => {
      // First write is not a known function: don't track subsequent writes to this field.
      state.overwritten.insert(key);
    }
  }
}

fn apply_field_store(
  state: &mut FieldState,
  obj: &Arg,
  field: &FieldRef,
  val: &Arg,
  alias: &alias::AliasResult,
  escapes: &EscapeResult,
  ownership: &OwnershipResult,
  defs: &HashMap<u32, VarFnDef>,
  getprop_consts: &HashMap<u32, FnId>,
) {
  let Arg::Var(obj_var) = obj else {
    return;
  };
  let Some(alloc) = var_is_safe_object(alias, escapes, ownership, *obj_var) else {
    return;
  };
  let Some(prop) = field_ref_key(field) else {
    return;
  };
  if prop == "__proto__" {
    clear_fields_for_alloc(&mut state.fields, &alloc);
    clear_overwritten_for_alloc(&mut state.overwritten, &alloc);
    return;
  }

  let key = FieldKey { alloc, prop };
  if state.overwritten.contains(&key) {
    state.fields.remove(&key);
    return;
  }
  if state.fields.contains_key(&key) {
    state.fields.remove(&key);
    state.overwritten.insert(key);
    return;
  }
  let mut memo = HashMap::<u32, Option<FnId>>::new();
  let fn_id = resolve_fn_id(val, defs, getprop_consts, &mut memo, &mut Vec::new());
  match fn_id {
    Some(id) => {
      state.fields.insert(key, id);
    }
    None => {
      state.overwritten.insert(key);
    }
  }
}

fn transfer_block(
  block: &[Inst],
  in_state: &FieldState,
  alias: &alias::AliasResult,
  escapes: &EscapeResult,
  ownership: &OwnershipResult,
  defs: &HashMap<u32, VarFnDef>,
  getprop_consts: &HashMap<u32, FnId>,
) -> FieldState {
  let mut state = in_state.clone();
  for inst in block {
    match inst.t {
      InstTyp::Call => {
        let (tgt, callee, this, args, spreads) = inst.as_call();
        if call_is_pure_alloc_marker(callee) {
          apply_object_literal_alloc(
            &mut state,
            tgt,
            callee,
            args,
            spreads,
            alias,
            escapes,
            ownership,
            defs,
            getprop_consts,
          );
          continue;
        }

        // Any unknown call may mutate allocations that have already escaped.
        let escaped = &state.escaped;
        if !escaped.is_empty() {
          state.fields.retain(|k, _| !escaped.contains(&k.alloc));
        }

        // Values passed into a call may escape from the current scope; once escaped, subsequent
        // calls may mutate them even if they are not passed again.
        if let Arg::Var(v) = this {
          escape_var(&mut state, *v, alias, defs, getprop_consts);
        }
        for arg in args {
          if let Arg::Var(v) = arg {
            escape_var(&mut state, *v, alias, defs, getprop_consts);
          }
        }
      }
      InstTyp::PropAssign => {
        let (obj, prop, val) = inst.as_prop_assign();
        apply_prop_assign(
          &mut state,
          obj,
          prop,
          val,
          alias,
          escapes,
          ownership,
          defs,
          getprop_consts,
        );

        // Storing a tracked allocation into an unknown receiver makes it reachable from unknown
        // code (e.g. `global.x = obj`), so conservatively treat it as escaped.
        let receiver_safe = match obj {
          Arg::Var(v) => var_is_safe_object(alias, escapes, ownership, *v).is_some(),
          _ => false,
        };
        if !receiver_safe {
          if let Arg::Var(v) = val {
            escape_var(&mut state, *v, alias, defs, getprop_consts);
          }
        }
      }
      InstTyp::FieldStore => {
        let (obj, field, val) = inst.as_field_store();
        apply_field_store(
          &mut state,
          obj,
          field,
          val,
          alias,
          escapes,
          ownership,
          defs,
          getprop_consts,
        );

        let receiver_safe = match obj {
          Arg::Var(v) => var_is_safe_object(alias, escapes, ownership, *v).is_some(),
          _ => false,
        };
        if !receiver_safe {
          if let Arg::Var(v) = val {
            escape_var(&mut state, *v, alias, defs, getprop_consts);
          }
        }
      }
      InstTyp::ForeignStore | InstTyp::UnknownStore => {
        if let Some(Arg::Var(v)) = inst.args.get(0) {
          escape_var(&mut state, *v, alias, defs, getprop_consts);
        }
      }
      InstTyp::Return => {
        if let Some(Arg::Var(v)) = inst.args.get(0) {
          escape_var(&mut state, *v, alias, defs, getprop_consts);
        }
      }
      InstTyp::Throw => {
        if let Some(Arg::Var(v)) = inst.args.get(0) {
          escape_var(&mut state, *v, alias, defs, getprop_consts);
        }
      }
      _ => {}
    }
  }
  state
}

fn compute_field_states(
  cfg: &Cfg,
  alias: &alias::AliasResult,
  escapes: &EscapeResult,
  ownership: &OwnershipResult,
  defs: &HashMap<u32, VarFnDef>,
) -> BTreeMap<u32, FieldState> {
  let labels = cfg.reverse_postorder();
  let mut in_states = BTreeMap::<u32, FieldState>::new();
  let mut out_states = BTreeMap::<u32, FieldState>::new();
  for &label in &labels {
    in_states.insert(label, FieldState::new());
    out_states.insert(label, FieldState::new());
  }

  let getprop_consts = HashMap::<u32, FnId>::new();
  let mut worklist: VecDeque<u32> = labels.iter().copied().collect();
  while let Some(label) = worklist.pop_front() {
    let in_state = join_field_states(cfg, label, &out_states);
    let stored_in = in_states.get(&label).cloned().unwrap_or_default();
    if stored_in != in_state {
      in_states.insert(label, in_state.clone());
    }

    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    let out_state = transfer_block(
      block,
      &in_state,
      alias,
      escapes,
      ownership,
      defs,
      &getprop_consts,
    );

    let stored_out = out_states.get(&label).cloned().unwrap_or_default();
    if stored_out != out_state {
      out_states.insert(label, out_state);
      for succ in cfg.graph.children_sorted(label) {
        if in_states.contains_key(&succ) {
          worklist.push_back(succ);
        }
      }
    }
  }

  in_states
}

fn compute_getprop_consts(
  cfg: &Cfg,
  in_states: &BTreeMap<u32, FieldState>,
  alias: &alias::AliasResult,
  escapes: &EscapeResult,
  ownership: &OwnershipResult,
  defs: &HashMap<u32, VarFnDef>,
) -> HashMap<u32, FnId> {
  let mut out = HashMap::<u32, FnId>::new();
  // No need for a fixed point here: `in_states` already represents the converged field map at
  // each block entry, and `GetProp` results are SSA values.
  for label in cfg.reverse_postorder() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    let mut state = in_states.get(&label).cloned().unwrap_or_default();
    for inst in block.iter() {
      if inst.t == InstTyp::Bin && inst.bin_op == BinOp::GetProp {
        let (tgt, obj, _, prop) = inst.as_bin();
        if let (Arg::Var(obj_var), Some(prop)) = (obj, prop_key(prop)) {
          if let Some(alloc) = var_is_safe_object(alias, escapes, ownership, *obj_var) {
            let key = FieldKey { alloc, prop };
            if let Some(fn_id) = state.fields.get(&key).copied() {
              out.insert(tgt, fn_id);
            }
          }
        }
      }
      if inst.t == InstTyp::FieldLoad {
        let (tgt, obj, field) = inst.as_field_load();
        if let (Arg::Var(obj_var), Some(prop)) = (obj, field_ref_key(field)) {
          if let Some(alloc) = var_is_safe_object(alias, escapes, ownership, *obj_var) {
            let key = FieldKey { alloc, prop };
            if let Some(fn_id) = state.fields.get(&key).copied() {
              out.insert(tgt, fn_id);
            }
          }
        }
      }

      match inst.t {
        InstTyp::Call => {
          let (tgt, callee, this, args, spreads) = inst.as_call();
          if call_is_pure_alloc_marker(callee) {
            apply_object_literal_alloc(
              &mut state,
              tgt,
              callee,
              args,
              spreads,
              alias,
              escapes,
              ownership,
              defs,
              &out,
            );
            continue;
          }

          let escaped = &state.escaped;
          if !escaped.is_empty() {
            state.fields.retain(|k, _| !escaped.contains(&k.alloc));
          }

          if let Arg::Var(v) = this {
            escape_var(&mut state, *v, alias, defs, &out);
          }
          for arg in args {
            if let Arg::Var(v) = arg {
              escape_var(&mut state, *v, alias, defs, &out);
            }
          }
        }
        InstTyp::PropAssign => {
          let (obj, prop, val) = inst.as_prop_assign();
          apply_prop_assign(
            &mut state,
            obj,
            prop,
            val,
            alias,
            escapes,
            ownership,
            defs,
            &out,
          );

          let receiver_safe = match obj {
            Arg::Var(v) => var_is_safe_object(alias, escapes, ownership, *v).is_some(),
            _ => false,
          };
          if !receiver_safe {
            if let Arg::Var(v) = val {
              escape_var(&mut state, *v, alias, defs, &out);
            }
          }
        }
        InstTyp::FieldStore => {
          let (obj, field, val) = inst.as_field_store();
          apply_field_store(
            &mut state,
            obj,
            field,
            val,
            alias,
            escapes,
            ownership,
            defs,
            &out,
          );

          let receiver_safe = match obj {
            Arg::Var(v) => var_is_safe_object(alias, escapes, ownership, *v).is_some(),
            _ => false,
          };
          if !receiver_safe {
            if let Arg::Var(v) = val {
              escape_var(&mut state, *v, alias, defs, &out);
            }
          }
        }
        InstTyp::ForeignStore | InstTyp::UnknownStore => {
          if let Some(Arg::Var(v)) = inst.args.get(0) {
            escape_var(&mut state, *v, alias, defs, &out);
          }
        }
        InstTyp::Return | InstTyp::Throw => {
          if let Some(Arg::Var(v)) = inst.args.get(0) {
            escape_var(&mut state, *v, alias, defs, &out);
          }
        }
        _ => {}
      }
    }
  }
  out
}

pub fn optpass_devirtualize(cfg: &mut Cfg) -> PassResult {
  let defs = build_var_fn_defs(cfg);

  let mut result = PassResult::default();
  let empty_getprop_consts = HashMap::<u32, FnId>::new();
  let mut memo = HashMap::<u32, Option<FnId>>::new();
  for label in cfg.reverse_postorder() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    // We need a separate mutable borrow to rewrite, but also want to keep traversal deterministic.
    let block_len = block.len();
    let block_mut = cfg.bblocks.get_mut(label);
    for inst_idx in 0..block_len {
      let inst = &mut block_mut[inst_idx];
      if inst.t != InstTyp::Call {
        continue;
      }
      let Some(callee) = inst.args.get(0).cloned() else {
        continue;
      };
      if !matches!(callee, Arg::Var(_)) {
        continue;
      }

      let Some(fn_id) = resolve_fn_id(
        &callee,
        &defs,
        &empty_getprop_consts,
        &mut memo,
        &mut Vec::new(),
      ) else {
        continue;
      };
      let new_callee = Arg::Fn(fn_id);
      if inst.args[0] != new_callee {
        inst.args[0] = new_callee;
        // Purity/effect metadata may have been computed using the previous call target. Reset to
        // conservative defaults so later passes do not observe stale information.
        inst.meta.callee_purity = Purity::Impure;
        inst.meta.effects.mark_unknown();
        result.mark_changed();
      }
    }
  }

  // Fast bailout: field tracking is only useful if we have both:
  //   1. An indirect call whose callee originates from `GetProp` (method call lowering).
  //   2. A field write that stores a known `Arg::Fn` value (either via `PropAssign` or an
  //      `__optimize_js_object` literal initializer).
  //
  // This avoids running heavier alias/escape/ownership analyses for programs that only have
  // ordinary indirect calls (handled above) or no relevant stores.
  let mut getprop_defs = HashSet::<u32>::new();
  let mut has_getprop_call = false;
  let mut has_fn_field_write = false;
  for label in cfg.reverse_postorder() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      if inst.t == InstTyp::Bin && inst.bin_op == BinOp::GetProp {
        getprop_defs.insert(inst.tgts[0]);
      }
      if inst.t == InstTyp::FieldLoad {
        getprop_defs.insert(inst.tgts[0]);
      }
    }
  }
  for label in cfg.reverse_postorder() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::Call => {
          let (_tgt, callee, _this, args, spreads) = inst.as_call();
          if matches!(callee, Arg::Builtin(name) if name == "__optimize_js_object") && spreads.is_empty() {
            if let Some(fields) = parse_object_literal_fields(args) {
              for value in fields.values() {
                let mut local_memo = HashMap::<u32, Option<FnId>>::new();
                if resolve_fn_id(value, &defs, &empty_getprop_consts, &mut local_memo, &mut Vec::new())
                  .is_some()
                {
                  has_fn_field_write = true;
                  break;
                }
              }
            }
          }
          if let Some(Arg::Var(v)) = inst.args.get(0) {
            if getprop_defs.contains(v) {
              has_getprop_call = true;
            }
          }
        }
        InstTyp::PropAssign => {
          let (_obj, prop, val) = inst.as_prop_assign();
          if prop_key(prop).is_none() {
            continue;
          }
          let mut local_memo = HashMap::<u32, Option<FnId>>::new();
          if resolve_fn_id(val, &defs, &empty_getprop_consts, &mut local_memo, &mut Vec::new())
            .is_some()
          {
            has_fn_field_write = true;
          }
        }
        InstTyp::FieldStore => {
          let (_obj, field, val) = inst.as_field_store();
          let Some(_prop) = field_ref_key(field) else {
            continue;
          };
          let mut local_memo = HashMap::<u32, Option<FnId>>::new();
          if resolve_fn_id(val, &defs, &empty_getprop_consts, &mut local_memo, &mut Vec::new())
            .is_some()
          {
            has_fn_field_write = true;
          }
        }
        _ => {}
      }
    }
  }
  if !has_getprop_call || !has_fn_field_write {
    return result;
  }

  let alias = alias::calculate_alias(cfg);
  let escapes = escape::analyze_cfg_escapes(cfg);
  let ownership = ownership::analyze_cfg_ownership_with_escapes(cfg, &escapes);

  let in_states = compute_field_states(cfg, &alias, &escapes, &ownership, &defs);
  let getprop_consts = compute_getprop_consts(cfg, &in_states, &alias, &escapes, &ownership, &defs);
  if getprop_consts.is_empty() {
    return result;
  }

  memo.clear();
  for label in cfg.reverse_postorder() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    let block_len = block.len();
    let block_mut = cfg.bblocks.get_mut(label);
    for inst_idx in 0..block_len {
      let inst = &mut block_mut[inst_idx];
      if inst.t != InstTyp::Call {
        continue;
      }
      let Some(callee) = inst.args.get(0).cloned() else {
        continue;
      };
      if !matches!(callee, Arg::Var(_)) {
        continue;
      }

      let Some(fn_id) = resolve_fn_id(&callee, &defs, &getprop_consts, &mut memo, &mut Vec::new())
      else {
        continue;
      };
      let new_callee = Arg::Fn(fn_id);
      if inst.args[0] != new_callee {
        inst.args[0] = new_callee;
        inst.meta.callee_purity = Purity::Impure;
        inst.meta.effects.mark_unknown();
        result.mark_changed();
      }
    }
  }

  result
}
