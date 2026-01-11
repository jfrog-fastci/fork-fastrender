//! Compilation entry points.
//!
//! `native-js` currently has two (partially overlapping) pipelines:
//! - A HIR/typecheck-driven path used by the native compiler driver (strict subset validation +
//!   LLVM lowering).
//! - A small `parse-js`-driven path used by early smoke tests and debugging tools, which can emit
//!   runnable artifacts by turning generated LLVM IR into an object file and linking it with the
//!   system toolchain.
//!
//! ## Diagnostic codes
//!
//! This module emits stable `NJS####` diagnostic codes:
//! - `NJS0201`: failed to access lowered HIR for the entry file

use crate::codes;
use crate::emit::TargetConfig;
use crate::llvm::expr::{FunctionCodegen, FunctionSymbol};
use crate::llvm::{LlvmBackend, ValueKind};
use crate::{
  compile_typescript_to_llvm_ir, emit, link, Artifact, CompileOptions, CompilerOptions, EmitKind,
  NativeJsError, OptLevel,
};
use diagnostics::{Diagnostic, Severity, Span, TextRange};
use hir_js::{Body, BodyId, DefId, DefKind, ExprId, ExprKind, FileKind, FunctionData, NameId, PatKind};
use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::module::Module;
use inkwell::types::BasicMetadataTypeEnum;
use inkwell::values::BasicValueEnum;
use inkwell::OptimizationLevel;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use typecheck_ts::{BodyCheckResult, FileId, FileKey, Host, Program};
/// Parse + typecheck a TypeScript program and then validate that it fits the
/// strict native-js subset.
///
/// This helper is intended for the native compilation driver: validation runs
/// only after the program typechecks cleanly, and must complete before any LLVM
/// IR is generated.
pub fn typecheck_and_validate_strict_subset(
  host: impl Host,
  roots: Vec<FileKey>,
) -> Result<Program, Vec<Diagnostic>> {
  let program = Program::new(host, roots);
  let diagnostics = program.check();
  if diagnostics.iter().any(|diag| diag.severity == Severity::Error) {
    return Err(diagnostics);
  }
  crate::validate::validate_strict_subset(&program)?;
  Ok(program)
}

pub struct CompileOutput {
  pub path: PathBuf,
  // Keep tempdir alive for as long as the output is needed.
  _tempdir: Option<TempDir>,
}

/// Compile textual LLVM IR into an on-disk artifact specified by [`CompileOptions::emit`].
///
/// This is a low-level helper intended for tooling and tests that already have LLVM IR (for example
/// `native-js-cli`, which uses the project/module resolver to build a multi-file IR module).
///
/// If `output_path` is `None`, the artifact is written into a temporary directory and cleaned up
/// when the returned [`CompileOutput`] is dropped (unless `opts.debug` is set).
pub fn compile_llvm_ir_to_artifact(
  llvm_ir: &str,
  opts: CompileOptions,
  output_path: Option<PathBuf>,
) -> Result<CompileOutput, NativeJsError> {
  if matches!(opts.emit, EmitKind::Executable) && !cfg!(target_os = "linux") {
    return Err(NativeJsError::UnsupportedPlatform {
      target_os: std::env::consts::OS.to_string(),
    });
  }

  let (out_path, out_tempdir) = resolve_output_path(opts.emit, opts.debug, output_path)?;

  if let Some(path) = opts.emit_ir.as_deref() {
    ensure_parent_dir(path)?;
    write_file(path, llvm_ir.as_bytes())?;
  }
  match opts.emit {
    EmitKind::LlvmIr => {
      write_file(&out_path, llvm_ir.as_bytes())?;
      Ok(CompileOutput {
        path: out_path,
        _tempdir: out_tempdir,
      })
    }

    EmitKind::Bitcode | EmitKind::Object | EmitKind::Assembly | EmitKind::Executable => {
      let context = Context::create();
      let module = parse_ir(&context, llvm_ir)?;

      module
        .verify()
        .map_err(|e| NativeJsError::Llvm(format!("module verification failed: {e}")))?;

      let target = target_config_from_opts(&opts);

      match opts.emit {
        EmitKind::Bitcode => {
          let bc = emit::emit_bitcode(&module);
          write_file(&out_path, &bc)?;
        }
        EmitKind::Object => {
          let obj =
            emit::emit_object_with_statepoints(&module, target).map_err(|e| NativeJsError::Llvm(e.to_string()))?;
          write_file(&out_path, &obj)?;
        }
        EmitKind::Assembly => {
          let asm =
            emit::emit_asm_with_statepoints(&module, target).map_err(|e| NativeJsError::Llvm(e.to_string()))?;
          write_file(&out_path, &asm)?;
        }
        EmitKind::Executable => {
          let obj =
            emit::emit_object_with_statepoints(&module, target).map_err(|e| NativeJsError::Llvm(e.to_string()))?;

          let mut tmp: Option<TempDir> = None;
          let obj_path = if opts.debug {
            path_with_suffix(&out_path, ".o")
          } else {
            let dir = TempDir::new().map_err(NativeJsError::TempDirCreateFailed)?;
            let obj_path = dir.path().join("out.o");
            tmp = Some(dir);
            obj_path
          };

          write_file(&obj_path, &obj)?;
          if opts.debug {
            let ll_path = path_with_suffix(&out_path, ".ll");
            write_file(&ll_path, llvm_ir.as_bytes())?;
          }

          link::link_object_to_executable(&obj_path, &out_path)?;
          drop(tmp);
        }
        EmitKind::LlvmIr => unreachable!("handled earlier"),
      }

      Ok(CompileOutput {
        path: out_path,
        _tempdir: out_tempdir,
      })
    }
  }
}

