use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::{Arg, BinOp, Inst, InstTyp};
use crate::{Program, ProgramFunction};
use diagnostics::{Diagnostic, Diagnostics, FileId, Span, TextRange};
use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, Debug)]
pub struct VerifyOptions {
  /// File id used for source spans in emitted diagnostics.
  pub file: FileId,
  /// Allow `InstTyp::UnknownLoad` / `InstTyp::UnknownStore`.
  pub allow_unknown_memory: bool,
  /// Allow `GetProp`/`PropAssign` operations with non-constant keys.
  pub allow_dynamic_getprop: bool,
  /// Allow call spreads (`...args`) in `InstTyp::Call`.
  pub allow_call_spreads: bool,
  /// Require typed metadata on value-producing instructions.
  ///
  /// Defaults to `true` in typed builds and `false` otherwise.
  pub require_type_metadata: bool,
}

impl Default for VerifyOptions {
  fn default() -> Self {
    Self {
      file: FileId(0),
      allow_unknown_memory: false,
      allow_dynamic_getprop: false,
      allow_call_spreads: false,
      require_type_metadata: cfg!(feature = "typed"),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum FunctionId {
  TopLevel,
  Fn(usize),
}

impl fmt::Display for FunctionId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::TopLevel => f.write_str("top_level"),
      Self::Fn(id) => write!(f, "fn:{id}"),
    }
  }
}

#[derive(Clone, Copy, Debug)]
struct InstLoc {
  fn_id: FunctionId,
  block: u32,
  inst_idx: usize,
}

fn inst_loc_suffix(loc: InstLoc) -> String {
  format!("fn={} block={} inst={}", loc.fn_id, loc.block, loc.inst_idx)
}

fn diagnostic_for_inst(
  code: &'static str,
  message: String,
  opts: &VerifyOptions,
  inst: &Inst,
  loc: InstLoc,
) -> Diagnostic {
  let primary_span = Span::new(
    opts.file,
    inst
      .meta
      .span
      .unwrap_or_else(|| TextRange::new(0, 0)),
  );

  let mut out = Diagnostic::error(code, message, primary_span);
  if let Some(range) = inst.meta.span {
    out.push_note(format!("span: {}..{}", range.start, range.end));
  } else {
    out.push_note("span: <unavailable>");
  }
  out.push_note(inst_loc_suffix(loc));
  out
}

fn is_static_prop_key(key: &Arg) -> bool {
  match key {
    // JS property keys can be strings/numbers/bigints/booleans/etc (they will be coerced). For
    // strict-native lowering we only care that the key is compile-time constant.
    Arg::Const(_) => true,
    // Some built-in "keys" (e.g. `Symbol.iterator`) are represented as builtins so we can avoid
    // materializing the symbol value via a dynamic lookup.
    Arg::Builtin(name) => name.starts_with("Symbol."),
    _ => false,
  }
}

fn is_forbidden_marker_builtin(name: &str) -> bool {
  // When structured IL instructions exist for a feature, the marker builtin must not appear in
  // strict-native mode.
  #[cfg(feature = "native-async-ops")]
  if name == "__optimize_js_await" {
    return true;
  }

  #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
  if name == "__optimize_js_array_chain" {
    return true;
  }

  // Typed lowering uses `InstTyp::StringConcat` for template literals. The legacy lowering uses
  // `Call(__optimize_js_template, ...)`.
  #[cfg(feature = "typed")]
  if name == "__optimize_js_template" {
    return true;
  }

  let _ = name;
  false
}

fn cfg_for_function(function: &ProgramFunction) -> &Cfg {
  function.cfg_ssa().unwrap_or(&function.body)
}

