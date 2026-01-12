//! SSA layout propagation and verification.
//!
//! Native (LLVM) lowering requires that each SSA variable has a single,
//! unambiguous `LayoutId`. This module builds a `Var(u32) -> LayoutId` map for a
//! [`Cfg`] and rejects IR that would require implicit layout conversions (e.g.
//! a `Phi` that merges values with incompatible layouts).

use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, Const, Inst, InstTyp};
use crate::types::LayoutId;
use ahash::HashMap;
use diagnostics::{Diagnostic, FileId, Span, TextRange};
use once_cell::sync::Lazy;
use std::ops::Deref;
use std::sync::Arc;

/// Per-function mapping from SSA variables to their native runtime layout.
///
/// In [`LayoutValidationMode::BestEffort`] mode this may also contain warning
/// diagnostics describing values that were forced to `unknown_layout`.
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
  // `types-ts-interned` uses a canonical closure payload layout, so the
  // particular signature/return type chosen here does not matter for the ABI.
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
  mode: LayoutValidationMode,
  code: &'static str,
  message: String,
  inst: &Inst,
) -> Diagnostic {
  // `validate_layouts` currently only sees a `Cfg`, not the full `Program`, so it
  // cannot reliably recover the original file id. Most `optimize-js` entry
  // points use `FileId(0)`; we still use the best-effort `InstMeta.span` to
  // point at the right source range when available.
  let span = Span::new(FileId(0), inst.meta.span.unwrap_or_else(|| TextRange::new(0, 0)));
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
  // Only handle builtins that are commonly treated as constants by the IL.
  // Everything else is left as unknown (and should typically flow through a
  // variable with `InstMeta.native_layout`).
  let store = &*FALLBACK_STORE;
  let prim = store.primitive_ids();
  match value {
    "undefined" => Some(store.layout_of(prim.undefined)),
    "NaN" | "Infinity" => Some(store.layout_of(prim.number)),
    // `Symbol.iterator`, etc. Symbols currently lower to opaque pointers in the
    // layout model.
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

fn set_layout(map: &mut LayoutMap, mode: LayoutValidationMode, var: u32, layout: LayoutId, inst: &Inst) -> bool {
  match map.layouts.get(&var).copied() {
    None => {
      map.layouts.insert(var, layout);
      true
    }
    Some(existing) if existing == layout => false,
    Some(existing) => {
      map.diagnostics.push(diagnostic_for_inst(
        mode,
        "OPT0103",
        format!("conflicting layouts for %{var}: {existing:?} vs {layout:?}"),
        inst,
      ));
      false
    }
  }
}

/// Validate layout consistency for an SSA-form CFG and return a `Var -> LayoutId` map.
///
/// The validator consumes `InstMeta::native_layout` as the authoritative layout for
/// value-defining instructions. `Phi` layouts are computed as the join of their
/// incoming values.
pub fn validate_layouts(cfg: &Cfg, mode: LayoutValidationMode) -> Result<LayoutMap, Vec<Diagnostic>> {
  let unknown_layout = fallback_unknown_layout();
  let mut map = LayoutMap::default();

  // Deterministic outer iteration: we only ever assign layouts (monotone) so a
  // bounded fixpoint converges quickly even in the presence of cycles (loop
  // phis, etc).
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
            let mut joined: Option<LayoutId> = None;
            let mut mismatch: Option<(LayoutId, LayoutId)> = None;
            for arg in inst.args.iter() {
              let Some(layout) = arg_layout(arg, &map, unknown_layout) else {
                continue;
              };
              match joined {
                None => joined = Some(layout),
                Some(existing) if existing == layout => {}
                Some(existing) => {
                  mismatch = Some((existing, layout));
                  break;
                }
              }
            }

            if let Some((a, b)) = mismatch {
              map.diagnostics.push(diagnostic_for_inst(
                mode,
                "OPT0101",
                format!("phi merges incompatible layouts: {a:?} vs {b:?}"),
                inst,
              ));
              if mode == LayoutValidationMode::BestEffort {
                changed |= set_layout(&mut map, mode, tgt, unknown_layout, inst);
              }
              continue;
            }

            let Some(layout) = joined else {
              if mode == LayoutValidationMode::BestEffort {
                changed |= set_layout(&mut map, mode, tgt, unknown_layout, inst);
              }
              continue;
            };

            // The phi result must match the joined layout.
            changed |= set_layout(&mut map, mode, tgt, layout, inst);

            // Propagate the phi layout back into any incoming vars that haven't
            // been assigned yet (e.g. function parameters).
            for arg in inst.args.iter() {
              let Arg::Var(v) = arg else {
                continue;
              };
              if map.get(*v).is_none() {
                changed |= set_layout(&mut map, mode, *v, layout, inst);
              }
            }
          }

          InstTyp::VarAssign => {
            let declared = inst.meta.native_layout;
            if let Some(layout) = declared {
              changed |= set_layout(&mut map, mode, tgt, layout, inst);
            } else if mode == LayoutValidationMode::BestEffort {
              changed |= set_layout(&mut map, mode, tgt, unknown_layout, inst);
            } else {
              map.diagnostics.push(diagnostic_for_inst(
                mode,
                "OPT0100",
                format!("missing InstMeta.native_layout for VarAssign result %{tgt}"),
                inst,
              ));
            }

            // Validate copy assignments (`%tgt = %src`) preserve layouts. We only
            // check `Arg::Var` sources: other RHS kinds (Const/Builtin/Fn) are
            // not "copies" and their ABI/layout is defined by the target
            // instruction metadata.
            if let Some(Arg::Var(src_var)) = inst.args.get(0) {
              let tgt_layout = map.get(tgt);
              let src_layout = map.get(*src_var);

              match (tgt_layout, src_layout) {
                (Some(tgt_layout), Some(src_layout)) => {
                  if tgt_layout != src_layout {
                    map.diagnostics.push(diagnostic_for_inst(
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
                  changed |= set_layout(&mut map, mode, *src_var, tgt_layout, inst);
                }
                (None, Some(src_layout)) => {
                  // Infer the target layout from the source when the VarAssign
                  // lacks metadata in best-effort mode.
                  if mode == LayoutValidationMode::BestEffort {
                    changed |= set_layout(&mut map, mode, tgt, src_layout, inst);
                  }
                }
                _ => {}
              }
            }
          }

          _ => {
            if let Some(layout) = inst.meta.native_layout {
              changed |= set_layout(&mut map, mode, tgt, layout, inst);
            } else if mode == LayoutValidationMode::BestEffort {
              changed |= set_layout(&mut map, mode, tgt, unknown_layout, inst);
              map.diagnostics.push(diagnostic_for_inst(
                mode,
                "OPT0100",
                format!("missing InstMeta.native_layout for instruction result %{tgt}"),
                inst,
              ));
            } else {
              map.diagnostics.push(diagnostic_for_inst(
                mode,
                "OPT0100",
                format!("missing InstMeta.native_layout for instruction result %{tgt}"),
                inst,
              ));
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
