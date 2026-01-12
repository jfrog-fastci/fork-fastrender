//! SSA/analysis-driven native backend powered by `optimize-js` IL.
//!
//! This backend is an MVP bridge between:
//! - `typecheck-ts` (strict subset validation + entrypoint discovery)
//! - `optimize-js` (SSA-form CFG + program analyses)
//! - `native-js` (LLVM/statepoint emission + runtime linking)
//!
//! The current implementation intentionally supports only a small, well-defined IL subset:
//! - numbers/booleans as `i32` values (booleans are `0/1`)
//! - arithmetic + comparisons
//! - if/else + while (via SSA CFG lowering)
//! - local variables (SSA temps + phi)
//! - direct calls to known functions (`Arg::Fn` or `ForeignLoad` of a hoisted function decl)
//! - return
//!
//! Unsupported IL instructions fail with stable `NJS01xx` diagnostics.

use crate::codes;
use crate::strict::Entrypoint;
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{DefKind, FileKind, StmtKind};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::debug_info::{
  AsDIScope, DICompileUnit, DIFile, DILocation, DISubprogram, DIType, DWARFEmissionKind, DWARFSourceLanguage,
  DebugInfoBuilder,
};
use inkwell::module::Linkage;
use inkwell::module::Module;
use inkwell::types::IntType;
use inkwell::values::{FunctionValue, IntValue, PhiValue};
use inkwell::IntPredicate;
use optimize_js::analysis::ProgramAnalyses;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::cfg::cfg::Terminator;
use optimize_js::il::inst::{Arg, BinOp, Const, Inst, InstTyp, UnOp};
use optimize_js::symbol::semantics::SymbolId;
use optimize_js::{CompileCfgOptions, Program as OptProgram, TopLevelMode};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use typecheck_ts::{DefId, FileId, Program};

/// Options controlling SSA backend codegen.
#[derive(Clone, Debug)]
pub struct CodegenOptions {
  pub module_name: String,
}

impl Default for CodegenOptions {
  fn default() -> Self {
    Self {
      module_name: "native_js".to_string(),
    }
  }
}

pub fn codegen<'ctx>(
  context: &'ctx Context,
  program: &Program,
  entry_file: FileId,
  entrypoint: Entrypoint,
  options: CodegenOptions,
  debug: bool,
) -> Result<Module<'ctx>, Vec<Diagnostic>> {
  // MVP limitation: this backend currently only compiles the entry file as a single module.
  if entrypoint.main_def.file() != entry_file {
    return Err(vec![Diagnostic::error(
      "NJS0150",
      "SSA backend currently requires `main` to be defined in the entry file (re-exports/imports not supported yet)",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]);
  }

  let source = program.file_text(entry_file).ok_or_else(|| {
    vec![Diagnostic::error(
      "NJS0150",
      "missing source text for entry file",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]
  })?;
  let lowered = program.hir_lowered(entry_file).ok_or_else(|| {
    vec![codes::MISSING_ENTRY_HIR.error(
      "failed to access lowered HIR for entry file",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]
  })?;
  if matches!(lowered.hir.file_kind, FileKind::Dts) {
    return Err(vec![Diagnostic::error(
      "NJS0150",
      "entry file must not be a declaration file",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]);
  }

  let mut opt_program = OptProgram::compile_lowered_with_cfg_options(
    source.as_ref(),
    lowered.as_ref().clone(),
    TopLevelMode::Module,
    debug,
    CompileCfgOptions {
      keep_ssa: true,
      // `optimize-js`'s optimisation passes are not yet guaranteed to preserve SSA form when
      // `keep_ssa=true` (some passes are written assuming SSA will be deconstructed afterwards).
      // For the native backend MVP we prefer correctness/stability and run downstream LLVM
      // optimisation pipelines instead.
      run_opt_passes: false,
      ..Default::default()
    },
  )?;
  let analyses = optimize_js::analysis::annotate_program(&mut opt_program);

  // Map optimize-js FnId ordering to `hir-js`/`typecheck-ts` DefIds by replicating
  // `optimize-js`'s `hoist_function_decls()` ordering.
  let fn_defs = collect_hoisted_function_defs(lowered.as_ref())?;
  if fn_defs.len() != opt_program.functions.len() {
    return Err(vec![Diagnostic::error(
      "NJS0152",
      format!(
        "SSA backend expected {} hoisted functions but optimize-js produced {} (nested functions likely not supported)",
        fn_defs.len(),
        opt_program.functions.len()
      ),
      Span::new(entry_file, TextRange::new(0, 0)),
    )]);
  }

  let main_fnid = fn_defs
    .iter()
    .position(|def| *def == entrypoint.main_def)
    .ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0152",
        "failed to map exported `main` to an optimize-js function id",
        Span::new(entry_file, TextRange::new(0, 0)),
      )]
    })?;

  let mut cg = ProgramCodegen::new(
    context,
    program,
    entry_file,
    source.clone(),
    &opt_program,
    analyses,
    fn_defs,
    &options.module_name,
    debug,
  )?;
  cg.codegen_all_functions()?;
  cg.build_c_main(main_fnid);
  Ok(cg.finish())
}

