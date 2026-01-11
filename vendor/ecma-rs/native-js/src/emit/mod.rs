//! Emission of compiler artifacts (LLVM IR, assembly, object files, etc).
//!
//! This module hosts the small "emit" surface used by `native-js-cli` and tests:
//!
//! - textual LLVM IR (`emit_llvm_ir`)
//! - LLVM bitcode (`emit_bitcode`)
//! - object files (`emit_object`)
//! - assembly (`emit_asm`)
//!
//! For GC bring-up, it also includes a helper to run the statepoint rewrite pass
//! and write an object file (`write_object_file`), which is used by the
//! `.llvm_stackmaps` regression tests.

use crate::llvm::passes;
use inkwell::module::Module;
use inkwell::targets::{
  CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;
use llvm_sys::support::LLVMParseCommandLineOptions;
use std::ffi::CString;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::Once;

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
  #[error(transparent)]
  Pass(#[from] passes::PassError),

  #[error("failed to create LLVM target machine for {triple}: {message}")]
  TargetMachine { triple: String, message: String },

  #[error("failed to emit LLVM module as {file_type:?} to {path}: {message}")]
  Codegen {
    file_type: FileType,
    path: PathBuf,
    message: String,
  },

  #[error("failed to emit LLVM module as {file_type:?}: {message}")]
  CodegenBuffer {
    file_type: FileType,
    message: String,
  },
}

/// Runs the native-js LLVM pass pipeline and writes an object file.
///
/// This currently applies `rewrite-statepoints-for-gc` (and in debug builds
/// `verify<safepoint-ir>`) before invoking LLVM codegen. The rewrite pass is what
/// causes LLVM to emit `llvm.experimental.gc.statepoint.*` intrinsics and, during
/// object emission, a `.llvm_stackmaps` section.
pub fn write_object_file(
  module: &Module<'_>,
  target_machine: &TargetMachine,
  path: &Path,
) -> Result<(), EmitError> {
  ensure_targets_initialized();
  passes::rewrite_statepoints_for_gc(module, target_machine)?;

  target_machine
    .write_to_file(module, FileType::Object, path)
    .map_err(|err| EmitError::Codegen {
      file_type: FileType::Object,
      path: path.to_path_buf(),
      message: err.to_string(),
    })?;

  Ok(())
}

static TARGETS_INITIALIZED: Once = Once::new();

fn ensure_targets_initialized() {
  TARGETS_INITIALIZED.call_once(|| {
    // LLVM can legally keep `gc.statepoint` roots in callee-saved registers and
    // describe them as `Register` locations in `.llvm_stackmaps`. Our runtime's
    // initial stack walking strategy is frame-pointer-only and does not use
    // unwind-based register reconstruction, so we must force spills.
    //
    // Equivalent to:
    //   llc-18 --fixup-max-csr-statepoints=0
    //
    // `LLVMParseCommandLineOptions` configures global codegen flags for this
    // process; do it once before any TargetMachine emits code.
    let argv = [
      CString::new("native-js").expect("argv[0]"),
      CString::new("--fixup-max-csr-statepoints=0").expect("argv[1]"),
    ];
    let argv_ptrs: Vec<*const c_char> = argv.iter().map(|s| s.as_ptr()).collect();
    unsafe {
      LLVMParseCommandLineOptions(argv_ptrs.len() as i32, argv_ptrs.as_ptr(), std::ptr::null());
    }

    // `initialize_native` registers the host target (Linux x86_64 on the agent
    // machines) and enables code generation.
    Target::initialize_native(&InitializationConfig::default())
      .expect("failed to initialize native LLVM target");
  });
}

#[derive(Debug)]
pub struct TargetConfig {
  pub triple: TargetTriple,
  pub cpu: String,
  pub features: String,
  pub opt_level: OptimizationLevel,
  pub reloc_mode: RelocMode,
  pub code_model: CodeModel,
}

impl Default for TargetConfig {
  fn default() -> Self {
    ensure_targets_initialized();
    TargetConfig {
      triple: TargetMachine::get_default_triple(),
      cpu: TargetMachine::get_host_cpu_name().to_string(),
      features: TargetMachine::get_host_cpu_features().to_string(),
      opt_level: OptimizationLevel::Default,
      reloc_mode: RelocMode::Default,
      code_model: CodeModel::Default,
    }
  }
}

pub fn emit_llvm_ir(module: &Module<'_>) -> String {
  module.print_to_string().to_string()
}

pub fn emit_bitcode(module: &Module<'_>) -> Vec<u8> {
  module.write_bitcode_to_memory().as_slice().to_vec()
}

pub fn emit_object(module: &Module<'_>, target: TargetConfig) -> Vec<u8> {
  ensure_targets_initialized();

  let target_ref = Target::from_triple(&target.triple).unwrap_or_else(|err| {
    panic!(
      "failed to resolve LLVM target from triple {}: {err}",
      target.triple
    )
  });
  let machine = target_ref
    .create_target_machine(
      &target.triple,
      &target.cpu,
      &target.features,
      target.opt_level,
      target.reloc_mode,
      target.code_model,
    )
    .unwrap_or_else(|| panic!("failed to create LLVM target machine for {}", target.triple));

  module.set_triple(&target.triple);
  module.set_data_layout(&machine.get_target_data().get_data_layout());

  machine
    .write_to_memory_buffer(module, FileType::Object)
    .expect("failed to emit object file")
    .as_slice()
    .to_vec()
}

pub fn emit_object_with_statepoints(
  module: &Module<'_>,
  target: TargetConfig,
) -> Result<Vec<u8>, EmitError> {
  ensure_targets_initialized();

  let target_ref = Target::from_triple(&target.triple).map_err(|err| EmitError::TargetMachine {
    triple: target.triple.to_string(),
    message: err.to_string(),
  })?;
  let machine = target_ref
    .create_target_machine(
      &target.triple,
      &target.cpu,
      &target.features,
      target.opt_level,
      target.reloc_mode,
      target.code_model,
    )
    .ok_or_else(|| EmitError::TargetMachine {
      triple: target.triple.to_string(),
      message: "failed to create LLVM TargetMachine".to_string(),
    })?;

  module.set_triple(&target.triple);
  module.set_data_layout(&machine.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(module, &machine)?;

  Ok(
    machine
      .write_to_memory_buffer(module, FileType::Object)
      .map_err(|e| EmitError::CodegenBuffer {
        file_type: FileType::Object,
        message: e.to_string(),
      })?
      .as_slice()
      .to_vec(),
  )
}

pub fn emit_asm(module: &Module<'_>, target: TargetConfig) -> Vec<u8> {
  ensure_targets_initialized();

  let target_ref =
    Target::from_triple(&target.triple).expect("failed to resolve LLVM target from triple");
  let machine = target_ref
    .create_target_machine(
      &target.triple,
      &target.cpu,
      &target.features,
      target.opt_level,
      target.reloc_mode,
      target.code_model,
    )
    .expect("failed to create LLVM target machine");

  module.set_triple(&target.triple);
  module.set_data_layout(&machine.get_target_data().get_data_layout());

  machine
    .write_to_memory_buffer(module, FileType::Assembly)
    .expect("failed to emit assembly")
    .as_slice()
    .to_vec()
}

pub fn emit_asm_with_statepoints(
  module: &Module<'_>,
  target: TargetConfig,
) -> Result<Vec<u8>, EmitError> {
  ensure_targets_initialized();

  let target_ref = Target::from_triple(&target.triple).map_err(|err| EmitError::TargetMachine {
    triple: target.triple.to_string(),
    message: err.to_string(),
  })?;
  let machine = target_ref
    .create_target_machine(
      &target.triple,
      &target.cpu,
      &target.features,
      target.opt_level,
      target.reloc_mode,
      target.code_model,
    )
    .ok_or_else(|| EmitError::TargetMachine {
      triple: target.triple.to_string(),
      message: "failed to create LLVM TargetMachine".to_string(),
    })?;

  module.set_triple(&target.triple);
  module.set_data_layout(&machine.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(module, &machine)?;

  Ok(
    machine
      .write_to_memory_buffer(module, FileType::Assembly)
      .map_err(|e| EmitError::CodegenBuffer {
        file_type: FileType::Assembly,
        message: e.to_string(),
      })?
      .as_slice()
      .to_vec(),
  )
}
