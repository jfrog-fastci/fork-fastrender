//! Compilation entry points.
//!
//! `native-js` currently has two (partially overlapping) pipelines:
//! - A HIR/typecheck-driven path used by the native compiler driver (strict subset validation +
//!   LLVM lowering).
//! - A small `parse-js`-driven path used by early smoke tests and debugging tools, which can emit
//!   runnable artifacts by turning generated LLVM IR into an object file and linking it with the
//!   system toolchain.

use crate::emit::TargetConfig;
use crate::{compile_typescript_to_llvm_ir, emit, link, CompileOptions, EmitKind, NativeJsError, OptLevel};
use diagnostics::{Diagnostic, Severity};
use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::OptimizationLevel;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use typecheck_ts::{FileKey, Host, Program};

use crate::validate::validate_strict_subset;

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
          let obj = emit::emit_object(&module, target);
          write_file(&out_path, &obj)?;
        }
        EmitKind::Assembly => {
          let asm = emit::emit_asm(&module, target);
          write_file(&out_path, &asm)?;
        }
        EmitKind::Executable => {
          let obj = emit::emit_object(&module, target);

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