fn collect_hoisted_function_defs(lowered: &hir_js::LowerResult) -> Result<Vec<DefId>, Vec<Diagnostic>> {
  let root = lowered
    .body(lowered.root_body())
    .ok_or_else(|| vec![Diagnostic::error(
      "NJS0150",
      "failed to access lowered root body",
      Span::new(lowered.hir.file, TextRange::new(0, 0)),
    )])?;

  let mut decls = Vec::new();
  for stmt in root.stmts.iter() {
    if let StmtKind::Decl(def_id) = stmt.kind {
      decls.push((stmt.span.start, stmt.span.end, def_id));
    }
  }
  decls.sort_by_key(|(start, end, def_id)| (*start, *end, *def_id));

  let mut out = Vec::new();
  for (_, _, def_id) in decls {
    let Some(def) = lowered.def(def_id) else {
      continue;
    };
    if def.path.kind != DefKind::Function {
      continue;
    }
    // Skip overloads/ambient declarations (no runtime body), matching optimize-js.
    if def.body.is_none() {
      continue;
    }
    out.push(def_id);
  }
  Ok(out)
}

#[derive(Clone)]
struct LineIndex {
  text: Arc<str>,
  /// 0-based byte offsets of the start of each line.
  line_starts: Vec<usize>,
}

impl LineIndex {
  fn new(text: Arc<str>) -> Self {
    let mut line_starts = vec![0usize];
    for (idx, b) in text.as_bytes().iter().enumerate() {
      if *b == b'\n' {
        line_starts.push(idx + 1);
      }
    }
    Self { text, line_starts }
  }

  fn clamp_offset_to_char_boundary(&self, mut offset: usize) -> usize {
    if offset > self.text.len() {
      offset = self.text.len();
    }
    while offset > 0 && !self.text.is_char_boundary(offset) {
      offset -= 1;
    }
    offset
  }

  fn line_col(&self, offset: u32) -> (u32, u32) {
    let offset = self.clamp_offset_to_char_boundary(offset as usize);

    // Find the last line start that is <= offset.
    let line_idx = match self.line_starts.binary_search(&offset) {
      Ok(idx) => idx.min(self.line_starts.len().saturating_sub(1)),
      Err(0) => 0,
      Err(idx) => idx - 1,
    };
    let line_start = *self.line_starts.get(line_idx).unwrap_or(&0);

    // DWARF columns are 1-based UTF-8 byte offsets within the line (0 means unknown).
    let line = (line_idx + 1) as u32;
    let col = offset.saturating_sub(line_start).saturating_add(1) as u32;
    (line, col)
  }
}

struct SsaDebug<'ctx> {
  builder: DebugInfoBuilder<'ctx>,
  #[allow(dead_code)]
  compile_unit: DICompileUnit<'ctx>,
  file: DIFile<'ctx>,
  i32_ty: DIType<'ctx>,
  line_index: LineIndex,
}

fn split_file_key(file_key: &str) -> (String, Option<String>) {
  // `Program::file_key` is usually a filesystem path (often a canonical absolute path) but it may
  // also be a bare filename ("main.ts") in tests or synthetic hosts.
  //
  // LLVM DIFile expects filename and directory as separate strings. Passing full paths in the
  // filename field (while leaving directory as ".") produces odd DWARF that debuggers often render
  // poorly. Split on common path separators when present.
  let last_sep = file_key.rfind(|c| c == '/' || c == '\\');
  let Some(last_sep) = last_sep else {
    return (file_key.to_string(), None);
  };

  // If the string ends with a separator, treat it as not path-like.
  if last_sep + 1 >= file_key.len() {
    return (file_key.to_string(), None);
  }

  let filename = file_key[(last_sep + 1)..].to_string();
  let mut directory = file_key[..last_sep].to_string();
  if directory.is_empty() {
    // Root path like `/main.ts`.
    directory = file_key[..=last_sep].to_string();
  }

  (filename, Some(directory))
}

impl<'ctx> SsaDebug<'ctx> {
  fn new(module: &Module<'ctx>, program: &Program, entry_file: FileId, source: Arc<str>) -> Self {
    let entry_name = program
      .file_key(entry_file)
      .map(|k| k.to_string())
      .unwrap_or_else(|| "entry.ts".to_string());
    let (entry_filename, entry_parent) = split_file_key(&entry_name);
    let compile_dir = entry_parent.clone().unwrap_or_else(|| {
      std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
    });
    let file_dir = entry_parent.unwrap_or_else(|| ".".to_string());

    // Keep the module-level `source_filename` in sync with the DWARF compile-unit file so tools
    // that inspect LLVM IR (or fall back to the module header) see a meaningful entry filename.
    module.set_source_file_name(&entry_name);

    // See `codegen::debuginfo` for the rationale behind these debug-info defaults.
    let (builder, compile_unit) = module.create_debug_info_builder(
      true,
      DWARFSourceLanguage::CPlusPlus,
      &entry_filename,
      &compile_dir,
      "native-js",
      false,
      "",
      0,
      "",
      DWARFEmissionKind::Full,
      0,
      false,
      false,
      "",
      "",
    );

    let file = builder.create_file(&entry_filename, &file_dir);
    let i32_ty = builder
      .create_basic_type("i32", 32, 0x05, 0)
      .expect("failed to create `i32` debug type")
      .as_type();

    Self {
      builder,
      compile_unit,
      file,
      i32_ty,
      line_index: LineIndex::new(source),
    }
  }

