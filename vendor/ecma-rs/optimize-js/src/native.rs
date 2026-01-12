//! Typed "native-backend ready" compilation API.
//!
//! This module exposes a single entry point that:
//! - reuses `typecheck-ts` cached lowering when available,
//! - forces SSA retention in the returned CFGs,
//! - runs the full analysis driver to populate [`crate::il::inst::InstMeta`],
//! - returns both the annotated IR and side-table summaries used by native backends.

use crate::analysis;
use crate::strict_native;
use crate::types;
use crate::{
  verify_program_strict_native, CompileCfgOptions, Diagnostic, OptimizeResult, Program, Span,
  TextRange, TopLevelMode, VerifyOptions,
};
use hir_js::FileKind as HirFileKind;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct NativeReadyOptions {
  /// Run optimization passes after SSA construction (enabled by default).
  pub run_opt_passes: bool,
  /// Validate the generated IL against the strict-native subset required by native backends.
  ///
  /// Enabled by default.
  pub verify_strict_native: bool,
  /// Options for the strict-native IL verifier.
  pub strict_native_opts: strict_native::StrictNativeOpts,
}

impl Default for NativeReadyOptions {
  fn default() -> Self {
    Self {
      run_opt_passes: true,
      verify_strict_native: true,
      strict_native_opts: strict_native::StrictNativeOpts::default(),
    }
  }
}

/// Result of compiling a single file into a form convenient for native codegen.
#[derive(Debug)]
pub struct NativeReadyProgram {
  pub program: Program,
  pub analyses: analysis::driver::ProgramAnalyses,
  pub types: types::TypeContext,
}

/// Compile a file from a `typecheck-ts` program into a "native backend ready" artifact.
///
/// This is only available when `optimize-js` is built with `feature = "typed"`.
pub fn compile_file_native_ready(
  type_program: Arc<typecheck_ts::Program>,
  file: typecheck_ts::FileId,
  mode: TopLevelMode,
  debug: bool,
  opts: NativeReadyOptions,
) -> OptimizeResult<NativeReadyProgram> {
  let cfg_options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: opts.run_opt_passes,
    ..Default::default()
  };

  let source = type_program.file_text(file).ok_or_else(|| {
    vec![Diagnostic::error(
      "OPT0003",
      format!("missing source text for {file:?}"),
      Span::new(file, TextRange::new(0, 0)),
    )]
  })?;

  let top_level_node = crate::parse_source(&source, file, mode)?;

  let (mut program, types) = if let Some(lowered) = type_program.hir_lowered(file) {
    let types = Arc::new(types::TypeContext::from_typecheck_program_aligned(
      Arc::clone(&type_program),
      file,
      lowered.as_ref(),
    ));
    let program = Program::compile_with_lower(
      top_level_node,
      lowered,
      mode,
      debug,
      Arc::clone(&types),
      cfg_options,
    )?;
    let types = Arc::try_unwrap(types).expect("TypeContext should have a single strong ref");
    (program, types)
  } else {
    let lower = hir_js::lower_file(file, HirFileKind::Ts, &top_level_node);
    let types = Arc::new(types::TypeContext::from_typecheck_program(
      Arc::clone(&type_program),
      file,
      &lower,
    ));
    let program = Program::compile_with_lower(
      top_level_node,
      Arc::new(lower),
      mode,
      debug,
      Arc::clone(&types),
      cfg_options,
    )?;
    let types = Arc::try_unwrap(types).expect("TypeContext should have a single strong ref");
    (program, types)
  };

  // Populate per-instruction metadata and return side-table analyses for native backends.
  let analyses = analysis::driver::annotate_program_typed(&mut program, &types);

  if opts.verify_strict_native {
    strict_native::validate_program(&program, opts.strict_native_opts)?;
    let verify_opts = VerifyOptions {
      file,
      // Property access restrictions are handled by `strict_native::validate_program` using
      // SSA-aware constant propagation. The standalone verifier's GetProp/PropAssign check is more
      // naive, so disable it here to avoid false positives.
      allow_dynamic_getprop: true,
      allow_call_spreads: opts.strict_native_opts.allow_spread_calls,
      allow_unknown_memory: opts.strict_native_opts.allow_unknown_memory,
      // Typed metadata requirements are handled by `strict_native::validate_program`.
      require_type_metadata: false,
      ..Default::default()
    };
    verify_program_strict_native(&program, &verify_opts).map_err(|diags| diags)?;
  }

  Ok(NativeReadyProgram {
    program,
    analyses,
    types,
  })
}