/// Compile TypeScript source into an on-disk artifact specified by [`CompileOptions::emit`].
///
/// If `output_path` is `None`, the artifact is written into a temporary directory and cleaned up
/// when the returned [`CompileOutput`] is dropped (unless `opts.debug` is set).
pub fn compile_typescript_to_artifact(
  source: &str,
  opts: CompileOptions,
  output_path: Option<PathBuf>,
) -> Result<CompileOutput, NativeJsError> {
  let ir = compile_typescript_to_llvm_ir(source, opts.clone())?;
  compile_llvm_ir_to_artifact(&ir, opts, output_path)
}

fn parse_ir<'ctx>(
  context: &'ctx Context,
  ir: &str,
) -> Result<inkwell::module::Module<'ctx>, NativeJsError> {
  let buf = MemoryBuffer::create_from_memory_range_copy(ir.as_bytes(), "module.ll");
  context
    .create_module_from_ir(buf)
    .map_err(|e| NativeJsError::Llvm(e.to_string()))
}

fn target_config_from_opts(opts: &CompileOptions) -> TargetConfig {
  let mut cfg = TargetConfig::default();

  cfg.opt_level = match opts.opt_level {
    OptLevel::O0 => OptimizationLevel::None,
    OptLevel::O1 => OptimizationLevel::Less,
    OptLevel::O2 => OptimizationLevel::Default,
    OptLevel::O3 => OptimizationLevel::Aggressive,
    // LLVM's `OptimizationLevel` doesn't distinguish size opts; map to a reasonable default.
    OptLevel::Os | OptLevel::Oz => OptimizationLevel::Default,
  };

  if let Some(triple) = &opts.target {
    cfg.triple = inkwell::targets::TargetTriple::create(&triple.to_string());
  }

  cfg
}

fn resolve_output_path(
  emit: EmitKind,
  debug: bool,
  output_path: Option<PathBuf>,
) -> Result<(PathBuf, Option<TempDir>), NativeJsError> {
  if let Some(path) = output_path {
    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent).map_err(|e| NativeJsError::Io {
          path: parent.to_path_buf(),
          source: e,
        })?;
      }
    }
    return Ok((path, None));
  }

  let dir = TempDir::new().map_err(NativeJsError::TempDirCreateFailed)?;
  let filename = match emit {
    EmitKind::LlvmIr => "out.ll",
    EmitKind::Bitcode => "out.bc",
    EmitKind::Object => "out.o",
    EmitKind::Assembly => "out.s",
    EmitKind::Executable => "out",
  };
  let out_path = dir.path().join(filename);

  let dir = if debug {
    let _ = dir.keep();
    None
  } else {
    Some(dir)
  };

  Ok((out_path, dir))
}

fn write_file(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), NativeJsError> {
  std::fs::write(path, contents.as_ref()).map_err(|e| NativeJsError::Io {
    path: path.to_path_buf(),
    source: e,
  })
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
  let mut os = path.as_os_str().to_owned();
  os.push(suffix);
  PathBuf::from(os)
}