  fn finalize(&self) {
    self.builder.finalize();
  }

  fn line_col_from_span(&self, span: Option<TextRange>) -> (u32, u32) {
    let Some(span) = span else {
      return (1, 0);
    };
    self.line_index.line_col(span.start)
  }

  fn create_subprogram(
    &self,
    program: &Program,
    def: DefId,
    line: u32,
    allow_void_return: bool,
    param_count: usize,
    function: FunctionValue<'ctx>,
    is_local_to_unit: bool,
  ) -> DISubprogram<'ctx> {
    let linkage_name = crate::llvm_symbol_for_def(program, def);
    // Prefer a friendly TS name for debuggers, but still attach the stable symbol as `linkageName`.
    let name = program.def_name(def).unwrap_or_else(|| linkage_name.clone());

    let return_ty = (!allow_void_return).then_some(self.i32_ty);
    let params: Vec<DIType<'ctx>> = std::iter::repeat(self.i32_ty).take(param_count).collect();
    let subroutine_type = self
      .builder
      .create_subroutine_type(self.file, return_ty, &params, 0);

    let sp = self.builder.create_function(
      self.file.as_debug_info_scope(),
      &name,
      Some(&linkage_name),
      self.file,
      line,
      subroutine_type,
      is_local_to_unit,
      true,
      line,
      0,
      false,
    );
    function.set_subprogram(sp);
    sp
  }

  fn location(&self, context: &'ctx Context, line: u32, col: u32, scope: DISubprogram<'ctx>) -> DILocation<'ctx> {
    self
      .builder
      .create_debug_location(context, line, col, scope.as_debug_info_scope(), None)
  }
}

struct ProgramCodegen<'ctx, 'p> {
  context: &'ctx Context,
  module: Module<'ctx>,
  i32_ty: IntType<'ctx>,
  program: &'p Program,
  opt_program: &'p OptProgram,
  #[allow(dead_code)]
  analyses: ProgramAnalyses,
  fn_defs: Vec<DefId>,
  llvm_fns: Vec<FunctionValue<'ctx>>,
  debug_subprograms: Vec<Option<DISubprogram<'ctx>>>,
  allow_void_return: Vec<bool>,
  foreign_fn_map: HashMap<SymbolId, usize>,
  exported_defs: HashSet<DefId>,
  debug: Option<SsaDebug<'ctx>>,
}

impl<'ctx, 'p> ProgramCodegen<'ctx, 'p> {
  fn new(
    context: &'ctx Context,
    program: &'p Program,
    entry_file: FileId,
    source: Arc<str>,
    opt_program: &'p OptProgram,
    analyses: ProgramAnalyses,
    fn_defs: Vec<DefId>,
    module_name: &str,
    debug: bool,
  ) -> Result<Self, Vec<Diagnostic>> {
    let i32_ty = context.i32_type();

    let mut exported_defs = HashSet::new();
    for entry in program.exports_of(fn_defs[0].file()).values() {
      if let Some(def) = entry.def {
        exported_defs.insert(def);
      }
    }

    let foreign_fn_map = collect_foreign_fn_map(opt_program.top_level.analyzed_cfg());

    let module = context.create_module(module_name);
    let debug = debug.then_some(SsaDebug::new(&module, program, entry_file, source));

    let mut cg = Self {
      context,
      module,
      i32_ty,
      program,
      opt_program,
      analyses,
      fn_defs,
      llvm_fns: Vec::new(),
      debug_subprograms: Vec::new(),
      allow_void_return: Vec::new(),
      foreign_fn_map,
      exported_defs,
      debug,
    };

    cg.declare_functions()?;
    Ok(cg)
  }

