//! Typed "native-backend ready" compilation API.
//!
//! This module exposes a single entry point that:
//! - reuses `typecheck-ts` cached lowering when available,
//! - forces SSA retention in the returned CFGs,
//! - runs the full analysis driver to populate [`crate::il::inst::InstMeta`],
//! - returns both the annotated IR and side-table summaries used by native backends.

use crate::analysis;
use crate::types;
use crate::{CompileCfgOptions, Diagnostic, OptimizeResult, Program, Span, TextRange, TopLevelMode};
use hir_js::FileKind as HirFileKind;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct NativeReadyOptions {
  /// Run optimization passes after SSA construction (enabled by default).
  pub run_opt_passes: bool,
}

impl Default for NativeReadyOptions {
  fn default() -> Self {
    Self { run_opt_passes: true }
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
  program: Arc<typecheck_ts::Program>,
  file: typecheck_ts::FileId,
  mode: TopLevelMode,
  debug: bool,
  opts: NativeReadyOptions,
) -> OptimizeResult<NativeReadyProgram> {
  let cfg_options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: opts.run_opt_passes,
  };

  let source = program.file_text(file).ok_or_else(|| {
    vec![Diagnostic::error(
      "OPT0003",
      format!("missing source text for {file:?}"),
      Span::new(file, TextRange::new(0, 0)),
    )]
  })?;

  let top_level_node = crate::parse_source(&source, file, mode)?;

  let (mut program, types) = if let Some(lowered) = program.hir_lowered(file) {
    let types = Arc::new(types::TypeContext::from_typecheck_program_aligned(
      Arc::clone(&program),
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
      Arc::clone(&program),
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

  Ok(NativeReadyProgram {
    program,
    analyses,
    types,
  })
}