pub struct CompileResult {
  pub llvm_ir: Option<String>,
  pub diagnostics: Vec<Diagnostic>,
}

/// Test helper: compile the given file's exported entry function into LLVM IR.
///
/// This currently supports only a minimal expression subset (numbers/booleans +
/// arithmetic/comparisons) and is intended for incremental development of the
/// HIR-driven backend.
pub fn compile_entry_to_llvm_ir(program: &Program, file: FileId, entry_export: &str) -> CompileResult {
  let mut diagnostics = Vec::new();

  // Ensure analysis/typechecking has run far enough that exports + HIR are available.
  let exports = program.exports_of(file);
  if exports.get(entry_export).and_then(|entry| entry.def).is_none() {
    diagnostics.push(codes::UNSUPPORTED_EXPR.error(
      format!("missing export `{entry_export}`"),
      Span::new(file, TextRange::new(0, 0)),
    ));
  }

  let Some(lowered) = program.hir_lowered(file) else {
    diagnostics.push(codes::UNSUPPORTED_EXPR.error(
      "missing HIR lowering for file",
      Span::new(file, TextRange::new(0, 0)),
    ));
    return CompileResult {
      llvm_ir: None,
      diagnostics,
    };
  };

  // Collect top-level function definitions we can see in this file. We compile
  // them all into one LLVM module so direct calls can be resolved locally.
  let mut defs: Vec<_> = lowered
    .defs
    .iter()
    .filter(|def| matches!(def.path.kind, DefKind::Function) && def.body.is_some())
    .map(|def| (def.id, def.name, def.body.unwrap()))
    .collect();
  defs.sort_by_key(|(def, _, _)| def.0);

  let context = Context::create();
  let options = CompileOptions {
    // Keep the IR readable/deterministic for tests; we also do not run an LLVM
    // optimization pipeline here.
    opt_level: OptLevel::O0,
    emit: EmitKind::LlvmIr,
    ..CompileOptions::default()
  };

  let mut backend = match LlvmBackend::new(&context, "native-js-expr", &options) {
    Ok(backend) => backend,
    Err(err) => {
      diagnostics.push(diagnostics::host_error(
        None,
        format!("failed to initialize LLVM backend: {err}"),
      ));
      return CompileResult {
        llvm_ir: None,
        diagnostics,
      };
    }
  };

  // Phase 1: declare function prototypes so calls can reference functions that
  // appear later in source order.
  let mut functions: HashMap<NameId, FunctionSymbol> = HashMap::new();
  for (def_id, name_id, body_id) in defs.iter().copied() {
    let Some(body) = lowered.body(body_id) else {
      continue;
    };
    let Some(symbol) = declare_function(program, &mut diagnostics, &mut backend, def_id, name_id, body_id, body)
    else {
      continue;
    };
    // Prefer the first declaration in deterministic DefId order.
    functions.entry(name_id).or_insert(symbol);
  }

  // Phase 2: codegen bodies.
  for (_def_id, name_id, body_id) in defs {
    let Some(symbol) = functions.get(&name_id).cloned() else {
      continue;
    };
    let Some(body) = lowered.body(body_id) else {
      continue;
    };
    let Some(func_data) = body.function.as_ref() else {
      continue;
    };

    let entry_block = backend.append_basic_block(symbol.function, "entry");
    backend.builder.position_at_end(entry_block);

    let param_names = param_names(body, func_data, &mut diagnostics, body_id);
    let types = program.check_body(body_id);
    let mut fc = FunctionCodegen::new(
      &mut backend,
      program,
      body_id,
      body,
      types.as_ref(),
      &functions,
      &mut diagnostics,
      symbol.function,
    );

    fc.codegen_params(&param_names, &symbol.params);
    let returned = fc.codegen_function_body(func_data, symbol.ret);

    // Ensure the function is well-formed even if codegen failed.
    let needs_default_return = backend
      .builder
      .get_insert_block()
      .and_then(|bb| bb.get_terminator())
      .is_none();
    if !returned || needs_default_return {
      match symbol.ret {
        ValueKind::Number => {
          let default_ret: BasicValueEnum<'_> = backend.f64_type().const_float(0.0).into();
          let _ = backend.builder.build_return(Some(&default_ret));
        }
        ValueKind::Boolean => {
          let default_ret: BasicValueEnum<'_> = backend.bool_type().const_int(0, false).into();
          let _ = backend.builder.build_return(Some(&default_ret));
        }
        ValueKind::Void => {
          let _ = backend.builder.build_return(None);
        }
      }
    }
  }

  if let Err(err) = backend.verify() {
    diagnostics.push(diagnostics::ice(
      Span::new(file, TextRange::new(0, 0)),
      format!("invalid LLVM module: {err}"),
    ));
  }

  codes::normalize_diagnostics(&mut diagnostics);
  let has_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);
  let llvm_ir = (!has_errors).then(|| backend.module.print_to_string().to_string());

  CompileResult { llvm_ir, diagnostics }
}