  fn finish(self) -> Module<'ctx> {
    if let Some(debug) = self.debug.as_ref() {
      debug.finalize();
    }
    self.module
  }

  fn declare_functions(&mut self) -> Result<(), Vec<Diagnostic>> {
    self.llvm_fns.clear();
    self.debug_subprograms.clear();
    self.allow_void_return.clear();
    self.llvm_fns.reserve(self.fn_defs.len());
    self.debug_subprograms.reserve(self.fn_defs.len());
    self.allow_void_return.reserve(self.fn_defs.len());

    for (fnid, def) in self.fn_defs.iter().copied().enumerate() {
      let sig = ts_function_sig_kind(self.program, def, self.opt_program.functions[fnid].params.len())?;
      self.allow_void_return.push(sig.allow_void_return);

      let params: Vec<_> = std::iter::repeat(self.i32_ty.into())
        .take(sig.param_count)
        .collect();
      let fn_ty = self.i32_ty.fn_type(&params, false);
      let name = crate::llvm_symbol_for_def(self.program, def);
      let linkage = if self.exported_defs.contains(&def) {
        None
      } else {
        Some(Linkage::Internal)
      };
      let f = self.module.add_function(&name, fn_ty, linkage);
      crate::stack_walking::apply_stack_walking_attrs(self.context, f);

      let sp = if let Some(debug) = self.debug.as_ref() {
        let span = self.program.span_of_def(def).map(|s| s.range);
        let (line, _col) = debug.line_col_from_span(span);
        let is_local_to_unit = f.get_linkage() == Linkage::Internal;
        Some(debug.create_subprogram(
          self.program,
          def,
          line,
          sig.allow_void_return,
          sig.param_count,
          f,
          is_local_to_unit,
        ))
      } else {
        None
      };

      self.llvm_fns.push(f);
      self.debug_subprograms.push(sp);
    }
    Ok(())
  }

  fn codegen_all_functions(&mut self) -> Result<(), Vec<Diagnostic>> {
    for fnid in 0..self.llvm_fns.len() {
      self.codegen_function(fnid)?;
    }
    Ok(())
  }

  fn codegen_function(&self, fnid: usize) -> Result<(), Vec<Diagnostic>> {
    let func = self.llvm_fns[fnid];
    if func.get_first_basic_block().is_some() {
      return Ok(());
    }

    let cfg = self
      .opt_program
      .functions
      .get(fnid)
      .ok_or_else(|| vec![Diagnostic::error(
        "NJS0150",
        "missing optimize-js function body",
        Span::new(self.fn_defs[fnid].file(), TextRange::new(0, 0)),
      )])?
      .analyzed_cfg();

    let debug_scope = self.debug_subprograms.get(fnid).copied().unwrap_or(None);
    let mut fc = FnCodegen::new(self, fnid, func, cfg, self.allow_void_return[fnid], debug_scope);
    fc.codegen()?;
    Ok(())
  }

  fn build_c_main(&self, main_fnid: usize) {
    let c_main = self.module.add_function("main", self.i32_ty.fn_type(&[], false), None);
    crate::stack_walking::apply_stack_walking_attrs(self.context, c_main);

    let builder = self.context.create_builder();
    let bb = self.context.append_basic_block(c_main, "entry");
    builder.position_at_end(bb);

    let call = builder
      .build_call(self.llvm_fns[main_fnid], &[], "ret")
      .expect("failed to build call");
    crate::stack_walking::mark_call_notail(call);
    let ret_val = call
      .try_as_basic_value()
      .left()
      .map(|v| v.into_int_value())
      .unwrap_or_else(|| self.i32_ty.const_zero());
    builder
      .build_return(Some(&ret_val))
      .expect("failed to build return");
  }
}

#[derive(Clone, Copy, Debug)]
struct SigInfo {
  param_count: usize,
  allow_void_return: bool,
}

fn ts_function_sig_kind(program: &Program, def: DefId, expected_param_count: usize) -> Result<SigInfo, Vec<Diagnostic>> {
  let span = program
    .span_of_def(def)
    .unwrap_or_else(|| Span::new(def.file(), TextRange::new(0, 0)));

  let func_ty = program.type_of_def_interned(def);
  let sigs = program.call_signatures(func_ty);
  if sigs.is_empty() {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "failed to resolve call signature for function",
      span,
    )]);
  }
  if sigs.len() != 1 {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "only single-signature functions are supported by native-js right now",
      span,
    )]);
  }
  let sig = &sigs[0].signature;
  if sig.this_param.is_some() {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "`this` parameters are not supported by native-js",
      span,
    )]);
  }
  if !sig.type_params.is_empty() {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "generic functions are not supported by native-js",
      span,
    )]);
  }

  if sig.params.len() != expected_param_count {
    return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
      "native-js: signature/parameter count mismatch",
      span,
    )]);
  }

  for (idx, param) in sig.params.iter().enumerate() {
    if param.optional || param.rest {
      return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!("optional/rest parameters are not supported by native-js yet (param #{idx})"),
        span,
      )]);
    }
    let kind = program.type_kind(param.ty);
    match kind {
      typecheck_ts::TypeKindSummary::Number
      | typecheck_ts::TypeKindSummary::NumberLiteral(_)
      | typecheck_ts::TypeKindSummary::Boolean
      | typecheck_ts::TypeKindSummary::BooleanLiteral(_) => {}
      _ => {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          format!(
            "unsupported parameter type for native-js ABI (expected number|boolean): {}",
            program.display_type(param.ty)
          ),
          span,
        )]);
      }
    }
  }

  let ret_kind = program.type_kind(sig.ret);
  let allow_void_return = matches!(
    ret_kind,
    typecheck_ts::TypeKindSummary::Void
      | typecheck_ts::TypeKindSummary::Undefined
      | typecheck_ts::TypeKindSummary::Never
  );
  if !allow_void_return {
    match ret_kind {
      typecheck_ts::TypeKindSummary::Number
      | typecheck_ts::TypeKindSummary::NumberLiteral(_)
      | typecheck_ts::TypeKindSummary::Boolean
      | typecheck_ts::TypeKindSummary::BooleanLiteral(_) => {}
      _ => {
        return Err(vec![codes::UNSUPPORTED_NATIVE_TYPE.error(
          format!(
            "unsupported return type for native-js ABI (expected number|boolean|void): {}",
            program.display_type(sig.ret)
          ),
          span,
        )]);
      }
    }
  }

  Ok(SigInfo {
    param_count: expected_param_count,
    allow_void_return,
  })
}

