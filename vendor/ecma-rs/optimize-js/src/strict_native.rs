//! Strict-native verifier for optimized IL.
//!
//! The native AOT backend consumes optimized `optimize-js` IL/CFGs (typically in
//! SSA form via [`crate::ProgramFunction::analyzed_cfg`]). This module enforces
//! the subset and invariants required by strict-native compilation at the IL
//! boundary so downstream backends do not need to re-walk HIR/AST.

use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use crate::{Diagnostic, Program, ProgramScopeKind, Span, TextRange};
use crate::{ScopeId, SymbolId};
use ahash::HashMap;
use ahash::HashMapExt;
#[cfg(feature = "semantic-ops")]
use once_cell::sync::Lazy;

const CODE_UNKNOWN_MEMORY: &str = "OPTN0001";
const CODE_FOREIGN_GLOBAL: &str = "OPTN0002";
const CODE_SPREAD_CALL: &str = "OPTN0003";
const CODE_DYNAMIC_PROP: &str = "OPTN0004";
const CODE_BANNED_BUILTIN: &str = "OPTN0005";
const CODE_MISSING_TYPE_ID: &str = "OPTN0006";

/// Options controlling strict-native validation at the IL boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrictNativeOpts {
  /// Allow [`InstTyp::UnknownLoad`] / [`InstTyp::UnknownStore`].
  ///
  /// This is useful for non-strict JS modes where unknown/global identifier
  /// accesses are lowered to unknown memory operations.
  pub allow_unknown_memory: bool,
  /// Allow [`InstTyp::ForeignLoad`] / [`InstTyp::ForeignStore`] accesses to
  /// symbols declared in the global scope.
  pub allow_foreign_global: bool,
  /// Allow [`InstTyp::ForeignLoad`] / [`InstTyp::ForeignStore`] accesses when
  /// the symbol cannot be resolved to a known scope.
  pub allow_foreign_unknown: bool,
  /// Allow [`InstTyp::Call`] with spread arguments (`Inst::spreads` non-empty).
  pub allow_spread_calls: bool,
  /// When enabled, require typed builds to have a `type_id` for every
  /// value-defining instruction.
  ///
  /// This is ignored when the crate is built without `--features typed`.
  pub require_type_ids: bool,
}

