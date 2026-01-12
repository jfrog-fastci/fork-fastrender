use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp, Purity};
use crate::opt::PassResult;
use crate::FnId;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
enum VarDef {
  Alias(u32),
  Fn(FnId),
  Phi(Vec<Arg>),
  GetProp { obj: Arg, key: Arg },
  /// Object literal allocation via the `__optimize_js_object` marker.
  ObjectLit { fields: BTreeMap<String, Arg> },
  Unknown,
}

#[derive(Clone, Debug)]
struct ObjectInfo {
  fields: BTreeMap<String, Arg>,
  overwritten_keys: BTreeSet<String>,
  unsafe_: bool,
}

fn is_object_literal_alloc(inst: &Inst) -> bool {
  if inst.t != InstTyp::Call {
    return false;
  }
  let (tgt, callee, _this, _args, spreads) = inst.as_call();
  if tgt.is_none() || !spreads.is_empty() {
    return false;
  }
  matches!(callee, Arg::Builtin(name) if name == "__optimize_js_object")
}

fn parse_object_literal_fields(inst: &Inst) -> Option<BTreeMap<String, Arg>> {
  if !is_object_literal_alloc(inst) {
    return None;
  }
  let (_tgt, _callee, _this, args, _spreads) = inst.as_call();

  // `__optimize_js_object` encodes each property as a triple:
  //   marker, key, value
  // where marker is one of:
  //   - __optimize_js_object_prop
  //   - __optimize_js_object_prop_computed
  //   - __optimize_js_object_spread
  //
  // For MVP devirtualization we only accept simple constant-key properties with the `*_prop`
  // marker, and we reject computed keys/spreads to stay conservative.
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

fn collect_reachable_labels(cfg: &Cfg) -> Vec<u32> {
  cfg.reverse_postorder()
}

fn build_var_defs(cfg: &Cfg, labels: &[u32]) -> BTreeMap<u32, VarDef> {
  let mut defs = BTreeMap::<u32, VarDef>::new();

  for &label in labels {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block {
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };

      let def = match inst.t {
        InstTyp::VarAssign => match inst.args.get(0) {
          Some(Arg::Var(src)) => VarDef::Alias(*src),
          Some(Arg::Fn(id)) => VarDef::Fn(*id),
          _ => VarDef::Unknown,
        },
        InstTyp::Phi => VarDef::Phi(inst.args.clone()),
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let (_tgt, obj, _op, key) = inst.as_bin();
          VarDef::GetProp {
            obj: obj.clone(),
            key: key.clone(),
          }
        }
        InstTyp::Call => parse_object_literal_fields(inst)
          .map(|fields| VarDef::ObjectLit { fields })
          .unwrap_or(VarDef::Unknown),
        _ => VarDef::Unknown,
      };

      defs
        .entry(tgt)
        .and_modify(|existing| {
          // Non-SSA CFGs may assign the same temp multiple times. Only keep definitions when we can
          // prove the value is constant.
          if !matches!(existing, VarDef::Unknown) {
            *existing = VarDef::Unknown;
          }
        })
        .or_insert(def);
    }
  }

  defs
}

fn is_alias_arg(arg: &Arg, aliases: &BTreeSet<u32>) -> bool {
  matches!(arg, Arg::Var(v) if aliases.contains(v))
}