fn collect_foreign_fn_map(cfg: &Cfg) -> HashMap<SymbolId, usize> {
  let mut map = HashMap::new();
  for (_, insts) in cfg.bblocks.all() {
    for inst in insts.iter() {
      if inst.t != InstTyp::ForeignStore {
        continue;
      }
      let Some(arg) = inst.args.get(0) else {
        continue;
      };
      let Arg::Fn(fnid) = arg else {
        continue;
      };
      map.insert(inst.foreign, *fnid);
    }
  }
  map
}

struct PhiSpec<'ctx> {
  label: u32,
  tgt: u32,
  phi: PhiValue<'ctx>,
  incomings: Vec<(u32, Arg)>,
}

struct FnCodegen<'ctx, 'a, 'p> {
  cg: &'a ProgramCodegen<'ctx, 'p>,
  fnid: usize,
  func: FunctionValue<'ctx>,
  cfg: &'a Cfg,
  allow_void_return: bool,
  debug_scope: Option<DISubprogram<'ctx>>,

  builder: Builder<'ctx>,
  bbs: HashMap<u32, BasicBlock<'ctx>>,
  values: HashMap<u32, IntValue<'ctx>>,
  used_vars: HashSet<u32>,
  fn_vars: HashMap<u32, usize>,
  phis: Vec<PhiSpec<'ctx>>,
}

impl<'ctx, 'a, 'p> FnCodegen<'ctx, 'a, 'p> {
  fn new(
    cg: &'a ProgramCodegen<'ctx, 'p>,
    fnid: usize,
    func: FunctionValue<'ctx>,
    cfg: &'a Cfg,
    allow_void_return: bool,
    debug_scope: Option<DISubprogram<'ctx>>,
  ) -> Self {
    let mut used_vars = HashSet::new();
    for (_, insts) in cfg.bblocks.all() {
      for inst in insts.iter() {
        for arg in inst.args.iter() {
          if let Arg::Var(v) = arg {
            used_vars.insert(*v);
          }
        }
      }
    }

    Self {
      cg,
      fnid,
      func,
      cfg,
      allow_void_return,
      debug_scope,
      builder: cg.context.create_builder(),
      bbs: HashMap::new(),
      values: HashMap::new(),
      used_vars,
      fn_vars: HashMap::new(),
      phis: Vec::new(),
    }
  }

  fn set_debug_location(&self, span: Option<TextRange>) {
    let Some(debug) = self.cg.debug.as_ref() else {
      return;
    };
    let Some(scope) = self.debug_scope else {
      return;
    };
    let Some(span) = span else {
      // No explicit span: keep the previous debug location so these instructions still map to the
      // last known source location (instead of clobbering it with an "unknown" column).
      return;
    };
    let (line, col) = debug.line_col_from_span(Some(span));
    let loc = debug.location(self.cg.context, line, col, scope);
    self.builder.set_current_debug_location(loc);
  }

  fn codegen(&mut self) -> Result<(), Vec<Diagnostic>> {
    let def_span = self
      .cg
      .program
      .span_of_def(self.cg.fn_defs[self.fnid])
      .map(|s| s.range)
      .unwrap_or_else(|| TextRange::new(0, 0));
    self.set_debug_location(Some(def_span));

    self.create_blocks();
    self.bind_params()?;
    self.create_phi_nodes()?;
    self.codegen_blocks()?;
    self.fill_phi_incomings()?;
    Ok(())
  }

  fn create_blocks(&mut self) {
    // Codegen order matters because we build SSA values eagerly: a value must be emitted before any
    // instruction that references it. `optimize-js` produces valid SSA (defs dominate uses), so
    // emitting blocks in reverse postorder ensures definitions are encountered before dominated
    // uses, regardless of the arbitrary numeric block labels.
    for label in self.cfg.reverse_postorder() {
      let bb = self.cg.context.append_basic_block(self.func, &format!("bb{label}"));
      self.bbs.insert(label, bb);
    }
  }

  fn bind_params(&mut self) -> Result<(), Vec<Diagnostic>> {
    let il_params = &self.cg.opt_program.functions[self.fnid].params;
    for (idx, var) in il_params.iter().copied().enumerate() {
      let value = self
        .func
        .get_nth_param(idx as u32)
        .ok_or_else(|| {
          vec![Diagnostic::error(
            "NJS0150",
            "missing LLVM parameter for optimize-js function param",
            Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
          )]
        })?
        .into_int_value();
      self.values.insert(var, value);
    }
    Ok(())
  }