fn declare_function<'ctx>(
  program: &Program,
  diagnostics: &mut Vec<Diagnostic>,
  backend: &mut LlvmBackend<'ctx>,
  def_id: DefId,
  _name_id: NameId,
  body_id: BodyId,
  body: &Body,
) -> Option<FunctionSymbol<'ctx>> {
  let Some(func) = body.function.as_ref() else {
    return None;
  };

  let span = program
    .span_of_def(def_id)
    .unwrap_or(Span::new(body_id.file(), body.span));

  let fn_ty = program.type_of_def_interned(def_id);
  let sigs = program.call_signatures(fn_ty);
  if sigs.len() != 1 {
    diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
      "only single-signature functions are supported by native-js right now",
      span,
    ));
    return None;
  }
  let sig = &sigs[0].signature;
  if sig.this_param.is_some() {
    diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
      "`this` parameters are not supported by native-js",
      span,
    ));
    return None;
  }
  if !sig.type_params.is_empty() {
    diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
      "generic functions are not supported by native-js",
      span,
    ));
    return None;
  }

  if sig.params.len() != func.params.len() {
    diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
      "native-js: signature/parameter count mismatch",
      span,
    ));
    return None;
  }

  // Infer parameter kinds. Bail out if any parameter is unsupported: we can't
  // represent it in the current subset and calls won't be type-safe.
  let mut param_kinds = Vec::new();
  for (sig_param, hir_param) in sig.params.iter().zip(func.params.iter()) {
    let PatKind::Ident(_) = body.pats.get(hir_param.pat.0 as usize)?.kind else {
      diagnostics.push(codes::UNSUPPORTED_EXPR.error(
        "unsupported parameter pattern",
        program
          .pat_span(body_id, hir_param.pat)
          .unwrap_or(Span::new(body_id.file(), body.span)),
      ));
      return None;
    };
    if sig_param.optional || sig_param.rest {
      diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        "optional/rest parameters are not supported by native-js yet",
        program
          .pat_span(body_id, hir_param.pat)
          .unwrap_or(Span::new(body_id.file(), body.span)),
      ));
      return None;
    }
    let kind_summary = program.type_kind(sig_param.ty);
    let Some(kind) = ValueKind::from_type_kind(&kind_summary) else {
      diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!(
          "unsupported parameter type for native-js ABI (expected number|boolean): {}",
          program.display_type(sig_param.ty)
        ),
        program
          .pat_span(body_id, hir_param.pat)
          .unwrap_or(Span::new(body_id.file(), body.span)),
      ));
      return None;
    };
    if kind == ValueKind::Void {
      diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        "parameters of type `void`/`undefined` are not supported by native-js",
        program
          .pat_span(body_id, hir_param.pat)
          .unwrap_or(Span::new(body_id.file(), body.span)),
      ));
      return None;
    }
    param_kinds.push(kind);
  }

  let ret_kind_summary = program.type_kind(sig.ret);
  let ret_kind = match ValueKind::from_type_kind(&ret_kind_summary) {
    Some(kind) => kind,
    None => {
      // Keep compiling to surface expression-level diagnostics, but fall back to
      // `number` for the LLVM signature.
      diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!(
          "unsupported return type for native-js ABI (expected number|boolean|void): {}",
          program.display_type(sig.ret)
        ),
        span,
      ));
      ValueKind::Number
    }
  };

  let param_tys: Vec<BasicMetadataTypeEnum<'ctx>> = param_kinds
    .iter()
    .copied()
    .map(|k| backend.llvm_type(k).into())
    .collect();
  let fn_type = match ret_kind {
    ValueKind::Number => backend.f64_type().fn_type(&param_tys, false),
    ValueKind::Boolean => backend.bool_type().fn_type(&param_tys, false),
    ValueKind::Void => backend.context.void_type().fn_type(&param_tys, false),
  };

  let llvm_name = crate::llvm_symbol_for_def(program, def_id);
  let function = backend.module.add_function(&llvm_name, fn_type, None);
  crate::stack_walking::apply_stack_walking_attrs(backend.context, function);

  Some(FunctionSymbol {
    function,
    params: param_kinds,
    ret: ret_kind,
  })
}