/// Like [`compile_file_native_ready`], but accepts a `&typecheck_ts::Program` instead of an
/// `Arc<typecheck_ts::Program>`.
///
/// This exists for downstream consumers (e.g. `native-js`) whose public APIs already take a shared
/// reference to a typechecked program. Those APIs cannot always produce an `Arc` without
/// re-typechecking or cloning the underlying program state.
///
/// In this mode we still populate `InstMeta.type_id` during lowering (via per-body expression type
/// tables), then backfill `InstMeta.native_layout` after IL construction using
/// `Program::layout_of_interned`.
pub fn compile_file_native_ready_programless(
  type_program: &typecheck_ts::Program,
  file: typecheck_ts::FileId,
  mode: TopLevelMode,
  debug: bool,
  opts: NativeReadyOptions,
) -> OptimizeResult<NativeReadyProgram> {
  let cfg_options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: opts.run_opt_passes,
    ..Default::default()
  };

  let source = type_program.file_text(file).ok_or_else(|| {
    vec![Diagnostic::error(
      "OPT0003",
      format!("missing source text for {file:?}"),
      Span::new(file, TextRange::new(0, 0)),
    )]
  })?;

  let top_level_node = crate::parse_source(&source, file, mode)?;

  let (mut program, types) = if let Some(lowered) = type_program.hir_lowered(file) {
    let types = Arc::new(types::TypeContext::from_typecheck_program_aligned_programless(
      type_program,
      file,
      lowered.as_ref(),
    ));
    let program = Program::compile_with_lower(
      top_level_node,
      lowered,
      mode,
      debug,
      Arc::clone(&types),
      cfg_options,
    )?;
    let types = Arc::try_unwrap(types).expect("TypeContext should have a single strong ref");
    (program, types)
  } else {
    let lower = hir_js::lower_file(file, HirFileKind::Ts, &top_level_node);
    let types = Arc::new(types::TypeContext::from_typecheck_program_programless(
      type_program,
      file,
      &lower,
    ));
    let program = Program::compile_with_lower(
      top_level_node,
      Arc::new(lower),
      mode,
      debug,
      Arc::clone(&types),
      cfg_options,
    )?;
    let types = Arc::try_unwrap(types).expect("TypeContext should have a single strong ref");
    (program, types)
  };

  // Backfill `InstMeta.native_layout` using the typecheck-ts program. Native backends rely on this
  // to map values to concrete runtime layouts.
  fn fill_native_layouts_in_cfg(
    cfg: &mut crate::cfg::cfg::Cfg,
    type_program: &typecheck_ts::Program,
  ) {
    for (_label, insts) in cfg.bblocks.all_mut() {
      for inst in insts.iter_mut() {
        // Only fill when we have a TypeId. Keep any existing layout if a future pipeline chooses
        // to populate it earlier.
        if inst.meta.native_layout.is_none() {
          inst.meta.native_layout = inst
            .meta
            .type_id
            .map(|ty| type_program.layout_of_interned(ty));
        }
      }
    }
  }

  // Top level.
  fill_native_layouts_in_cfg(&mut program.top_level.body, type_program);
  if let Some(ssa) = program.top_level.ssa_body.as_mut() {
    fill_native_layouts_in_cfg(ssa, type_program);
  }
  // Nested functions.
  for f in program.functions.iter_mut() {
    fill_native_layouts_in_cfg(&mut f.body, type_program);
    if let Some(ssa) = f.ssa_body.as_mut() {
      fill_native_layouts_in_cfg(ssa, type_program);
    }
  }

  // Populate per-instruction metadata and return side-table analyses for native backends.
  let analyses = analysis::driver::annotate_program_with_typecheck(&mut program, type_program);

  if opts.verify_strict_native {
    strict_native::validate_program(&program, opts.strict_native_opts)?;
    let verify_opts = VerifyOptions {
      file,
      // Property access restrictions are handled by `strict_native::validate_program` using
      // SSA-aware constant propagation. The standalone verifier's GetProp/PropAssign check is more
      // naive, so disable it here to avoid false positives.
      allow_dynamic_getprop: true,
      allow_call_spreads: opts.strict_native_opts.allow_spread_calls,
      allow_unknown_memory: opts.strict_native_opts.allow_unknown_memory,
      // Typed metadata requirements are handled by `strict_native::validate_program`.
      require_type_metadata: false,
      ..Default::default()
    };
    verify_program_strict_native(&program, &verify_opts).map_err(|diags| diags)?;
  }

  Ok(NativeReadyProgram {
    program,
    analyses,
    types,
  })
}
