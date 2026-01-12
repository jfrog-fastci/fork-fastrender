//! LLVM integration helpers for `native-js`.
//!
//! The native backend uses LLVM's *statepoint* infrastructure to support a
//! moving GC. On LLVM 18, the "manual" path (constructing `gc.statepoint`
//! intrinsics directly) is easy to get wrong:
//!
//! - Intrinsic signatures contain `immarg` parameters and require the callee
//!   argument to be annotated with `elementtype(<fn-ty>)`.
//! - Manually-built statepoints require extra trailing `i32 0, i32 0`
//!   transition/flags fields.
//!
//! Instead of computing liveness and constructing statepoints in Rust, we rely
//! on LLVM's `rewrite-statepoints-for-gc` pass to:
//! - rewrite plain calls into `llvm.experimental.gc.statepoint.*`
//! - attach the required `"gc-live"` operand bundle
//! - insert `llvm.experimental.gc.relocate.*` / `gc.result.*` and rewrite uses

use std::path::Path;
use std::sync::OnceLock;
use std::{ffi::CString, os::raw::c_char};

use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{
  CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::values::FunctionValue;
use inkwell::{module::Linkage, OptimizationLevel};
use llvm_sys::support::LLVMParseCommandLineOptions;
use target_lexicon::Triple;

#[cfg(feature = "legacy-expr-backend")]
pub mod legacy_expr;
pub mod gc;
pub mod gc_lint;
pub mod passes;
pub mod statepoint_directives;
pub mod statepoints;
pub mod types;

pub use gc_lint::{lint_module_gc_pointer_discipline, LintError, LintRule, LintViolation};
pub use types::{classify_type, llvm_type, NativeType};

/// Run the GC pointer discipline lint in debug builds/tests.
///
/// In release builds this is a no-op unless the `gc-lint` feature is enabled.
#[cfg(any(debug_assertions, feature = "gc-lint"))]
pub fn debug_lint_module_gc_pointer_discipline(module: &Module<'_>) -> Result<(), LintError> {
  lint_module_gc_pointer_discipline(module)
}

/// Release builds omit GC IR lint by default for compile-time performance.
#[cfg(not(any(debug_assertions, feature = "gc-lint")))]
pub fn debug_lint_module_gc_pointer_discipline(_module: &Module<'_>) -> Result<(), LintError> {
  Ok(())
}

/// Apply the target triple + data layout from `target_machine` onto `module`.
///
/// When linking LLVM bitcode with `clang -flto`, missing or mismatched target
/// information produces warnings and can lead to incorrect codegen.
pub fn apply_target_machine(module: &Module<'_>, target_machine: &TargetMachine) {
  module.set_triple(&target_machine.get_triple());
  module.set_data_layout(&target_machine.get_target_data().get_data_layout());
}

/// Emit LLVM bitcode into memory.
pub fn emit_bitcode(module: &Module<'_>, target_machine: &TargetMachine) -> Vec<u8> {
  apply_target_machine(module, target_machine);
  module.write_bitcode_to_memory().as_slice().to_vec()
}

/// Emit an object file into memory.
pub fn emit_object(module: &Module<'_>, target_machine: &TargetMachine) -> Vec<u8> {
  apply_target_machine(module, target_machine);
  target_machine
    .write_to_memory_buffer(module, FileType::Object)
    .expect("write object")
    .as_slice()
    .to_vec()
}

static LLVM_NATIVE_TARGET_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Initialize LLVM's native target machinery (targets, asm printer/parser, etc).
///
/// This is a global one-time operation; calling it multiple times is safe.
pub fn init_native_target() -> Result<(), String> {
  LLVM_NATIVE_TARGET_INIT
    .get_or_init(|| {
      // LLVM can legally keep `gc.statepoint` roots in callee-saved registers and
      // describe them as `Register` locations in `.llvm_stackmaps`. Our runtime's
      // initial stack walking strategy is frame-pointer-only and does not use
      // unwind-based register reconstruction, so we must force spills.
      //
      // Equivalent to:
      //   llc-18 --fixup-allow-gcptr-in-csr=false
      //   llc-18 --fixup-max-csr-statepoints=0
      //
      // `LLVMParseCommandLineOptions` configures global codegen flags for this
      // process; do it once before any TargetMachine emits code.
      let argv = [
        CString::new("native-js").expect("argv[0]"),
        // Preferred: disallow GC pointers in callee-saved registers entirely.
        CString::new("--fixup-allow-gcptr-in-csr=false").expect("argv[1]"),
        // Fallback/defense-in-depth: allow at most 0 statepoints to keep GC pointers in CSRs.
        CString::new("--fixup-max-csr-statepoints=0").expect("argv[2]"),
        // Always poll at loop backedges, even for "counted" loops with statically-known trip
        // counts. Without this, a call-free counted loop can run for a long time after the entry
        // poll and delay a stop-the-world GC request.
        CString::new("--spp-all-backedges").expect("argv[3]"),
      ];
      let argv_ptrs: Vec<*const c_char> = argv.iter().map(|s| s.as_ptr()).collect();
      unsafe {
        LLVMParseCommandLineOptions(argv_ptrs.len() as i32, argv_ptrs.as_ptr(), std::ptr::null());
      }

      Target::initialize_native(&InitializationConfig::default())
        .map_err(|e| format!("failed to initialize native LLVM target: {e}"))
    })
    .clone()
}

fn to_machine_opt_level(level: crate::OptLevel) -> OptimizationLevel {
  match level {
    crate::OptLevel::O0 => OptimizationLevel::None,
    crate::OptLevel::O1 => OptimizationLevel::Less,
    crate::OptLevel::O2 => OptimizationLevel::Default,
    crate::OptLevel::O3 => OptimizationLevel::Aggressive,
    // LLVM's inkwell wrapper doesn't expose size-optimizing levels here; fall
    // back to `Default`.
    crate::OptLevel::Os | crate::OptLevel::Oz => OptimizationLevel::Default,
  }
}

fn to_pass_pipeline(level: crate::OptLevel) -> &'static str {
  match level {
    crate::OptLevel::O0 => "default<O0>",
    crate::OptLevel::O1 => "default<O1>",
    crate::OptLevel::O2 => "default<O2>",
    crate::OptLevel::O3 => "default<O3>",
    crate::OptLevel::Os => "default<Os>",
    crate::OptLevel::Oz => "default<Oz>",
  }
}