fn build_object_infos(
  cfg: &Cfg,
  labels: &[u32],
  defs: &BTreeMap<u32, VarDef>,
) -> BTreeMap<u32, ObjectInfo> {
  let mut infos = BTreeMap::<u32, ObjectInfo>::new();

  // Collect object allocation sites.
  let allocs: Vec<(u32, BTreeMap<String, Arg>)> = defs
    .iter()
    .filter_map(|(var, def)| match def {
      VarDef::ObjectLit { fields } => Some((*var, fields.clone())),
      _ => None,
    })
    .collect();

  for (alloc_var, fields) in allocs {
    // Find SSA aliases of the allocation (simple copy chains + phis that merge only aliases).
    let mut aliases = BTreeSet::<u32>::new();
    aliases.insert(alloc_var);
    let mut changed = true;
    while changed {
      changed = false;
      for (tgt, def) in defs.iter() {
        if aliases.contains(tgt) {
          continue;
        }
        match def {
          VarDef::Alias(src) if aliases.contains(src) => {
            aliases.insert(*tgt);
            changed = true;
          }
          VarDef::Phi(args) => {
            let all_alias = args.iter().all(|arg| matches!(arg, Arg::Var(v) if aliases.contains(v)));
            if all_alias {
              aliases.insert(*tgt);
              changed = true;
            }
          }
          _ => {}
        }
      }
    }

    // Scan uses of any alias. If we see an unknown/dynamic use, mark the whole allocation unsafe.
    let mut overwritten_keys = BTreeSet::<String>::new();
    let mut unsafe_ = false;

    for &label in labels {
      let Some(block) = cfg.bblocks.maybe_get(label) else {
        continue;
      };
      for inst in block {
        // Special-case phi nodes: if an alias flows into a phi that is *not* itself an alias, the
        // object may flow into an unknown value, so stay conservative.
        if inst.t == InstTyp::Phi {
          let tgt_is_alias = inst.tgts.get(0).is_some_and(|t| aliases.contains(t));
          if !tgt_is_alias && inst.args.iter().any(|arg| is_alias_arg(arg, &aliases)) {
            unsafe_ = true;
            break;
          }
          continue;
        }

        match inst.t {
          InstTyp::VarAssign => {
            // Alias copies are fine as long as the target is also tracked as an alias.
            if is_alias_arg(&inst.args[0], &aliases) {
              let tgt_is_alias = inst.tgts.get(0).is_some_and(|t| aliases.contains(t));
              if !tgt_is_alias {
                unsafe_ = true;
                break;
              }
            }
          }
          InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
            let (_tgt, obj, _op, key) = inst.as_bin();
            if !is_alias_arg(obj, &aliases) {
              // If the alias appears anywhere else in the instruction, reject.
              if inst.args.iter().any(|arg| is_alias_arg(arg, &aliases)) {
                unsafe_ = true;
                break;
              }
              continue;
            }
            if !matches!(key, Arg::Const(Const::Str(s)) if s != "__proto__") {
              unsafe_ = true;
              break;
            }
          }
          InstTyp::Bin => {
            if inst.args.iter().any(|arg| is_alias_arg(arg, &aliases)) {
              unsafe_ = true;
              break;
            }
          }
          InstTyp::PropAssign => {
            let (obj, prop, val) = inst.as_prop_assign();
            let obj_is_alias = is_alias_arg(obj, &aliases);
            let prop_is_alias = is_alias_arg(prop, &aliases);
            let val_is_alias = is_alias_arg(val, &aliases);

            if prop_is_alias || val_is_alias {
              // Storing the object as a property key/value escapes it.
              unsafe_ = true;
              break;
            }

            if obj_is_alias {
              let Arg::Const(Const::Str(key)) = prop else {
                // Dynamic write could overwrite any key.
                unsafe_ = true;
                break;
              };
              if key == "__proto__" {
                unsafe_ = true;
                break;
              }
              overwritten_keys.insert(key.clone());
            }
          }
          InstTyp::Call => {
            // The only allowed use in calls is passing the object as the explicit `this` value.
            // Passing it as a callee or argument makes it escape to an unknown function.
            let (_tgt, _callee, _this, call_args, _spreads) = inst.as_call();
            let callee_arg = &inst.args[0];
            let this_arg = &inst.args[1];
            if is_alias_arg(callee_arg, &aliases) {
              unsafe_ = true;
              break;
            }
            if call_args.iter().any(|arg| is_alias_arg(arg, &aliases)) {
              unsafe_ = true;
              break;
            }
            // Allow `this` unconditionally (it preserves this semantics for method-style calls).
            let _ = this_arg;
          }
          InstTyp::ForeignStore | InstTyp::UnknownStore | InstTyp::Return | InstTyp::Throw => {
            if inst.args.iter().any(|arg| is_alias_arg(arg, &aliases)) {
              unsafe_ = true;
              break;
            }
          }
          _ => {
            if inst.args.iter().any(|arg| is_alias_arg(arg, &aliases)) {
              unsafe_ = true;
              break;
            }
          }
        }
      }
      if unsafe_ {
        break;
      }
    }

    infos.insert(
      alloc_var,
      ObjectInfo {
        fields,
        overwritten_keys,
        unsafe_,
      },
    );
  }

  infos
}

fn resolve_fn_id(
  arg: &Arg,
  defs: &BTreeMap<u32, VarDef>,
  objects: &BTreeMap<u32, ObjectInfo>,
  visiting: &mut Vec<u32>,
) -> Option<FnId> {
  match arg {
    Arg::Fn(id) => Some(*id),
    Arg::Var(v) => resolve_var_fn_id(*v, defs, objects, visiting),
    _ => None,
  }
}