impl Default for StrictNativeOpts {
  fn default() -> Self {
    Self {
      allow_unknown_memory: false,
      allow_foreign_global: false,
      allow_foreign_unknown: false,
      allow_spread_calls: false,
      require_type_ids: cfg!(feature = "typed"),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ValueState {
  Unknown,
  /// Compile-time-known constant argument (must not be `Arg::Var`).
  Known(Arg),
  /// Known not to be a compile-time constant.
  NonConst,
}

impl Default for ValueState {
  fn default() -> Self {
    Self::Unknown
  }
}

fn merge_state(old: ValueState, new: ValueState) -> ValueState {
  use ValueState::*;
  match (old, new) {
    (NonConst, _) | (_, NonConst) => NonConst,
    (Known(a), Known(b)) => {
      if a == b {
        Known(a)
      } else {
        NonConst
      }
    }
    (Known(a), Unknown) => Known(a),
    (Unknown, Known(b)) => Known(b),
    (Unknown, Unknown) => Unknown,
  }
}

fn eval_arg_state(arg: &Arg, vars: &HashMap<u32, ValueState>) -> ValueState {
  match arg {
    Arg::Const(_) | Arg::Builtin(_) | Arg::Fn(_) => ValueState::Known(arg.clone()),
    Arg::Var(v) => vars.get(v).cloned().unwrap_or(ValueState::Unknown),
  }
}

fn value_state_for_inst(inst: &Inst, vars: &HashMap<u32, ValueState>) -> ValueState {
  match inst.t {
    InstTyp::VarAssign => eval_arg_state(&inst.args[0], vars),
    InstTyp::Phi => {
      let mut saw_unknown = false;
      let mut known: Option<Arg> = None;
      for arg in &inst.args {
        match eval_arg_state(arg, vars) {
          ValueState::NonConst => return ValueState::NonConst,
          ValueState::Unknown => saw_unknown = true,
          ValueState::Known(v) => match known.as_ref() {
            None => known = Some(v),
            Some(existing) if existing == &v => {}
            Some(_) => return ValueState::NonConst,
          },
        }
      }
      if saw_unknown {
        ValueState::Unknown
      } else {
        known.map(ValueState::Known).unwrap_or(ValueState::Unknown)
      }
    }
    InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
      let obj = eval_arg_state(&inst.args[0], vars);
      let prop = eval_arg_state(&inst.args[1], vars);

      // Best-effort builtin path reconstruction so we can ban calls like
      // `Reflect.setPrototypeOf` even after lowering.
      match (obj, prop) {
        (ValueState::Known(Arg::Builtin(base)), ValueState::Known(Arg::Const(Const::Str(prop)))) => {
          ValueState::Known(Arg::Builtin(format!("{base}.{prop}")))
        }
        (ValueState::Known(Arg::Builtin(base)), ValueState::Known(Arg::Const(Const::Num(prop)))) => {
          // Numeric property names are valid; stringify to match builtin.js paths.
          ValueState::Known(Arg::Builtin(format!("{base}.{}", prop.0)))
        }
        (ValueState::Unknown, _) | (_, ValueState::Unknown) => ValueState::Unknown,
        _ => ValueState::NonConst,
      }
    }
    _ => ValueState::NonConst,
  }
}

fn compute_value_states(cfg: &Cfg) -> HashMap<u32, ValueState> {
  // Deterministic traversal order.
  let labels = cfg.graph.labels_sorted();
  let mut defs: Vec<(u32, &Inst)> = Vec::new();
  for label in labels.iter().copied() {
    for inst in cfg.bblocks.get(label).iter() {
      for &tgt in inst.tgts.iter() {
        defs.push((tgt, inst));
      }
    }
  }

  let mut vars: HashMap<u32, ValueState> = HashMap::new();
  for (tgt, _) in defs.iter().copied() {
    vars.entry(tgt).or_insert(ValueState::Unknown);
  }

  loop {
    let mut changed = false;
    for (tgt, inst) in defs.iter().copied() {
      let computed = value_state_for_inst(inst, &vars);
      let prev = vars.get(&tgt).cloned().unwrap_or(ValueState::Unknown);
      let merged = merge_state(prev.clone(), computed);
      if merged != prev {
        vars.insert(tgt, merged);
        changed = true;
      }
    }
    if !changed {
      break;
    }
  }

  vars
}

#[cfg(feature = "typed")]
fn compute_var_layouts(cfg: &Cfg) -> HashMap<u32, types_ts_interned::LayoutId> {
  let labels = cfg.graph.labels_sorted();
  let mut layouts = HashMap::new();
  for label in labels {
    for inst in cfg.bblocks.get(label).iter() {
      let Some(layout) = inst.meta.native_layout else {
        continue;
      };
      for &tgt in &inst.tgts {
        // SSA-form CFGs should only define each variable once; keep the first entry
        // deterministically if we ever see duplicates.
        layouts.entry(tgt).or_insert(layout);
      }
    }
  }
  layouts
}

fn resolve_known_arg(arg: &Arg, vars: &HashMap<u32, ValueState>) -> Option<Arg> {
  match arg {
    Arg::Var(v) => match vars.get(v)? {
      ValueState::Known(value) => Some(value.clone()),
      _ => None,
    },
    Arg::Const(_) | Arg::Builtin(_) | Arg::Fn(_) => Some(arg.clone()),
  }
}

fn resolve_builtin_path(arg: &Arg, vars: &HashMap<u32, ValueState>) -> Option<String> {
  match resolve_known_arg(arg, vars)? {
    Arg::Builtin(path) => Some(path),
    _ => None,
  }
}

fn is_static_prop_key(arg: &Arg, vars: &HashMap<u32, ValueState>) -> bool {
  let Some(resolved) = resolve_known_arg(arg, vars) else {
    return false;
  };
  match resolved {
    Arg::Const(Const::Str(_)) => true,
    Arg::Const(Const::Num(_)) => true,
    Arg::Builtin(path) => path.starts_with("Symbol."),
    _ => false,
  }
}

fn resolves_to_proto_key(arg: &Arg, vars: &HashMap<u32, ValueState>) -> bool {
  matches!(
    resolve_known_arg(arg, vars),
    Some(Arg::Const(Const::Str(s))) if s == "__proto__"
  )
}

fn is_banned_root_call(path: &str, root: &str) -> bool {
  let Some(rest) = path.strip_prefix(root) else {
    return false;
  };
  // Ban `root` itself and anything directly under it (e.g. `eval.call`).
  rest.is_empty() || rest.starts_with('.')
}

fn is_banned_function_constructor_call(path: &str, root: &str) -> bool {
  let Some(rest) = path.strip_prefix(root) else {
    return false;
  };
  // `Function.prototype.*` is used for normal function invocation; banning it at the strict-native
  // IL layer is too broad. We still ban invoking `Function` itself (including `Function.call`,
  // `Function.apply`, etc), since that can be used to synthesize code dynamically.
  if rest.starts_with(".prototype.") {
    return false;
  }
  rest.is_empty() || rest.starts_with('.')
}

fn is_banned_builtin_call(path: &str) -> bool {
  // Keep this list small and conservative: false positives here are painful.
  is_banned_root_call(path, "eval")
    || is_banned_function_constructor_call(path, "Function")
    || is_banned_root_call(path, "Proxy")
    || is_banned_root_call(path, "Reflect.setPrototypeOf")
    || is_banned_root_call(path, "Object.setPrototypeOf")
    || is_banned_root_call(path, "globalThis.eval")
    || is_banned_function_constructor_call(path, "globalThis.Function")
    || is_banned_root_call(path, "globalThis.Proxy")
}

fn is_banned_constructor(path: &str) -> bool {
  matches!(path, "Function" | "Proxy" | "globalThis.Function" | "globalThis.Proxy")
}

#[cfg(feature = "semantic-ops")]
fn banned_known_api_call(api: hir_js::ApiId) -> Option<&'static str> {
  // NOTE: Keep in sync with `is_banned_builtin_call` / `is_banned_constructor`.
  //
  // `KnownApiCall` bypasses builtin-path reconstruction (there is no callee
  // `Arg::Builtin`), so strict-native must explicitly reject known APIs that are
  // otherwise banned (e.g. `eval`).
  static BANNED: Lazy<Vec<(hir_js::ApiId, &'static str)>> = Lazy::new(|| {
    [
      "eval",
      "Function",
      "Proxy",
      "Proxy.revocable",
      "Reflect.setPrototypeOf",
      "Object.setPrototypeOf",
      "globalThis.eval",
      "globalThis.Function",
      "globalThis.Proxy",
      "globalThis.Proxy.revocable",
    ]
    .into_iter()
    .map(|name| (hir_js::ApiId::from_name(name), name))
    .collect()
  });