fn param_names(body: &Body, func: &FunctionData, diagnostics: &mut Vec<Diagnostic>, body_id: BodyId) -> Vec<NameId> {
  let mut names = Vec::new();
  for param in func.params.iter() {
    let Some(pat) = body.pats.get(param.pat.0 as usize) else {
      continue;
    };
    match pat.kind {
      PatKind::Ident(name) => names.push(name),
      _ => {
        diagnostics.push(codes::UNSUPPORTED_EXPR.error(
          "unsupported parameter pattern",
          Span::new(body_id.file(), pat.span),
        ));
      }
    }
  }
  names
}

pub(crate) fn compile_program(
  program: &Program,
  entry: FileId,
  opts: &CompilerOptions,
) -> Result<Artifact, NativeJsError> {
  let compiler = Compiler { program, entry, opts };
  compiler.compile()
}

/// Compile a program assuming it has already been successfully type-checked via
/// `Program::check()` (i.e. no `Severity::Error` diagnostics).
///
/// This is an internal helper used to avoid redundant typechecking when layering
/// wrappers (e.g. [`crate::compile`] calling into [`crate::compile_program`]).
pub(crate) fn compile_program_checked(
  program: &Program,
  entry: FileId,
  opts: &CompilerOptions,
) -> Result<Artifact, NativeJsError> {
  let compiler = Compiler { program, entry, opts };
  compiler.compile_checked()
}

struct Compiler<'a> {
  program: &'a Program,
  entry: FileId,
  opts: &'a CompilerOptions,
}

impl<'a> Compiler<'a> {
  fn compile(&self) -> Result<Artifact, NativeJsError> {
    self.ensure_typecheck_ok()?;
    self.compile_checked()
  }

  fn compile_checked(&self) -> Result<Artifact, NativeJsError> {
    let loaded = self.load_hir_and_types()?;
    self.validate_strict_subset()?;
    self.reject_disabled_builtins()?;

    let context = Context::create();
    let module = self.build_llvm_module(&context, &loaded)?;

    self.emit_extra_llvm_ir(&module)?;

    module
      .verify()
      .map_err(|e| NativeJsError::Llvm(format!("LLVM module verification failed: {e}")))?;

    self.emit_artifact(&module)
  }

