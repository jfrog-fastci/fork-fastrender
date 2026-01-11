//! Compilation entry points.
//!
//! `native-js` currently has two (partially overlapping) pipelines:
//! - A HIR/typecheck-driven path used by the native compiler driver (strict subset validation +
//!   LLVM lowering).
//! - A small `parse-js`-driven path used by early smoke tests and debugging tools, which can emit
//!   runnable artifacts by turning generated LLVM IR into an object file and linking it with the
//!   system toolchain.

use crate::codes;
use crate::emit::TargetConfig;
use crate::llvm::expr::{FunctionCodegen, FunctionSymbol};
use crate::llvm::{LlvmBackend, ValueKind};
use crate::validate::validate_strict_subset;
use crate::{compile_typescript_to_llvm_ir, emit, link, CompileOptions, EmitKind, NativeJsError, OptLevel};
use diagnostics::{Diagnostic, Severity, Span, TextRange};
use hir_js::{
  Body, BodyId, DefId, DefKind, ExprId, FunctionBody, FunctionData, NameId, PatKind, StmtId, StmtKind,
};
use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::types::BasicType;
use inkwell::values::BasicValueEnum;
use inkwell::OptimizationLevel;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use typecheck_ts::{FileId, FileKey, Host, Program};

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
  if diagnostics.iter().any(|d| d.severity == Severity::Error) {
    return Err(diagnostics);
  }
  validate_strict_subset(&program)?;
  Ok(program)
}

pub struct CompileOutput {
  pub path: PathBuf,
  // Keep tempdir alive for as long as the output is needed.
  _tempdir: Option<TempDir>,
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
  let (out_path, out_tempdir) = resolve_output_path(opts.emit, opts.debug, output_path)?;

  let ir = compile_typescript_to_llvm_ir(source, opts.clone())?;

  match opts.emit {
    EmitKind::LlvmIr => {
      write_file(&out_path, ir.as_bytes())?;
      Ok(CompileOutput {
        path: out_path,
        _tempdir: out_tempdir,
      })
    }

    EmitKind::Object | EmitKind::Assembly | EmitKind::Executable => {
      let context = Context::create();
      let module = parse_ir(&context, &ir)?;

      module
        .verify()
        .map_err(|e| NativeJsError::Llvm(format!("module verification failed: {e}")))?;

      let target = target_config_from_opts(&opts);

      match opts.emit {
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
            write_file(&ll_path, ir.as_bytes())?;
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

fn parse_ir<'ctx>(context: &'ctx Context, ir: &str) -> Result<inkwell::module::Module<'ctx>, NativeJsError> {
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
      let default_ret: BasicValueEnum<'_> = match symbol.ret {
        ValueKind::Number => backend.f64_type().const_float(0.0).into(),
        ValueKind::Boolean => backend.bool_type().const_int(0, false).into(),
      };
      let _ = backend.builder.build_return(Some(&default_ret));
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
  name_id: NameId,
  body_id: BodyId,
  body: &Body,
) -> Option<FunctionSymbol<'ctx>> {
  let Some(func) = body.function.as_ref() else {
    return None;
  };

  let types = program.check_body(body_id);

  // Infer parameter kinds. Bail out if any parameter is unsupported: we can't
  // represent it in the current subset and calls won't be type-safe.
  let mut param_kinds = Vec::new();
  for param in func.params.iter() {
    let PatKind::Ident(_) = body.pats.get(param.pat.0 as usize)?.kind else {
      diagnostics.push(codes::UNSUPPORTED_EXPR.error(
        "unsupported parameter pattern",
        program
          .pat_span(body_id, param.pat)
          .unwrap_or(Span::new(body_id.file(), body.span)),
      ));
      return None;
    };
    let Some(ty) = types.pat_type(param.pat) else {
      diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        "missing type for parameter",
        program
          .pat_span(body_id, param.pat)
          .unwrap_or(Span::new(body_id.file(), body.span)),
      ));
      return None;
    };
    let kind = program.type_kind(ty);
    let Some(kind) = ValueKind::from_type_kind(&kind) else {
      diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!("unsupported parameter type: {kind:?}"),
        program
          .pat_span(body_id, param.pat)
          .unwrap_or(Span::new(body_id.file(), body.span)),
      ));
      return None;
    };
    param_kinds.push(kind);
  }

  // Infer return kind from the body.
  let ret_expr = return_expr(body, &func.body)?;
  let ret_ty = program.type_of_expr(body_id, ret_expr);
  let ret_kind_summary = program.type_kind(ret_ty);
  let ret_kind = match ValueKind::from_type_kind(&ret_kind_summary) {
    Some(kind) => kind,
    None => {
      // Keep compiling to surface expression-level diagnostics, but fall back to
      // `number` for the LLVM signature.
      diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!("unsupported return type: {ret_kind_summary:?}"),
        program
          .expr_span(body_id, ret_expr)
          .unwrap_or(Span::new(body_id.file(), body.span)),
      ));
      ValueKind::Number
    }
  };

  let ret_ty = backend.llvm_type(ret_kind);
  let param_tys: Vec<_> = param_kinds
    .iter()
    .copied()
    .map(|k| backend.llvm_type(k).into())
    .collect();
  let fn_type = ret_ty.fn_type(&param_tys, false);

  let base_name = program
    .hir_lowered(def_id.file())
    .and_then(|lowered| lowered.names.resolve(name_id).map(|s| s.to_string()))
    .unwrap_or_else(|| format!("fn_{:x}", def_id.0));
  let llvm_name = format!("{base_name}_{}", def_id.local());
  let function = backend.module.add_function(&llvm_name, fn_type, None);

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

fn return_expr(body: &Body, func_body: &FunctionBody) -> Option<ExprId> {
  match func_body {
    FunctionBody::Expr(expr) => Some(*expr),
    FunctionBody::Block(stmts) => find_return_expr_in_stmts(body, stmts),
  }
}

fn find_return_expr_in_stmts(body: &Body, stmts: &[StmtId]) -> Option<ExprId> {
  for stmt_id in stmts.iter().copied() {
    let stmt = body.stmts.get(stmt_id.0 as usize)?;
    match &stmt.kind {
      StmtKind::Return(Some(expr)) => return Some(*expr),
      StmtKind::Block(nested) => {
        if let Some(expr) = find_return_expr_in_stmts(body, nested) {
          return Some(expr);
        }
      }
      _ => {}
    }
  }
  None
}