fn verify_function(program: &Program, fn_id: FunctionId, function: &ProgramFunction, opts: &VerifyOptions, diagnostics: &mut Diagnostics) {
  let _ = program;
  let cfg = cfg_for_function(function);
  let dom = Dom::<false>::calculate(cfg);
  let dom_by = dom.dominated_by_graph();

  let labels = cfg.graph.labels_sorted();

  #[derive(Clone, Copy, Debug)]
  struct DefLoc {
    block: u32,
    inst_idx: usize,
  }

  let mut defs: HashMap<u32, DefLoc> = HashMap::new();
  for &block in &labels {
    let insts = cfg.bblocks.get(block);
    for (inst_idx, inst) in insts.iter().enumerate() {
      for &tgt in inst.tgts.iter() {
        // SSA sanity: each SSA variable should have exactly one defining instruction.
        if defs.insert(tgt, DefLoc { block, inst_idx }).is_some() {
          diagnostics.push(diagnostic_for_inst(
            "OPTN0007",
            format!(
              "strict-native: SSA variable %{tgt} is defined multiple times ({})",
              inst_loc_suffix(InstLoc {
                fn_id,
                block,
                inst_idx,
              })
            ),
            opts,
            inst,
            InstLoc {
              fn_id,
              block,
              inst_idx,
            },
          ));
        }
      }
    }
  }

  for &block in &labels {
    let insts = cfg.bblocks.get(block);
    let parents = cfg.graph.parents_sorted(block);
    let mut seen_non_phi = false;

    for (inst_idx, inst) in insts.iter().enumerate() {
      let loc = InstLoc {
        fn_id,
        block,
        inst_idx,
      };

      match inst.t {
        InstTyp::Phi => {
          if seen_non_phi {
            diagnostics.push(diagnostic_for_inst(
              "OPTN0007",
              format!(
                "strict-native: Phi must appear before non-Phi instructions ({})",
                inst_loc_suffix(loc)
              ),
              opts,
              inst,
              loc,
            ));
          }

          if inst.labels.len() != inst.args.len() {
            diagnostics.push(diagnostic_for_inst(
              "OPTN0007",
              format!(
                "strict-native: Phi labels/args length mismatch (labels={}, args={}) ({})",
                inst.labels.len(),
                inst.args.len(),
                inst_loc_suffix(loc)
              ),
              opts,
              inst,
              loc,
            ));
          }

          if inst.labels.len() != parents.len() {
            diagnostics.push(diagnostic_for_inst(
              "OPTN0007",
              format!(
                "strict-native: Phi must have one incoming value per predecessor (preds={}, labels={}) ({})",
                parents.len(),
                inst.labels.len(),
                inst_loc_suffix(loc)
              ),
              opts,
              inst,
              loc,
            ));
          } else {
            // Ensure the Phi maps exactly one incoming value per predecessor block.
            //
            // Note: `Inst::insert_phi` rejects duplicate labels (in debug builds), but we still
            // validate here because `optimize-js` can be built without debug assertions and because
            // mismatches can lead to silent miscompiles in SSA-based consumers.
            let mut phi_labels = inst.labels.clone();
            phi_labels.sort_unstable();
            let mut preds = parents.clone();
            preds.sort_unstable();
            if phi_labels != preds {
              diagnostics.push(diagnostic_for_inst(
                "OPTN0007",
                format!(
                  "strict-native: Phi incoming labels do not match CFG predecessors (preds={preds:?}, labels={phi_labels:?}) ({})",
                  inst_loc_suffix(loc)
                ),
                opts,
                inst,
                loc,
              ));
            }
          }

          // Dominance for phi arguments is on the predecessor edge.
          for (&pred, arg) in inst.labels.iter().zip(inst.args.iter()) {
            let Arg::Var(v) = arg else {
              continue;
            };
            let def_block = defs.get(v).map(|def| def.block).unwrap_or(cfg.entry);
            if !dom_by.dominated_by(pred, def_block) {
              diagnostics.push(diagnostic_for_inst(
                "OPTN0007",
                format!(
                  "strict-native: %{v} does not dominate Phi incoming edge from {pred} ({})",
                  inst_loc_suffix(loc)
                ),
                opts,
                inst,
                loc,
              ));
            }
          }
        }
        _ => {
          seen_non_phi = true;
        }
      }

      // 1) Strict native instruction set checks.
      match inst.t {
        InstTyp::UnknownLoad | InstTyp::UnknownStore => {
          if !opts.allow_unknown_memory {
            diagnostics.push(diagnostic_for_inst(
              "OPTN0001",
              format!(
                "strict-native: {} is forbidden ({})",
                match inst.t {
                  InstTyp::UnknownLoad => "UnknownLoad",
                  InstTyp::UnknownStore => "UnknownStore",
                  _ => "<unknown>",
                },
                inst_loc_suffix(loc)
              ),
              opts,
              inst,
              loc,
            ));
          }
        }
        _ => {}
      }

      // 2) Property access restrictions.
      if inst.t == InstTyp::Bin && inst.bin_op == BinOp::GetProp {
        if let Some(key) = inst.args.get(1) {
          if !opts.allow_dynamic_getprop && !is_static_prop_key(key) {
            diagnostics.push(diagnostic_for_inst(
              "OPTN0004",
              format!(
                "strict-native: dynamic GetProp key is forbidden ({})",
                inst_loc_suffix(loc)
              ),
              opts,
              inst,
              loc,
            ));
          }
        }
      }
      if inst.t == InstTyp::PropAssign {
        if let Some(key) = inst.args.get(1) {
          if !opts.allow_dynamic_getprop && !is_static_prop_key(key) {
            diagnostics.push(diagnostic_for_inst(
              "OPTN0004",
              format!(
                "strict-native: dynamic PropAssign key is forbidden ({})",
                inst_loc_suffix(loc)
              ),
              opts,
              inst,
              loc,
            ));
          }
        }
      }

      // 3) Call spread restriction.
      if inst.t == InstTyp::Call && !opts.allow_call_spreads && !inst.spreads.is_empty() {
        diagnostics.push(diagnostic_for_inst(
          "OPTN0003",
          format!(
            "strict-native: call spread is forbidden (spreads={:?}) ({})",
            inst.spreads,
            inst_loc_suffix(loc)
          ),
          opts,
          inst,
          loc,
        ));
      }

      // 4) Marker builtin restriction (feature-gated).
      for arg in inst.args.iter() {
        let Arg::Builtin(name) = arg else {
          continue;
        };
        if is_forbidden_marker_builtin(name) {
          diagnostics.push(diagnostic_for_inst(
            "OPTN0005",
            format!(
              "strict-native: marker builtin `{name}` is forbidden ({})",
              inst_loc_suffix(loc)
            ),
            opts,
            inst,
            loc,
          ));
        }
      }

      // 5) Typed invariants for SSA values.
      if opts.require_type_metadata && !inst.tgts.is_empty() {
        if inst.meta.type_id.is_none() {
          diagnostics.push(diagnostic_for_inst(
            "OPTN0006",
            format!(
              "strict-native: value-producing instruction missing InstMeta.type_id ({})",
              inst_loc_suffix(loc)
            ),
            opts,
            inst,
            loc,
          ));
        }
        #[cfg(feature = "typed")]
        if inst.meta.type_id.is_some() && inst.meta.native_layout.is_none() {
          diagnostics.push(diagnostic_for_inst(
            "OPTN0006",
            format!(
              "strict-native: value-producing instruction missing InstMeta.native_layout ({})",
              inst_loc_suffix(loc)
            ),
            opts,
            inst,
            loc,
          ));
        }
      }

      // 6) Dominance sanity for non-Phi uses.
      if inst.t != InstTyp::Phi {
        for arg in inst.args.iter() {
          let Arg::Var(v) = arg else {
            continue;
          };
          let def = defs.get(v).copied();
          let def_block = def.map(|def| def.block).unwrap_or(cfg.entry);

          if !dom_by.dominated_by(block, def_block) {
            diagnostics.push(diagnostic_for_inst(
              "OPTN0007",
              format!(
                "strict-native: %{v} does not dominate its use ({})",
                inst_loc_suffix(loc)
              ),
              opts,
              inst,
              loc,
            ));
            continue;
          }

          if let Some(def) = def {
            if def.block == block && def.inst_idx >= inst_idx {
              diagnostics.push(diagnostic_for_inst(
                "OPTN0007",
                format!(
                  "strict-native: %{v} is used before its definition in block {block} ({})",
                  inst_loc_suffix(loc)
                ),
                opts,
                inst,
                loc,
              ));
            }
          }
        }
      }
    }
  }
}

/// Strict verifier for the subset of `optimize-js` IL accepted by native backends.
///
/// This is intended to be run on "native-ready" SSA CFGs (i.e. `Cfg` values that may still contain
/// `InstTyp::Phi`). The verifier is conservative: it emits *actionable* diagnostics rather than
/// silently accepting IL patterns that native backends do not implement.
pub fn verify_program_strict_native(program: &Program, opts: &VerifyOptions) -> Result<(), Diagnostics> {
  let mut diagnostics: Diagnostics = Vec::new();

  verify_function(program, FunctionId::TopLevel, &program.top_level, opts, &mut diagnostics);

  for (id, func) in program.functions.iter().enumerate() {
    verify_function(program, FunctionId::Fn(id), func, opts, &mut diagnostics);
  }

  if diagnostics.is_empty() {
    Ok(())
  } else {
    diagnostics::sort_diagnostics(&mut diagnostics);
    Err(diagnostics)
  }
}