  fn create_phi_nodes(&mut self) -> Result<(), Vec<Diagnostic>> {
    for label in self.cfg.reverse_postorder() {
      let insts = self.cfg.bblocks.get(label);
      let bb = self.bbs[&label];
      self.builder.position_at_end(bb);

      for inst in insts.iter() {
        if inst.t != InstTyp::Phi {
          continue;
        }
        let tgt = inst.tgts.get(0).copied().ok_or_else(|| {
          vec![Diagnostic::error(
            "NJS0150",
            "malformed phi (missing target)",
            Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
          )]
        })?;
        // Work around SSA graphs that contain unused phi nodes with placeholder incoming values.
        // `optimize-js`'s SSA-construction pass can introduce phis for internal bookkeeping temps
        // that are later DCE'd by opt passes (which are currently disabled for `keep_ssa=true`).
        // Skipping unused phis keeps the backend robust without affecting semantics.
        if !self.used_vars.contains(&tgt) {
          continue;
        }
        let phi = self
          .builder
          .build_phi(self.cg.i32_ty, &format!("phi{tgt}"))
          .expect("failed to build phi");
        self.values.insert(tgt, phi.as_basic_value().into_int_value());
        let incomings = inst
          .labels
          .iter()
          .copied()
          .zip(inst.args.iter().cloned())
          .collect();
        self.phis.push(PhiSpec {
          label,
          tgt,
          phi,
          incomings,
        });
      }
    }
    Ok(())
  }

  fn fill_phi_incomings(&mut self) -> Result<(), Vec<Diagnostic>> {
    for spec in self.phis.iter() {
      for (pred, arg) in spec.incomings.iter().cloned() {
        let pred_bb = self.bbs.get(&pred).copied().ok_or_else(|| {
          vec![Diagnostic::error(
            "NJS0150",
            format!("phi refers to unknown predecessor block {pred}"),
            Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
          )]
        })?;
        let value = match &arg {
          Arg::Const(c) => self.const_i32(c)?,
          Arg::Var(v) => self.values.get(v).copied().ok_or_else(|| {
            vec![Diagnostic::error(
              "NJS0150",
              format!(
                "use of undefined SSA value %{v} (incoming to phi %{tgt} in bb{label} from bb{pred})",
                v = v,
                tgt = spec.tgt,
                label = spec.label,
                pred = pred,
              ),
              Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
            )]
          })?,
          other => {
            return Err(vec![Diagnostic::error(
              "NJS0151",
              format!("unsupported IL argument in SSA backend (expected i32): {other:?}"),
              Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
            )]);
          }
        };
        spec.phi.add_incoming(&[(&value, pred_bb)]);
      }
    }
    Ok(())
  }

  fn codegen_blocks(&mut self) -> Result<(), Vec<Diagnostic>> {
    // Emit all blocks (including potentially unreachable ones) so every LLVM basic block ends up
    // terminated and phi incoming lists remain consistent with the CFG edges.
    for label in self.cfg.reverse_postorder() {
      let bb = self.bbs[&label];
      self.builder.position_at_end(bb);

      let mut terminated = false;
      let insts = self.cfg.bblocks.get(label);
      for inst in insts.iter() {
        self.set_debug_location(inst.meta.span);
        match inst.t.clone() {
          InstTyp::Phi => {}
          InstTyp::VarAssign => self.codegen_var_assign(label, inst)?,
          InstTyp::Bin => self.codegen_bin(label, inst)?,
          InstTyp::Un => self.codegen_un(label, inst)?,
          InstTyp::Call => self.codegen_call(label, inst)?,
          InstTyp::ForeignLoad => self.codegen_foreign_load(label, inst)?,
          InstTyp::CondGoto => {
            self.codegen_cond_goto(label, inst)?;
            terminated = true;
            break;
          }
          InstTyp::Return => {
            self.codegen_return(label, inst)?;
            terminated = true;
            break;
          }
          other => {
            return Err(vec![Diagnostic::error(
              "NJS0151",
              format!("unsupported optimize-js instruction in SSA backend: {other:?}"),
              Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
            )]);
          }
        }
      }

      if !terminated && bb.get_terminator().is_none() {
        match self.cfg.terminator(label) {
          Terminator::Stop => {
            self.builder
              .build_unreachable()
              .expect("failed to build unreachable");
          }
          Terminator::Goto(target) => {
            let target_bb = self.bbs[&target];
            self.builder
              .build_unconditional_branch(target_bb)
              .expect("failed to build branch");
          }
          Terminator::CondGoto { .. } => {
            // `CondGoto` is always represented explicitly as an instruction; if we get here we
            // forgot to lower it above.
            return Err(vec![Diagnostic::error(
              "NJS0150",
              "unexpected implicit conditional terminator",
              Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
            )]);
          }
          Terminator::Multi { targets } => {
            return Err(vec![Diagnostic::error(
              "NJS0151",
              format!("unsupported multi-way branch in SSA backend (targets={targets:?})"),
              Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
            )]);
          }
        }
      }
    }
    Ok(())
  }

  fn codegen_var_assign(&mut self, label: u32, inst: &Inst) -> Result<(), Vec<Diagnostic>> {
    let (tgt, arg) = inst.as_var_assign();
    match arg {
      Arg::Fn(fnid) => {
        self.fn_vars.insert(tgt, *fnid);
      }
      Arg::Var(src) => {
        if let Some(fnid) = self.fn_vars.get(src).copied() {
          self.fn_vars.insert(tgt, fnid);
          return Ok(());
        }
        let v = self.values.get(src).copied().ok_or_else(|| {
          vec![Diagnostic::error(
            "NJS0150",
            format!("use of undefined SSA value %{src} (in bb{label} inst {inst:?})"),
            Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
          )]
        })?;
        self.values.insert(tgt, v);
      }
      _ => {
        let v = self.arg_i32_in_inst(label, inst, arg)?;
        self.values.insert(tgt, v);
      }
    }
    Ok(())
  }

