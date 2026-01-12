//! Legacy `compiler::compile_entry_to_llvm_ir` helper.
//!
//! This module exists only to support old tests/debugging workflows. It is feature-gated and
//! deprecated in favor of the checked `strict::entrypoint + codegen::codegen` pipeline.
#![cfg(feature = "legacy-expr-backend")]

use crate::codes;
use crate::llvm::legacy_expr::{FunctionCodegen, FunctionSymbol, ValueKind};
use crate::llvm::LlvmBackend;
use crate::{CompileOptions, EmitKind, OptLevel};
use diagnostics::{Diagnostic, Severity, Span, TextRange};
use hir_js::{Body, BodyId, DefId, DefKind, FunctionData, NameId, PatKind};
use inkwell::context::Context;
use inkwell::types::BasicMetadataTypeEnum;
use inkwell::values::BasicValueEnum;
use std::collections::HashMap;
use typecheck_ts::{FileId, Program};

pub struct CompileResult {
  pub llvm_ir: Option<String>,
  pub diagnostics: Vec<Diagnostic>,
}

/// Test helper: compile the given file's exported entry function into LLVM IR.
///
/// Deprecated: prefer `strict::entrypoint + codegen::codegen`.
#[deprecated(note = "deprecated: prefer strict::entrypoint + codegen::codegen")]
pub fn compile_entry_to_llvm_ir(program: &Program, file: FileId, entry_export: &str) -> CompileResult {
  let mut diagnostics = Vec::new();

  // Ensure analysis/typechecking has run far enough that exports + HIR are available.
  let exports = program.exports_of(file);
  if exports
    .get(entry_export)
    .and_then(|entry| entry.def.or_else(|| program.symbol_info(entry.symbol).and_then(|info| info.def)))
    .is_none()
  {
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
    let Some(symbol) = declare_function(program, &mut diagnostics, &mut backend, def_id, body_id, body) else {
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