  fn ensure_typecheck_ok(&self) -> Result<(), NativeJsError> {
    let diagnostics = self.program.check();
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
      return Err(NativeJsError::TypecheckFailed { diagnostics });
    }
    Ok(())
  }

  fn load_hir_and_types(&self) -> Result<LoadedProgram, NativeJsError> {
    let lowered = self
      .program
      .hir_lowered(self.entry)
      .ok_or_else(|| NativeJsError::Rejected {
        diagnostics: vec![codes::MISSING_ENTRY_HIR.error(
          "failed to access lowered HIR for entry file",
          Span::new(self.entry, TextRange::new(0, 0)),
        )],
      })?;

    // Locate `export function main()` and validate its shape.
    let entrypoint = crate::strict::entrypoint(self.program, self.entry)
      .map_err(|diagnostics| NativeJsError::Rejected { diagnostics })?;

    // Ensure type tables are materialized for bodies we intend to codegen.
    //
    // (The strict subset validator will also call `check_body` for every body it
    // touches, but we materialize these explicitly since we know we'll need
    // them for lowering.)
    let mut checked_bodies: BTreeMap<BodyId, Arc<BodyCheckResult>> = BTreeMap::new();
    let root_body = lowered.root_body();
    checked_bodies.insert(root_body, self.program.check_body(root_body));
    checked_bodies.insert(
      entrypoint.main_body,
      self.program.check_body(entrypoint.main_body),
    );

    Ok(LoadedProgram {
      lowered,
      checked_bodies,
      entrypoint,
    })
  }

  fn validate_strict_subset(&self) -> Result<(), NativeJsError> {
    crate::validate::validate_strict_subset(self.program)
      .map_err(|diagnostics| NativeJsError::Rejected { diagnostics })
  }

  fn reject_disabled_builtins(&self) -> Result<(), NativeJsError> {
    if self.opts.builtins {
      return Ok(());
    }

    fn callee_is_ident(body: &Body, lowered: &hir_js::LowerResult, expr: ExprId, target: &str) -> bool {
      let Some(expr) = body.exprs.get(expr.0 as usize) else {
        return false;
      };
      match &expr.kind {
        ExprKind::Ident(name) => lowered.names.resolve(*name) == Some(target),
        _ => false,
      }
    }

    let mut diagnostics = Vec::new();
    for file in self.program.reachable_files() {
      let Some(lowered) = self.program.hir_lowered(file) else {
        continue;
      };
      if matches!(lowered.hir.file_kind, FileKind::Dts) {
        continue;
      }

      for body_id in self.program.bodies_in_file(file) {
        let Some(body) = lowered.body(body_id) else {
          continue;
        };
        for expr in body.exprs.iter() {
          let ExprKind::Call(call) = &expr.kind else {
            continue;
          };
          if callee_is_ident(body, lowered.as_ref(), call.callee, "print") {
            diagnostics.push(
              codes::BUILTINS_DISABLED
                .error(
                  "`print(...)` intrinsic is disabled because builtin intrinsics are disabled",
                  Span::new(file, expr.span),
                )
                .with_note("re-enable builtin intrinsics by setting `CompilerOptions.builtins = true`"),
            );
          }
        }
      }
    }

    if diagnostics.is_empty() {
      Ok(())
    } else {
      codes::normalize_diagnostics(&mut diagnostics);
      Err(NativeJsError::Rejected { diagnostics })
    }
  }

  fn build_llvm_module<'ctx>(
    &self,
    context: &'ctx Context,
    loaded: &LoadedProgram,
  ) -> Result<Module<'ctx>, NativeJsError> {
    crate::codegen::codegen(
      context,
      self.program,
      self.entry,
      loaded.entrypoint,
      crate::codegen::CodegenOptions {
        module_name: "native-js".to_string(),
      },
    )
    .map_err(|diagnostics| NativeJsError::Rejected { diagnostics })
  }

  fn emit_artifact<'ctx>(&self, module: &Module<'ctx>) -> Result<Artifact, NativeJsError> {
    match self.opts.emit {
      EmitKind::LlvmIr => {
        let path = self.output_path(".ll")?;
        let ir = emit::emit_llvm_ir(module);
        write_file(&path, ir.as_bytes())?;
        Ok(Artifact {
          kind: self.opts.emit,
          path: path.clone(),
          stdout_hint: Some(format!("wrote {}", path.display())),
        })
      }
      EmitKind::Bitcode => {
        let path = self.output_path(".bc")?;
        let bytes = emit::emit_bitcode(module);
        write_file(&path, bytes)?;
        Ok(Artifact {
          kind: self.opts.emit,
          path: path.clone(),
          stdout_hint: Some(format!("wrote {}", path.display())),
        })
      }
      EmitKind::Object => {
        let path = self.output_path(".o")?;
        let bytes = emit::emit_object_with_statepoints(module, target_config_from_opts(self.opts))
          .map_err(|e| NativeJsError::Llvm(e.to_string()))?;
        write_file(&path, bytes)?;
        Ok(Artifact {
          kind: self.opts.emit,
          path: path.clone(),
          stdout_hint: Some(format!("wrote {}", path.display())),
        })
      }
      EmitKind::Assembly => {
        let path = self.output_path(".s")?;
        let bytes = emit::emit_asm_with_statepoints(module, target_config_from_opts(self.opts))
          .map_err(|e| NativeJsError::Llvm(e.to_string()))?;
        write_file(&path, bytes)?;
        Ok(Artifact {
          kind: self.opts.emit,
          path: path.clone(),
          stdout_hint: Some(format!("wrote {}", path.display())),
        })
      }
      EmitKind::Executable => {
        if !cfg!(target_os = "linux") {
          return Err(NativeJsError::UnsupportedPlatform {
            target_os: std::env::consts::OS.to_string(),
          });
        }

        let exe_path = self.output_path("")?;
        let _ = std::fs::remove_file(&exe_path);

        let obj =
          emit::emit_object_with_statepoints(module, target_config_from_opts(self.opts)).map_err(|e| {
            NativeJsError::Llvm(e.to_string())
          })?;

        let mut tmp_obj: Option<TempDir> = None;
        let keep_obj = self.opts.debug;
        let obj_path = if keep_obj {
          path_with_suffix(&exe_path, ".o")
        } else {
          let dir = TempDir::new().map_err(NativeJsError::TempDirCreateFailed)?;
          let obj_path = dir.path().join("out.o");
          tmp_obj = Some(dir);
          obj_path
        };

        write_file(&obj_path, &obj)?;
        if self.opts.debug {
          let ll_path = path_with_suffix(&exe_path, ".ll");
          let ir = emit::emit_llvm_ir(module);
          write_file(&ll_path, ir.as_bytes())?;
        }

        link::link_elf_executable_with_options(
          &exe_path,
          &[obj_path.clone()],
          link::LinkOpts {
            debug: self.opts.debug,
            ..Default::default()
          },
        )
        .map_err(|err| NativeJsError::Internal(err.to_string()))?;
        drop(tmp_obj);

        #[cfg(unix)]
        {
          use std::os::unix::fs::PermissionsExt;
          let meta = std::fs::metadata(&exe_path).map_err(|source| NativeJsError::Io {
            path: exe_path.clone(),
            source,
          })?;
          let mut perms = meta.permissions();
          perms.set_mode(perms.mode() | 0o111);
          std::fs::set_permissions(&exe_path, perms).map_err(|source| NativeJsError::Io {
            path: exe_path.clone(),
            source,
          })?;
        }

        Ok(Artifact {
          kind: self.opts.emit,
          path: exe_path.clone(),
          stdout_hint: Some(format!("wrote {}", exe_path.display())),
        })
      }
    }
  }

  fn emit_extra_llvm_ir<'ctx>(&self, module: &Module<'ctx>) -> Result<(), NativeJsError> {
    let Some(path) = self.opts.emit_ir.as_deref() else {
      return Ok(());
    };
    ensure_parent_dir(path)?;
    let ir = emit::emit_llvm_ir(module);
    write_file(path, ir.as_bytes())?;
    Ok(())
  }

  fn output_path(&self, suffix: &str) -> Result<PathBuf, NativeJsError> {
    if let Some(path) = self.opts.output.clone() {
      if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
          std::fs::create_dir_all(parent).map_err(|err| NativeJsError::Io {
            path: parent.to_path_buf(),
            source: err,
          })?;
        }
      }
      return Ok(path);
    }

    let tmp = tempfile::Builder::new()
      .prefix("native-js-")
      .suffix(suffix)
      .tempfile()
      .map_err(NativeJsError::TempFileCreateFailed)?;
    let (mut file, path) = tmp.keep()?;

    // Ensure the file exists and is writable, even if the caller only ends up
    // writing via `std::fs::write`.
    file
      .flush()
      .map_err(NativeJsError::TempFileCreateFailed)?;

    Ok(path)
  }
}