fn resolve_object_alloc_id(
  arg: &Arg,
  defs: &BTreeMap<u32, VarDef>,
  visiting: &mut Vec<u32>,
) -> Option<u32> {
  match arg {
    Arg::Var(v) => resolve_var_object_alloc_id(*v, defs, visiting),
    _ => None,
  }
}

fn resolve_var_object_alloc_id(
  var: u32,
  defs: &BTreeMap<u32, VarDef>,
  visiting: &mut Vec<u32>,
) -> Option<u32> {
  if visiting.contains(&var) {
    return None;
  }
  visiting.push(var);

  let out = match defs.get(&var) {
    Some(VarDef::ObjectLit { .. }) => Some(var),
    Some(VarDef::Alias(src)) => resolve_var_object_alloc_id(*src, defs, visiting),
    Some(VarDef::Phi(args)) => {
      let mut merged: Option<u32> = None;
      for arg in args {
        let Some(id) = resolve_object_alloc_id(arg, defs, visiting) else {
          visiting.pop();
          return None;
        };
        merged = match merged {
          None => Some(id),
          Some(prev) if prev == id => Some(prev),
          _ => {
            visiting.pop();
            return None;
          }
        };
      }
      merged
    }
    _ => None,
  };

  visiting.pop();
  out
}

fn resolve_var_fn_id(
  var: u32,
  defs: &BTreeMap<u32, VarDef>,
  objects: &BTreeMap<u32, ObjectInfo>,
  visiting: &mut Vec<u32>,
) -> Option<FnId> {
  if visiting.contains(&var) {
    return None;
  }
  visiting.push(var);

  let out = match defs.get(&var) {
    Some(VarDef::Fn(id)) => Some(*id),
    Some(VarDef::Alias(src)) => resolve_var_fn_id(*src, defs, objects, visiting),
    Some(VarDef::Phi(args)) => {
      let mut merged: Option<FnId> = None;
      for arg in args {
        let Some(id) = resolve_fn_id(arg, defs, objects, visiting) else {
          merged = None;
          break;
        };
        merged = match merged {
          None => Some(id),
          Some(prev) if prev == id => Some(prev),
          _ => {
            merged = None;
            break;
          }
        };
      }
      merged
    }
    Some(VarDef::GetProp { obj, key }) => (|| {
      let prop = match key {
        Arg::Const(Const::Str(prop)) if prop != "__proto__" => prop,
        _ => return None,
      };

      let mut obj_visiting = Vec::new();
      let alloc = resolve_object_alloc_id(obj, defs, &mut obj_visiting)?;
      let info = objects.get(&alloc)?;
      if info.unsafe_ || info.overwritten_keys.contains(prop) {
        return None;
      }
      let init = info.fields.get(prop)?;
      resolve_fn_id(init, defs, objects, visiting)
    })(),
    _ => None,
  };

  visiting.pop();
  out
}

/// Devirtualize indirect calls whose callee resolves to a unique `FnId`.
///
/// MVP:
/// - Alias chains through `VarAssign`
/// - Phi nodes that merge a single `FnId`
/// - `GetProp(obj, "k")` when `obj` is a `__optimize_js_object` allocation with a constant-key
///   initializer of a known function and the field is never overwritten/escaped.
pub fn optpass_devirtualize(cfg: &mut Cfg) -> PassResult {
  let labels = collect_reachable_labels(cfg);
  let defs = build_var_defs(cfg, &labels);
  let objects = build_object_infos(cfg, &labels, &defs);

  let mut result = PassResult::default();

  for label in labels {
    let block = cfg.bblocks.get_mut(label);
    for inst in block.iter_mut() {
      if inst.t != InstTyp::Call {
        continue;
      }
      let callee = inst.args.get(0).expect("Call args[0] callee").clone();
      let mut visiting = Vec::new();
      let Some(id) = resolve_fn_id(&callee, &defs, &objects, &mut visiting) else {
        continue;
      };
      if matches!(callee, Arg::Fn(existing) if existing == id) {
        continue;
      }

      inst.args[0] = Arg::Fn(id);
      // Purity/effect metadata may have been computed using the previous call target. Reset to the
      // conservative defaults so later passes do not observe stale information.
      inst.meta.callee_purity = Purity::Impure;
      inst.meta.effects.mark_unknown();

      result.mark_changed();
    }
  }

  result
}