/// Wrapper around an LLVM module + builder wired up for a specific target.
pub struct LlvmBackend<'ctx> {
  pub context: &'ctx Context,
  pub module: Module<'ctx>,
  pub builder: Builder<'ctx>,
  pub target_machine: TargetMachine,
  pub target_triple: TargetTriple,
}

impl<'ctx> LlvmBackend<'ctx> {
  /// Create a new LLVM backend with module target triple + data layout set based
  /// on the host (or an override in `options.target`).
  pub fn new(
    context: &'ctx Context,
    module_name: &str,
    options: &crate::CompileOptions,
  ) -> Result<Self, String> {
    init_native_target()?;

    let module = context.create_module(module_name);
    let builder = context.create_builder();

    let target_triple = match options.target.as_ref() {
      Some(triple) => TargetTriple::create(&triple.to_string()),
      None => TargetMachine::get_default_triple(),
    };

    let target = Target::from_triple(&target_triple)
      .map_err(|e| format!("failed to select LLVM target for triple: {e}"))?;

    let cpu = TargetMachine::get_host_cpu_name().to_string();
    let features = TargetMachine::get_host_cpu_features().to_string();

    let target_machine = target
      .create_target_machine(
        &target_triple,
        &cpu,
        &features,
        to_machine_opt_level(options.opt_level),
        RelocMode::Default,
        CodeModel::Default,
      )
      .ok_or_else(|| "failed to create LLVM target machine".to_string())?;

    module.set_triple(&target_triple);
    module.set_data_layout(&target_machine.get_target_data().get_data_layout());

    Ok(Self {
      context,
      module,
      builder,
      target_machine,
      target_triple,
    })
  }

  /// Add a function to the module.
  pub fn add_function(
    &self,
    name: &str,
    ty: inkwell::types::FunctionType<'ctx>,
    linkage: Option<Linkage>,
  ) -> FunctionValue<'ctx> {
    self.module.add_function(name, ty, linkage)
  }

  /// Append a basic block to a function.
  pub fn append_basic_block(&self, function: FunctionValue<'ctx>, name: &str) -> BasicBlock<'ctx> {
    self.context.append_basic_block(function, name)
  }

  /// Verify the module.
  pub fn verify(&self) -> Result<(), String> {
    self
      .module
      .verify()
      .map_err(|e| format!("invalid LLVM IR: {e}"))
  }

  /// Run an LLVM optimization pipeline on the module.
  pub fn optimize_module(&self, opt_level: crate::OptLevel) -> Result<(), String> {
    let options = PassBuilderOptions::create();
    self
      .module
      .run_passes(to_pass_pipeline(opt_level), &self.target_machine, options)
      .map_err(|e| format!("failed to run LLVM optimization passes: {e}"))?;

    Ok(())
  }

  /// Emit LLVM textual IR to `path`.
  pub fn emit_llvm_ir(&self, path: impl AsRef<Path>) -> Result<(), String> {
    self
      .module
      .print_to_file(path.as_ref())
      .map_err(|e| format!("failed to write LLVM IR: {e}"))
  }

  /// Emit a native object file to `path`.
  pub fn emit_object(&self, path: impl AsRef<Path>) -> Result<(), String> {
    self
      .target_machine
      .write_to_file(&self.module, FileType::Object, path.as_ref())
      .map_err(|e| format!("failed to write object file: {e}"))
  }
}

/// Helper to convert a `target-lexicon` triple to an LLVM `TargetTriple`.
///
/// This is currently only used by parts of `native-js` that accept
/// `target_lexicon::Triple` in public APIs.
pub fn target_triple_from_lexicon(triple: &Triple) -> TargetTriple {
  TargetTriple::create(&triple.to_string())
}
