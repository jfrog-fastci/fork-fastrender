//! Emission of compiler artifacts (LLVM IR, assembly, object files, etc).

use inkwell::module::Module;
use inkwell::targets::{
  CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;
use std::sync::Once;

static TARGETS_INITIALIZED: Once = Once::new();

fn ensure_targets_initialized() {
  TARGETS_INITIALIZED.call_once(|| {
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
    .write_to_memory_buffer(module, FileType::Object)
    .expect("failed to emit object file")
    .as_slice()
    .to_vec()
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
