//! SSA native layout propagation and validation.
//!
//! Typed native backends treat `InstMeta::native_layout` as the ABI/layout for an SSA value.
//! SSA construction and optimisation passes can introduce new SSA defs (notably `Phi` nodes and
//! preheader temps) which must also carry layout metadata.
//!
//! This module provides two related utilities:
//! - [`propagate_cfg_native_layouts`]: best-effort propagation of layouts through SSA, writing the
//!   result back into `InstMeta::native_layout` for every value-defining instruction.
//! - [`validate_layouts`]/[`validate_layouts_with_file`]: strict verification that the CFG has
//!   consistent layouts (e.g. no `Phi` node merges incompatible layouts, no `VarAssign` lies about
//!   copy layouts), producing deterministic diagnostics.
//!
//! These are complementary: propagation keeps metadata stable across passes, while validation is
//! used by strict-native verifiers to reject IR that would require implicit layout conversions.

use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, Const, Inst, InstTyp};
use crate::types::{LayoutId, TypeContext, TypeId};
use ahash::HashMap;
use ahash::HashMapExt;
use diagnostics::{Diagnostic, FileId, Span, TextRange};
use once_cell::sync::Lazy;
use std::collections::BTreeSet;
use std::ops::Deref;
use std::sync::Arc;

/// Per-function mapping from SSA variables to their native runtime layout.
///
/// In [`LayoutValidationMode::BestEffort`] mode this may also contain warning diagnostics describing
/// values that were forced to an "unknown" layout.
#[derive(Clone, Debug, Default)]
pub struct LayoutMap {
  layouts: HashMap<u32, LayoutId>,
  diagnostics: Vec<Diagnostic>,
}

impl LayoutMap {
  pub fn get(&self, var: u32) -> Option<LayoutId> {
    self.layouts.get(&var).copied()
  }

  pub fn diagnostics(&self) -> &[Diagnostic] {
    &self.diagnostics
  }
}

impl Deref for LayoutMap {
  type Target = HashMap<u32, LayoutId>;

  fn deref(&self) -> &Self::Target {
    &self.layouts
  }
}

/// Controls how strictly layout conflicts are handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutValidationMode {
  /// Reject any missing or conflicting layout information.
  Strict,
  /// Continue compilation by assigning an "unknown" layout and emitting warnings.
  BestEffort,
}

static FALLBACK_STORE: Lazy<Arc<types_ts_interned::TypeStore>> = Lazy::new(types_ts_interned::TypeStore::new);
static FALLBACK_FN_LAYOUT: Lazy<LayoutId> = Lazy::new(|| {
  // Model `Arg::Fn` constants as the layout of a representative callable type.
  // `types-ts-interned` uses a canonical closure payload layout, so the particular
  // signature/return type chosen here does not matter for the ABI.
  let store = &*FALLBACK_STORE;
  let prim = store.primitive_ids();
  let sig = types_ts_interned::Signature {
    params: Vec::new(),
    ret: prim.unknown,
    this_param: None,
    type_params: Vec::new(),
  };
  let sig_id = store.intern_signature(sig);
  let ty = store.intern_type(types_ts_interned::TypeKind::Callable {
    overloads: vec![sig_id],
  });
  store.layout_of(ty)
});

fn fallback_unknown_layout() -> LayoutId {
  let store = &*FALLBACK_STORE;
  store.layout_of(store.primitive_ids().unknown)
}

fn layout_of_const(value: &Const) -> LayoutId {
  let store = &*FALLBACK_STORE;
  let prim = store.primitive_ids();
  let ty = match value {
    Const::Bool(_) => prim.boolean,
    Const::Num(_) => prim.number,
    Const::Str(_) => prim.string,
    Const::Null => prim.null,
    Const::Undefined => prim.undefined,
    Const::BigInt(_) => prim.bigint,
  };
  store.layout_of(ty)
}

fn diagnostic_for_inst(
  file: FileId,
  mode: LayoutValidationMode,
  code: &'static str,
  message: String,
  inst: &Inst,
) -> Diagnostic {
  // `validate_layouts*` typically only sees a `Cfg`, not the full `Program`, so the caller may need
  // to supply the file id. `validate_layouts` defaults to `FileId(0)` (matching `compile_source`);
  // `validate_layouts_with_file` is available for typed pipelines that keep the original file id.
  let span = Span::new(file, inst.meta.span.unwrap_or_else(|| TextRange::new(0, 0)));
  let mut diag = match mode {
    LayoutValidationMode::Strict => Diagnostic::error(code, message, span),
    LayoutValidationMode::BestEffort => Diagnostic::warning(code, message, span),
  };
  if let Some(expr) = inst.meta.hir_expr {
    diag.push_note(format!("hir expr: {expr:?}"));
  }
  diag
}