  BANNED.iter().find(|(id, _)| *id == api).map(|(_, name)| *name)
}

fn span_for_inst(program: &Program, inst: &Inst) -> Span {
  let range = inst
    .meta
    .span
    .unwrap_or_else(|| TextRange::new(0, program.source_len));
  Span::new(program.source_file, range)
}

fn diag(program: &Program, inst: &Inst, code: &'static str, message: impl Into<String>) -> Diagnostic {
  Diagnostic::error(code, message, span_for_inst(program, inst))
}

fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
  diagnostics.sort_by(|a, b| {
    a.primary
      .file
      .cmp(&b.primary.file)
      .then(a.primary.range.start.cmp(&b.primary.range.start))
      .then(a.primary.range.end.cmp(&b.primary.range.end))
      .then(a.code.cmp(&b.code))
      .then(a.message.cmp(&b.message))
  });
}

#[derive(Clone, Debug, Default)]
struct SymbolScopes {
  sym_to_scope: HashMap<SymbolId, ScopeId>,
  scope_to_kind: HashMap<ScopeId, ProgramScopeKind>,
  sym_to_name: HashMap<SymbolId, String>,
}

impl SymbolScopes {
  fn from_program(program: &Program) -> Self {
    let mut out = Self::default();
    let Some(symbols) = program.symbols.as_ref() else {
      return out;
    };

    for scope in symbols.scopes.iter() {
      out.scope_to_kind.insert(scope.id, scope.kind.clone());
    }
    for sym in symbols.symbols.iter() {
      out.sym_to_scope.insert(sym.id, sym.scope);
      out.sym_to_name.insert(sym.id, sym.name.clone());
    }
    out
  }