  fn codegen_foreign_load(&mut self, _label: u32, inst: &Inst) -> Result<(), Vec<Diagnostic>> {
    let Some(&tgt) = inst.tgts.get(0) else {
      return Err(vec![Diagnostic::error(
        "NJS0150",
        "malformed foreign load (missing target)",
        Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
      )]);
    };
    if let Some(&fnid) = self.cg.foreign_fn_map.get(&inst.foreign) {
      self.fn_vars.insert(tgt, fnid);
      return Ok(());
    }
    Err(vec![Diagnostic::error(
      "NJS0153",
      "SSA backend only supports foreign loads of hoisted top-level function declarations",
      Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
    )])
  }

  fn codegen_bin(&mut self, label: u32, inst: &Inst) -> Result<(), Vec<Diagnostic>> {
    let (tgt, lhs, op, rhs) = inst.as_bin();
    let lhs = self.arg_i32_in_inst(label, inst, lhs)?;
    let rhs = self.arg_i32_in_inst(label, inst, rhs)?;
    let out = match op {
      BinOp::Add => self.builder.build_int_add(lhs, rhs, "add").expect("add"),
      BinOp::Sub => self.builder.build_int_sub(lhs, rhs, "sub").expect("sub"),
      BinOp::Mul => self.builder.build_int_mul(lhs, rhs, "mul").expect("mul"),
      BinOp::Div => self.builder.build_int_signed_div(lhs, rhs, "div").expect("div"),
      BinOp::Mod => self.builder.build_int_signed_rem(lhs, rhs, "rem").expect("rem"),
      BinOp::Shl => self.builder.build_left_shift(lhs, rhs, "shl").expect("shl"),
      BinOp::Shr => self
        .builder
        .build_right_shift(lhs, rhs, true, "shr")
        .expect("shr"),
      BinOp::UShr => self
        .builder
        .build_right_shift(lhs, rhs, false, "ushr")
        .expect("ushr"),
      BinOp::BitAnd => self.builder.build_and(lhs, rhs, "and").expect("and"),
      BinOp::BitOr => self.builder.build_or(lhs, rhs, "or").expect("or"),
      BinOp::BitXor => self.builder.build_xor(lhs, rhs, "xor").expect("xor"),

      BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq | BinOp::StrictEq | BinOp::NotStrictEq => {
        let pred = match op {
          BinOp::Lt => IntPredicate::SLT,
          BinOp::Leq => IntPredicate::SLE,
          BinOp::Gt => IntPredicate::SGT,
          BinOp::Geq => IntPredicate::SGE,
          BinOp::StrictEq => IntPredicate::EQ,
          BinOp::NotStrictEq => IntPredicate::NE,
          _ => unreachable!(),
        };
        let cmp = self
          .builder
          .build_int_compare(pred, lhs, rhs, "cmp")
          .expect("cmp");
        self.bool_to_i32(cmp)
      }

      other => {
        return Err(vec![Diagnostic::error(
          "NJS0151",
          format!("unsupported optimize-js binary op in SSA backend: {other:?}"),
          Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
        )]);
      }
    };
    self.values.insert(tgt, out);
    Ok(())
  }

  fn codegen_un(&mut self, label: u32, inst: &Inst) -> Result<(), Vec<Diagnostic>> {
    let (tgt, op, arg) = inst.as_un();
    let v = self.arg_i32_in_inst(label, inst, arg)?;
    let out = match op {
      UnOp::Plus => v,
      UnOp::Neg => self.builder.build_int_neg(v, "neg").expect("neg"),
      UnOp::BitNot => self.builder.build_not(v, "not").expect("not"),
      UnOp::Not => {
        let is_zero = self
          .builder
          .build_int_compare(IntPredicate::EQ, v, self.cg.i32_ty.const_zero(), "is_zero")
          .expect("cmp");
        self.bool_to_i32(is_zero)
      }
      other => {
        return Err(vec![Diagnostic::error(
          "NJS0151",
          format!("unsupported optimize-js unary op in SSA backend: {other:?}"),
          Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
        )]);
      }
    };
    self.values.insert(tgt, out);
    Ok(())
  }