fn layout_of_builtin(value: &str) -> Option<LayoutId> {
  // Only handle builtins that are commonly treated as constants by the IL. Everything else is left
  // as unknown (and should typically flow through a variable with `InstMeta.native_layout`).
  let store = &*FALLBACK_STORE;
  let prim = store.primitive_ids();
  match value {
    "undefined" => Some(store.layout_of(prim.undefined)),
    "NaN" | "Infinity" => Some(store.layout_of(prim.number)),
    // `Symbol.iterator`, etc. Symbols currently lower to opaque pointers in the layout model.
    v if v.starts_with("Symbol.") => Some(store.layout_of(prim.symbol)),
    _ => None,
  }
}

fn arg_layout(arg: &Arg, map: &LayoutMap, unknown_layout: LayoutId) -> Option<LayoutId> {
  match arg {
    Arg::Var(v) => map.get(*v),
    Arg::Const(c) => Some(layout_of_const(c)),
    Arg::Fn(_) => Some(*FALLBACK_FN_LAYOUT),
    Arg::Builtin(name) => layout_of_builtin(name).or(Some(unknown_layout)),
  }
}

fn set_layout(
  map: &mut LayoutMap,
  file: FileId,
  mode: LayoutValidationMode,
  var: u32,
  layout: LayoutId,
  inst: &Inst,
) -> bool {
  match map.layouts.get(&var).copied() {
    None => {
      map.layouts.insert(var, layout);
      true
    }
    Some(existing) if existing == layout => false,
    Some(existing) => {
      map.diagnostics.push(diagnostic_for_inst(
        file,
        mode,
        "OPT0103",
        format!("conflicting layouts for %{var}: {existing:?} vs {layout:?}"),
        inst,
      ));
      false
    }
  }
}

fn phi_incoming_layouts(
  inst: &Inst,
  map: &LayoutMap,
  unknown_layout: LayoutId,
) -> Vec<(u32, Option<LayoutId>)> {
  let mut incoming: Vec<(u32, Option<LayoutId>)> = inst
    .labels
    .iter()
    .copied()
    .zip(inst.args.iter())
    .map(|(pred, arg)| (pred, arg_layout(arg, map, unknown_layout)))
    .collect();
  incoming.sort_by_key(|(pred, _)| *pred);
  incoming
}

fn fmt_incoming(incoming: &[(u32, Option<LayoutId>)]) -> String {
  incoming
    .iter()
    .map(|(pred, layout)| match layout {
      Some(layout) => format!("{pred}=>{layout:?}"),
      None => format!("{pred}=><unresolved>"),
    })
    .collect::<Vec<_>>()
    .join(", ")
}

/// Validate layout consistency for an SSA-form CFG and return a `Var -> LayoutId` map.
///
/// The validator treats `InstMeta::native_layout` as the authoritative layout for
/// value-defining instructions. `Phi` layouts are computed as the join of their
/// incoming values, and `VarAssign` layouts can be inferred from their RHS for common cases.
pub fn validate_layouts(cfg: &Cfg, mode: LayoutValidationMode) -> Result<LayoutMap, Vec<Diagnostic>> {
  validate_layouts_with_file(cfg, FileId(0), mode)
}