fn ensure_parent_dir(path: &Path) -> Result<(), NativeJsError> {
  if let Some(parent) = path.parent() {
    if !parent.as_os_str().is_empty() {
      std::fs::create_dir_all(parent).map_err(|source| NativeJsError::Io {
        path: parent.to_path_buf(),
        source,
      })?;
    }
  }
  Ok(())
}

struct LoadedProgram {
  lowered: Arc<hir_js::LowerResult>,
  checked_bodies: BTreeMap<BodyId, Arc<BodyCheckResult>>,
  #[allow(dead_code)]
  entrypoint: crate::strict::Entrypoint,
}

impl LoadedProgram {
  #[allow(dead_code)]
  fn hir_body(&self, id: BodyId) -> Option<&hir_js::Body> {
    self.lowered.body(id)
  }

  #[allow(dead_code)]
  fn body_check(&self, id: BodyId) -> Option<&Arc<BodyCheckResult>> {
    self.checked_bodies.get(&id)
  }

  #[allow(dead_code)]
  fn body_with_types(&self, id: BodyId) -> Option<BodyWithTypes<'_>> {
    let hir = self.lowered.body(id)?;
    let types = self.checked_bodies.get(&id)?.clone();
    Some(BodyWithTypes { id, hir, types })
  }
}

#[allow(dead_code)]
struct BodyWithTypes<'a> {
  id: BodyId,
  hir: &'a hir_js::Body,
  types: Arc<BodyCheckResult>,
}
