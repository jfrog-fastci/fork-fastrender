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
use crate::resolve::Resolver;
use crate::{
  compile_typescript_to_llvm_ir, emit, link, Artifact, CompileOptions, CompilerOptions, EmitKind,
  NativeJsError, OptLevel,
};
use diagnostics::{Diagnostic, Severity, Span, TextRange};
use hir_js::{BodyId, ExprKind, FileKind};
use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::module::Module;
use inkwell::OptimizationLevel;
use std::collections::BTreeMap;
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

          let runtime_native_a = require_runtime_native_staticlib()?;

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

          let _ = std::fs::remove_file(&out_path);
          link::link_elf_executable_with_options_and_static_libs(
            &out_path,
            &[obj_path.clone()],
            link::LinkOpts {
              pie: opts.pie,
              debug: opts.debug,
              ..Default::default()
            },
            std::slice::from_ref(&runtime_native_a),
          )
          .map_err(|err| NativeJsError::Internal(err.to_string()))?;

          #[cfg(unix)]
          {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&out_path).map_err(|source| NativeJsError::Io {
              path: out_path.clone(),
              source,
            })?;
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o111);
            std::fs::set_permissions(&out_path, perms).map_err(|source| NativeJsError::Io {
              path: out_path.clone(),
              source,
            })?;
          }
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

  // PIE requires position-independent code generation, otherwise lld will reject
  // absolute relocations such as `R_X86_64_32`.
  if opts.pie {
    cfg.reloc_mode = inkwell::targets::RelocMode::PIC;
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

fn require_runtime_native_staticlib() -> Result<PathBuf, NativeJsError> {
  let Some(p) = link::find_runtime_native_staticlib() else {
    return Err(NativeJsError::RuntimeNativeNotFound {
      message:
        "unable to locate runtime-native static library `libruntime_native.a` required for native executable linking; \
set NATIVE_JS_RUNTIME_NATIVE_A=/path/to/libruntime_native.a to override discovery"
          .to_string(),
    });
  };

  if p.is_file() {
    return Ok(p);
  }

  // `find_runtime_native_staticlib` treats `NATIVE_JS_RUNTIME_NATIVE_A` as an explicit override;
  // if set, a missing file is almost certainly a misconfiguration and warrants a dedicated error.
  if std::env::var_os("NATIVE_JS_RUNTIME_NATIVE_A").is_some() {
    return Err(NativeJsError::RuntimeNativeNotFound {
      message: format!(
        "NATIVE_JS_RUNTIME_NATIVE_A was set to {}, but that file does not exist; \
set it to the path of `libruntime_native.a`",
        p.display()
      ),
    });
  }

  Err(NativeJsError::RuntimeNativeNotFound {
    message: format!(
      "unable to locate runtime-native static library at {}; \
set NATIVE_JS_RUNTIME_NATIVE_A=/path/to/libruntime_native.a to override discovery",
      p.display()
    ),
  })
}
#[cfg(feature = "legacy-expr-backend")]
mod legacy_expr;
#[cfg(feature = "legacy-expr-backend")]
#[allow(deprecated)]
pub use legacy_expr::*;

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

    // Locate the exported `main()` entrypoint and validate its shape.
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

    let resolver = Resolver::new(self.program);

    let mut diagnostics = Vec::new();
    for file in self.program.reachable_files() {
      let Some(lowered) = self.program.hir_lowered(file) else {
        continue;
      };
      if matches!(lowered.hir.file_kind, FileKind::Dts) {
        continue;
      }
      let file_resolver = resolver.for_file(file);

      for body_id in self.program.bodies_in_file(file) {
        let Some(body) = lowered.body(body_id) else {
          continue;
        };
        for expr in body.exprs.iter() {
          let ExprKind::Call(call) = &expr.kind else {
            continue;
          };
          let Some(callee_expr) = body.exprs.get(call.callee.0 as usize) else {
            continue;
          };
          let ExprKind::Ident(ident) = &callee_expr.kind else {
            continue;
          };
          let Some(name) = lowered.names.resolve(*ident) else {
            continue;
          };
          let Some(intrinsic) = crate::builtins::intrinsic_by_name(name) else {
            continue;
          };
          // `typecheck-ts` symbol occurrences only cover file-local bindings; global names (coming
          // from injected `.d.ts` libs) resolve to `None` here. If the identifier resolves to any
          // file-local binding, treat it as a user-defined function and do not consider it an
          // intrinsic.
          if file_resolver.resolve_expr_ident(body, call.callee).is_some() {
            continue;
          }

          diagnostics.push(
            codes::BUILTINS_DISABLED
              .error(
                format!(
                  "`{}(...)` intrinsic is disabled because builtin intrinsics are disabled",
                  intrinsic.name()
                ),
                Span::new(file, expr.span),
              )
              .with_note("re-enable builtin intrinsics by setting `CompilerOptions.builtins = true`"),
          );
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

        let runtime_native_a = require_runtime_native_staticlib()?;

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

        link::link_elf_executable_with_options_and_static_libs(
          &exe_path,
          &[obj_path.clone()],
          link::LinkOpts {
            pie: self.opts.pie,
            debug: self.opts.debug,
            ..Default::default()
          },
          std::slice::from_ref(&runtime_native_a),
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