  fn codegen_call(&mut self, label: u32, inst: &Inst) -> Result<(), Vec<Diagnostic>> {
    // args[0] = callee, args[1] = this, args[2..] = call args
    if !inst.spreads.is_empty() {
      return Err(vec![Diagnostic::error(
        "NJS0151",
        "spread call arguments are not supported in SSA backend",
        Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
      )]);
    }
    let callee = inst.args.get(0).ok_or_else(|| vec![Diagnostic::error(
      "NJS0150",
      "malformed call (missing callee)",
      Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
    )])?;
    let this_arg = inst.args.get(1).ok_or_else(|| vec![Diagnostic::error(
      "NJS0150",
      "malformed call (missing this)",
      Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
    )])?;
    // For now only support direct calls (`foo(x)`), which lower with `this=undefined`.
    if !matches!(this_arg, Arg::Const(Const::Undefined)) {
      return Err(vec![Diagnostic::error(
        "NJS0151",
        "SSA backend only supports direct calls (callee must have `this=undefined`)",
        Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
      )]);
    }

    let fnid = self.resolve_fn_callee(callee)?;
    let callee_fn = *self
      .cg
      .llvm_fns
      .get(fnid)
      .ok_or_else(|| vec![Diagnostic::error(
        "NJS0150",
        format!("call references unknown function id Fn{fnid}"),
        Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
      )])?;

    let mut args = Vec::new();
    for arg in inst.args.iter().skip(2) {
      args.push(self.arg_i32_in_inst(label, inst, arg)?.into());
    }
    let call = self
      .builder
      .build_call(callee_fn, &args, "call")
      .expect("failed to build call");
    crate::stack_walking::mark_call_notail(call);

    if let Some(&tgt) = inst.tgts.get(0) {
      let value = call
        .try_as_basic_value()
        .left()
        .map(|v| v.into_int_value())
        .unwrap_or_else(|| self.cg.i32_ty.const_zero());
      self.values.insert(tgt, value);
    }
    Ok(())
  }

  fn resolve_fn_callee(&self, callee: &Arg) -> Result<usize, Vec<Diagnostic>> {
    match callee {
      Arg::Fn(id) => Ok(*id),
      Arg::Var(v) => self.fn_vars.get(v).copied().ok_or_else(|| {
        vec![Diagnostic::error(
          "NJS0151",
          "SSA backend only supports direct calls to known functions",
          Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
        )]
      }),
      _ => Err(vec![Diagnostic::error(
        "NJS0151",
        "SSA backend only supports direct calls to known functions",
        Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
      )]),
    }
  }

  fn codegen_cond_goto(&mut self, label: u32, inst: &Inst) -> Result<(), Vec<Diagnostic>> {
    let (cond, t, f) = inst.as_cond_goto();
    let cond = self.arg_i32_in_inst(label, inst, cond)?;
    let cond_i1 = self
      .builder
      .build_int_compare(IntPredicate::NE, cond, self.cg.i32_ty.const_zero(), "cond")
      .expect("cond");
    let t_bb = self.bbs[&t];
    let f_bb = self.bbs[&f];
    self
      .builder
      .build_conditional_branch(cond_i1, t_bb, f_bb)
      .expect("br");
    Ok(())
  }

  fn codegen_return(&mut self, label: u32, inst: &Inst) -> Result<(), Vec<Diagnostic>> {
    let value = inst.as_return().cloned();
    match value {
      Some(arg) if self.allow_void_return => {
        // Mirror the HIR backend: allow returning a value from a void function, but ignore it.
        let _ = self.arg_i32_in_inst(label, inst, &arg)?;
        self
          .builder
          .build_return(Some(&self.cg.i32_ty.const_zero()))
          .expect("ret");
        Ok(())
      }
      Some(arg) => {
        let v = self.arg_i32_in_inst(label, inst, &arg)?;
        self.builder.build_return(Some(&v)).expect("ret");
        Ok(())
      }
      None if self.allow_void_return => {
        self
          .builder
          .build_return(Some(&self.cg.i32_ty.const_zero()))
          .expect("ret");
        Ok(())
      }
      None => Err(vec![Diagnostic::error(
        "NJS0116",
        "`return` without a value is not supported in this codegen subset",
        Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
      )]),
    }
  }

  fn arg_i32_in_inst(&self, label: u32, inst: &Inst, arg: &Arg) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    match arg {
      Arg::Const(c) => self.const_i32(c),
      Arg::Var(v) => self.values.get(v).copied().ok_or_else(|| {
        vec![Diagnostic::error(
          "NJS0150",
          format!("use of undefined SSA value %{v} (in bb{label} inst {inst:?})"),
          Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
        )]
      }),
      other => Err(vec![Diagnostic::error(
        "NJS0151",
        format!("unsupported IL argument in SSA backend (expected i32): {other:?}"),
        Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
      )]),
    }
  }

  fn const_i32(&self, c: &Const) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    match c {
      Const::Bool(b) => Ok(self.cg.i32_ty.const_int(if *b { 1 } else { 0 }, false)),
      Const::Num(n) => {
        let f = n.0;
        if !f.is_finite() || f.fract() != 0.0 || f < i32::MIN as f64 || f > i32::MAX as f64 {
          return Err(vec![Diagnostic::error(
            "NJS0104",
            "numeric literal cannot be represented as a 32-bit integer",
            Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
          )]);
        }
        Ok(self.cg.i32_ty.const_int(f as i64 as u64, true))
      }
      other => Err(vec![Diagnostic::error(
        "NJS0151",
        format!("unsupported constant in SSA backend: {other:?}"),
        Span::new(self.cg.fn_defs[self.fnid].file(), TextRange::new(0, 0)),
      )]),
    }
  }

  fn bool_to_i32(&self, v: IntValue<'ctx>) -> IntValue<'ctx> {
    self
      .builder
      .build_int_z_extend(v, self.cg.i32_ty, "bool")
      .expect("zext")
  }
}