/// Like [`validate_layouts`] but uses the provided `file` id for diagnostics.
pub fn validate_layouts_with_file(
  cfg: &Cfg,
  file: FileId,
  mode: LayoutValidationMode,
) -> Result<LayoutMap, Vec<Diagnostic>> {
  let unknown_layout = fallback_unknown_layout();
  let mut map = LayoutMap::default();

  // Deterministic outer iteration: we only ever assign layouts (monotone) so a bounded fixpoint
  // converges quickly even in the presence of cycles (loop phis, etc).
  let mut changed = true;
  let mut iterations = 0usize;
  let max_iterations = 64usize;

  while changed && iterations < max_iterations {
    iterations += 1;
    changed = false;

    for label in cfg.graph.labels_sorted() {
      let Some(block) = cfg.bblocks.maybe_get(label) else {
        continue;
      };
      for inst in block.iter() {
        let Some(&tgt) = inst.tgts.get(0) else {
          continue;
        };

        match inst.t {
          InstTyp::Phi => {
            let incoming = phi_incoming_layouts(inst, &map, unknown_layout);
            let mut distinct = BTreeSet::new();
            for (_, layout) in &incoming {
              if let Some(layout) = *layout {
                distinct.insert(layout);
              }
            }

            if distinct.len() > 1 {
              map.diagnostics.push(diagnostic_for_inst(
                file,
                mode,
                "OPT0101",
                format!("phi merges incompatible layouts: [{}]", fmt_incoming(&incoming)),
                inst,
              ));
              if mode == LayoutValidationMode::BestEffort {
                changed |= set_layout(&mut map, file, mode, tgt, unknown_layout, inst);
              }
              continue;
            }

            let layout = distinct.into_iter().next();
            let Some(layout) = layout else {
              if mode == LayoutValidationMode::BestEffort {
                changed |= set_layout(&mut map, file, mode, tgt, unknown_layout, inst);
              }
              continue;
            };

            // The phi result must match the joined layout.
            changed |= set_layout(&mut map, file, mode, tgt, layout, inst);

            // Propagate the phi layout back into any incoming vars that haven't been assigned yet
            // (e.g. function parameters).
            for arg in inst.args.iter() {
              let Arg::Var(v) = arg else {
                continue;
              };
              if map.get(*v).is_none() {
                changed |= set_layout(&mut map, file, mode, *v, layout, inst);
              }
            }
          }

          InstTyp::VarAssign => {
            let declared = inst.meta.native_layout;
            if let Some(layout) = declared {
              changed |= set_layout(&mut map, file, mode, tgt, layout, inst);
            } else if let Some(rhs) = inst.args.get(0) {
              // VarAssign can be both a copy (`%t = %x`) and an actual definition (`%t = 123`,
              // `%t = Fn0`, `%t = undefined`). When lowering did not attach `InstMeta.native_layout`,
              // we can still infer the layout from the RHS for most cases.
              if let Some(rhs_layout) = arg_layout(rhs, &map, unknown_layout) {
                changed |= set_layout(&mut map, file, mode, tgt, rhs_layout, inst);
              } else if mode == LayoutValidationMode::BestEffort {
                changed |= set_layout(&mut map, file, mode, tgt, unknown_layout, inst);
              }
            } else if mode == LayoutValidationMode::BestEffort {
              changed |= set_layout(&mut map, file, mode, tgt, unknown_layout, inst);
            }

            // Validate copy assignments (`%tgt = %src`) preserve layouts. We only check `Arg::Var`
            // sources: other RHS kinds (Const/Builtin/Fn) are not "copies" and their ABI/layout is
            // defined by the target instruction metadata.
            if let Some(Arg::Var(src_var)) = inst.args.get(0) {
              let tgt_layout = map.get(tgt);
              let src_layout = map.get(*src_var);

              match (tgt_layout, src_layout) {
                (Some(tgt_layout), Some(src_layout)) => {
                  if tgt_layout != src_layout {
                    map.diagnostics.push(diagnostic_for_inst(
                      file,
                      mode,
                      "OPT0102",
                      format!(
                        "copy assigns between incompatible layouts: %{tgt}={tgt_layout:?} from %{src_var}={src_layout:?}"
                      ),
                      inst,
                    ));
                  }
                }
                (Some(tgt_layout), None) => {
                  // Infer the source layout from the target (common for params).
                  changed |= set_layout(&mut map, file, mode, *src_var, tgt_layout, inst);
                }
                (None, Some(src_layout)) => {
                  // Infer the target layout from the source when the VarAssign lacks metadata in
                  // best-effort mode.
                  if mode == LayoutValidationMode::BestEffort {
                    changed |= set_layout(&mut map, file, mode, tgt, src_layout, inst);
                  }
                }
                _ => {}
              }
            }
          }

          _ => {
            if let Some(layout) = inst.meta.native_layout {
              changed |= set_layout(&mut map, file, mode, tgt, layout, inst);
            } else {
              map.diagnostics.push(diagnostic_for_inst(
                file,
                mode,
                "OPT0100",
                format!("missing InstMeta.native_layout for instruction result %{tgt}"),
                inst,
              ));
              if mode == LayoutValidationMode::BestEffort {
                changed |= set_layout(&mut map, file, mode, tgt, unknown_layout, inst);
              }
            }
          }
        }
      }
    }
  }

  // Ensure every value-defining SSA variable got a layout assignment.
  if mode == LayoutValidationMode::Strict {
    for label in cfg.graph.labels_sorted() {
      let Some(block) = cfg.bblocks.maybe_get(label) else {
        continue;
      };
      for inst in block.iter() {
        let Some(&tgt) = inst.tgts.get(0) else {
          continue;
        };
        if map.get(tgt).is_none() {
          map.diagnostics.push(diagnostic_for_inst(
            file,
            mode,
            "OPT0100",
            format!("missing layout for SSA value %{tgt}"),
            inst,
          ));
        }
      }
    }
  }

  if mode == LayoutValidationMode::Strict && !map.diagnostics.is_empty() {
    return Err(map.diagnostics);
  }

  Ok(map)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutState {
  /// No constraints known (any layout is possible).
  Any,
  /// Exactly one layout is possible.
  Exact(LayoutId),
  /// Conflicting constraints (no single layout satisfies all requirements).
  Conflict,
}

impl LayoutState {
  fn meet(self, other: Self) -> Self {
    use LayoutState::*;
    match (self, other) {
      (Conflict, _) | (_, Conflict) => Conflict,
      (Any, x) | (x, Any) => x,
      (Exact(a), Exact(b)) => {
        if a == b {
          Exact(a)
        } else {
          Conflict
        }
      }
    }
  }

  fn as_layout(self) -> Option<LayoutId> {
    match self {
      LayoutState::Exact(id) => Some(id),
      LayoutState::Any | LayoutState::Conflict => None,
    }
  }
}

fn type_store(types: &TypeContext) -> Option<Arc<types_ts_interned::TypeStore>> {
  Some(types.program.as_ref()?.interned_type_store())
}

fn layout_of_type(store: Option<&types_ts_interned::TypeStore>, ty: TypeId) -> Option<LayoutId> {
  let store = store?;
  if !store.contains_type_id(ty) {
    return None;
  }
  Some(store.layout_of(store.canon(ty)))
}

fn state_of_arg(
  arg: &Arg,
  vars: &HashMap<u32, LayoutState>,
  _store: Option<&types_ts_interned::TypeStore>,
) -> LayoutState {
  match arg {
    Arg::Var(v) => vars.get(v).copied().unwrap_or(LayoutState::Any),
    Arg::Const(c) => LayoutState::Exact(layout_of_const(c)),
    Arg::Fn(_) => LayoutState::Exact(*FALLBACK_FN_LAYOUT),
    Arg::Builtin(name) => LayoutState::Exact(layout_of_builtin(name).unwrap_or_else(fallback_unknown_layout)),
  }
}

fn def_constraint(
  inst: &Inst,
  vars: &HashMap<u32, LayoutState>,
  store: Option<&types_ts_interned::TypeStore>,
) -> LayoutState {
  match inst.t {
    InstTyp::Phi => inst
      .args
      .iter()
      .fold(LayoutState::Any, |acc, arg| acc.meet(state_of_arg(arg, vars, store))),
    InstTyp::VarAssign => {
      if let Some(layout) = inst.meta.native_layout {
        return LayoutState::Exact(layout);
      }
      if let Some(type_id) = inst.meta.type_id {
        if let Some(layout) = layout_of_type(store, type_id) {
          return LayoutState::Exact(layout);
        }
      }
      inst
        .args
        .get(0)
        .map(|arg| state_of_arg(arg, vars, store))
        .unwrap_or(LayoutState::Any)
    }
    _ => {
      if let Some(layout) = inst.meta.native_layout {
        return LayoutState::Exact(layout);
      }
      if let Some(type_id) = inst.meta.type_id {
        return layout_of_type(store, type_id)
          .map(LayoutState::Exact)
          .unwrap_or(LayoutState::Any);
      }
      LayoutState::Any
    }
  }
}

/// Compute and write back `InstMeta::native_layout` for all value defs in `cfg`.
pub fn propagate_cfg_native_layouts(cfg: &mut Cfg, types: &TypeContext) -> HashMap<u32, Option<LayoutId>> {
  let store_arc = type_store(types);
  let store = store_arc.as_deref();

  // Deterministic list of SSA defs (label, inst index, tgt).
  let mut defs: Vec<(u32, usize, u32)> = Vec::new();
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  for label in labels {
    let block = cfg.bblocks.get(label);
    for (idx, inst) in block.iter().enumerate() {
      if let Some(&tgt) = inst.tgts.get(0) {
        defs.push((label, idx, tgt));
      }
    }
  }

  let mut var_states: HashMap<u32, LayoutState> = HashMap::new();
  for &(_, _, tgt) in &defs {
    var_states.entry(tgt).or_insert(LayoutState::Any);
  }

  // Iterate to a fixpoint. `LayoutState` only refines (`Any` -> `Exact` -> `Conflict`), so this
  // converges quickly even with loops.
  loop {
    let mut changed = false;
    for &(label, idx, tgt) in &defs {
      let inst = &cfg.bblocks.get(label)[idx];
      let new_state = def_constraint(inst, &var_states, store);
      let entry = var_states.entry(tgt).or_insert(LayoutState::Any);
      if *entry != new_state {
        *entry = new_state;
        changed = true;
      }
    }
    if !changed {
      break;
    }
  }

  let mut out: HashMap<u32, Option<LayoutId>> = HashMap::new();
  for &(label, idx, tgt) in &defs {
    let state = var_states.get(&tgt).copied().unwrap_or(LayoutState::Any);
    let layout = state.as_layout();
    out.insert(tgt, layout);
    cfg.bblocks.get_mut(label)[idx].meta.native_layout = layout;
  }
  out
}