  fn scope_kind_for(&self, sym: SymbolId) -> Option<ProgramScopeKind> {
    let scope = *self.sym_to_scope.get(&sym)?;
    self.scope_to_kind.get(&scope).cloned()
  }

  fn name_for(&self, sym: SymbolId) -> Option<&str> {
    self.sym_to_name.get(&sym).map(|s| s.as_str())
  }
}

fn validate_cfg(program: &Program, cfg: &Cfg, scopes: &SymbolScopes, opts: StrictNativeOpts) -> Vec<Diagnostic> {
  let vars = compute_value_states(cfg);
  let labels = cfg.graph.labels_sorted();
  let mut diagnostics = Vec::new();

  #[cfg(feature = "typed")]
  let var_layouts = compute_var_layouts(cfg);

  for label in labels {
    for inst in cfg.bblocks.get(label).iter() {
      match inst.t {
        InstTyp::UnknownLoad | InstTyp::UnknownStore if !opts.allow_unknown_memory => {
          let name = inst.unknown.as_str();
          diagnostics.push(diag(
            program,
            inst,
            CODE_UNKNOWN_MEMORY,
            format!("strict-native forbids unknown identifier access `{name}`"),
          ));
        }
        InstTyp::ForeignLoad | InstTyp::ForeignStore => {
          let sym = inst.foreign;
          match scopes.scope_kind_for(sym) {
            Some(ProgramScopeKind::Global) if !opts.allow_foreign_global => {
              let name = scopes.name_for(sym).unwrap_or("<unknown>");
              diagnostics.push(diag(
                program,
                inst,
                CODE_FOREIGN_GLOBAL,
                format!("strict-native forbids global variable access `{name}`"),
              ));
            }
            None if !opts.allow_foreign_unknown => {
              diagnostics.push(diag(
                program,
                inst,
                CODE_FOREIGN_GLOBAL,
                format!("strict-native forbids foreign variable access (symbol {})", sym.raw_id()),
              ));
            }
            _ => {}
          }
        }
        InstTyp::Call => {
          if !opts.allow_spread_calls && !inst.spreads.is_empty() {
            diagnostics.push(diag(
              program,
              inst,
              CODE_SPREAD_CALL,
              "strict-native forbids spread arguments in calls",
            ));
          }

          if let Some(path) = resolve_builtin_path(&inst.args[0], &vars) {
            if path == "__optimize_js_new" {
              if let Some(ctor) = resolve_builtin_path(&inst.args[1], &vars) {
                if is_banned_constructor(&ctor) {
                  diagnostics.push(diag(
                    program,
                    inst,
                    CODE_BANNED_BUILTIN,
                    format!("strict-native forbids constructing `{ctor}`"),
                  ));
                }
              }
            } else if is_banned_builtin_call(&path) {
              diagnostics.push(diag(
                program,
                inst,
                CODE_BANNED_BUILTIN,
                format!("strict-native forbids calling `{path}`"),
              ));
            }
          }
        }
        #[cfg(feature = "semantic-ops")]
        InstTyp::KnownApiCall { api } => {
          if !opts.allow_spread_calls && !inst.spreads.is_empty() {
            diagnostics.push(diag(
              program,
              inst,
              CODE_SPREAD_CALL,
              "strict-native forbids spread arguments in calls",
            ));
          }

          if let Some(name) = banned_known_api_call(api) {
            diagnostics.push(diag(
              program,
              inst,
              CODE_BANNED_BUILTIN,
              format!("strict-native forbids calling `{name}`"),
            ));
          }
        }
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let prop = &inst.args[1];
          if !is_static_prop_key(prop, &vars) {
            diagnostics.push(diag(
              program,
              inst,
              CODE_DYNAMIC_PROP,
              "strict-native forbids dynamic property access",
            ));
          }
        }
        InstTyp::PropAssign => {
          let prop = &inst.args[1];
          if !is_static_prop_key(prop, &vars) {
            diagnostics.push(diag(
              program,
              inst,
              CODE_DYNAMIC_PROP,
              "strict-native forbids dynamic property assignment",
            ));
          } else if resolves_to_proto_key(prop, &vars) {
            diagnostics.push(diag(
              program,
              inst,
              CODE_BANNED_BUILTIN,
              "strict-native forbids `__proto__` writes",
            ));
          }
        }
        _ => {}
      }

      #[cfg(feature = "typed")]
      {
        if opts.require_type_ids && inst.tgts.get(0).is_some() {
          if inst.meta.hir_expr.is_some() && inst.meta.type_id.is_none() {
            diagnostics.push(diag(
              program,
              inst,
              CODE_MISSING_TYPE_ID,
              "missing type metadata for value instruction",
            ));
          } else if inst.meta.native_layout.is_none() {
            diagnostics.push(diag(
              program,
              inst,
              CODE_MISSING_TYPE_ID,
              "missing native layout metadata for value instruction",
            ));
          }
        }

        if opts.require_type_ids {
          match inst.t {
            InstTyp::VarAssign => {
              let (Some(tgt_layout), Some(Arg::Var(src))) = (inst.meta.native_layout, inst.args.get(0)) else {
                continue;
              };
              let Some(src_layout) = var_layouts.get(src) else {
                continue;
              };
              if *src_layout != tgt_layout {
                diagnostics.push(diag(
                  program,
                  inst,
                  CODE_MISSING_TYPE_ID,
                  "native layout mismatch in VarAssign",
                ));
              }
            }
            InstTyp::Phi => {
              let Some(phi_layout) = inst.meta.native_layout else {
                continue;
              };
              for arg in &inst.args {
                let Arg::Var(v) = arg else {
                  continue;
                };
                let Some(arg_layout) = var_layouts.get(v) else {
                  continue;
                };
                if *arg_layout != phi_layout {
                  diagnostics.push(diag(
                    program,
                    inst,
                    CODE_MISSING_TYPE_ID,
                    "native layout mismatch in Phi",
                  ));
                  break;
                }
              }
            }
            _ => {}
          }
        }
      }
    }
  }

  diagnostics
}

/// Validate a fully compiled [`Program`] against the strict-native IL subset.
///
/// Returns `Ok(())` when all checks pass; otherwise returns a stable, sorted
/// list of diagnostics.
pub fn validate_program(program: &Program, opts: StrictNativeOpts) -> Result<(), Vec<Diagnostic>> {
  let scopes = SymbolScopes::from_program(program);
  let mut diagnostics = Vec::new();

  diagnostics.extend(validate_cfg(
    program,
    program.top_level.analyzed_cfg(),
    &scopes,
    opts,
  ));
  for func in program.functions.iter() {
    diagnostics.extend(validate_cfg(program, func.analyzed_cfg(), &scopes, opts));
  }

  sort_diagnostics(&mut diagnostics);
  if diagnostics.is_empty() {
    Ok(())
  } else {
    Err(diagnostics)
  }
}
