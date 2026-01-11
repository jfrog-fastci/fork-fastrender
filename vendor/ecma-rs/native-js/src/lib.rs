//! Native (LLVM-backed) code generation for `ecma-rs`.
//!
//! This crate is still early: most of the real TS/HIR lowering is not implemented yet.
//!
//! It currently contains:
//! - A strict TypeScript subset validator + HIR-driven LLVM codegen used by the `native-js` binary.
//! - A tiny `parse-js`-driven LLVM IR emitter used by `native-js-cli` integration tests and for
//!   debugging the native pipeline.
//!
//! ## GC stack walking
//! The native runtime performs **precise GC** using LLVM statepoints. In addition to stack maps,
//! the runtime must be able to walk frames and recover return addresses deterministically.
//!
//! We currently enforce a simple, robust invariant: generated functions always keep frame pointers
//! and never participate in tail-call optimization. See `docs/gc_stack_walking.md`.
//!
//! ## `.llvm_stackmaps` discovery
//!
//! LLVM's statepoint/stackmap infrastructure emits a `.llvm_stackmaps` section in the final ELF.
//! That section is needed by the native runtime's GC to locate safepoints, but LLVM's own
//! `__LLVM_StackMaps` symbol is `STB_LOCAL` (not linkable from other objects).
//!
//! The native-js link pipeline therefore exports two **global** symbols that delimit the in-memory
//! stackmap blob:
//!
//! - [`link::FASTR_STACKMAPS_START_SYM`]
//! - [`link::FASTR_STACKMAPS_END_SYM`]
//!
//! Runtime usage (Rust):
//!
//! ```ignore
//! extern "C" {
//!   static __fastr_stackmaps_start: u8;
//!   static __fastr_stackmaps_end: u8;
//! }
//!
//! let ptr = unsafe { &__fastr_stackmaps_start as *const u8 };
//! let len = unsafe { (&__fastr_stackmaps_end as *const u8).offset_from(ptr) as usize };
//! let stackmaps = unsafe { std::slice::from_raw_parts(ptr, len) };
//! ```

pub mod compiler;
pub mod codegen;
pub mod codes;
pub mod emit;
pub mod gc;
pub mod link;
pub mod llvm;
pub mod poc;
pub mod runtime_abi;
pub mod poc_stackmaps;
pub mod strict;
pub mod llvm_passes;
pub mod validate;

mod stack_walking;
pub use stack_walking::CodeGen;

use llvm_sys as _;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use target_lexicon::Triple;

/// Optimization level to apply during compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OptLevel {
  O0,
  O1,
  O2,
  O3,
  Os,
  Oz,
}

/// Which artifact to emit from the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EmitKind {
  /// Emit LLVM IR (`.ll`) for debugging.
  LlvmIr,
  /// Emit an object file (`.o`).
  Object,
  /// Emit assembly (`.s`).
  Assembly,
}

/// Options controlling native compilation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CompileOptions {
  /// The optimization level to use for codegen.
  pub opt_level: OptLevel,
  /// What kind of artifact to emit.
  pub emit: EmitKind,
  /// Target triple to compile for. `None` means "host default".
  pub target: Option<Triple>,
  /// Whether to emit debug info.
  pub debug: bool,
  /// If true, recognize and lower small builtin APIs such as `console.log` and `assert`.
  pub builtins: bool,
}

impl Default for CompileOptions {
  fn default() -> Self {
    Self {
      opt_level: OptLevel::O2,
      emit: EmitKind::Object,
      target: None,
      debug: false,
      builtins: true,
    }
  }
}

/// Entry-point type for native compilation.
#[derive(Debug)]
pub struct Compiler {
  options: CompileOptions,
}

impl Compiler {
  pub fn new(options: CompileOptions) -> Self {
    Self { options }
  }

  pub fn options(&self) -> &CompileOptions {
    &self.options
  }

  /// Compile a program using the configured [`CompileOptions`].
  ///
  /// Note: TS/HIR lowering is not implemented yet.
  pub fn compile(&self) -> Result<(), NativeJsError> {
    Err(NativeJsError::Unimplemented)
  }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NativeJsError {
  #[error(transparent)]
  Parse(#[from] parse_js::error::SyntaxError),

  #[error(transparent)]
  Codegen(#[from] codegen::CodegenError),

  #[error("native-js codegen is not implemented yet")]
  Unimplemented,

  #[error("LLVM error: {0}")]
  Llvm(String),
}

pub fn compile_typescript_to_llvm_ir(
  source: &str,
  opts: CompileOptions,
) -> Result<String, NativeJsError> {
  let ast = parse_with_options(
    source,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  )?;
  Ok(codegen::emit_llvm_module(&ast, opts)?)
}

#[cfg(test)]
mod tests {
  use inkwell::context::Context;

  #[test]
  fn llvm_ir_sanity() {
    let context = Context::create();
    let module = context.create_module("native_js_sanity");
    let builder = context.create_builder();

    let i32_type = context.i32_type();
    let fn_type = i32_type.fn_type(&[i32_type.into(), i32_type.into()], false);
    let function = module.add_function("add", fn_type, None);

    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let a = function
      .get_nth_param(0)
      .expect("param 0")
      .into_int_value();
    let b = function
      .get_nth_param(1)
      .expect("param 1")
      .into_int_value();

    let sum = builder.build_int_add(a, b, "sum").expect("build add");
    builder.build_return(Some(&sum)).expect("build ret");

    if let Err(err) = module.verify() {
      panic!(
        "LLVM module verification failed: {err}\n\nIR:\n{}",
        module.print_to_string()
      );
    }
  }
}
