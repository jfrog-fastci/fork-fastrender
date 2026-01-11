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
//!
//! Note: when linking multiple compilation units, `.llvm_stackmaps` is not guaranteed to contain a
//! single StackMap table. Object-file linking typically concatenates multiple StackMap v3 blobs
//! back-to-back, while full LTO (`clang -flto`) tends to emit one merged blob. Runtime parsers must
//! iterate `stackmaps[..]` and parse blobs until the end of the range. See `docs/stackmaps.md`.

pub mod compiler;
pub mod codegen;
pub mod codes;
pub mod emit;
pub mod eval;
pub mod gc;
pub mod link;
pub mod llvm;
pub mod poc;
pub mod poc_stackmaps;
mod project;
pub mod resolve;
pub mod runtime_abi;
pub mod runtime_fn;
pub mod stackmaps;
pub mod strict;
pub mod validate;

mod error;
mod stack_walking;

pub use error::NativeJsError;
pub use project::compile_project_to_llvm_ir;
pub use resolve::Resolver;
pub use stack_walking::CodeGen;

use llvm_sys as _;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::path::PathBuf;
use target_lexicon::Triple;
use typecheck_ts::{DefId, FileId, Program};

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
  /// Emit LLVM bitcode (`.bc`).
  Bitcode,
  /// Emit an object file (`.o`).
  Object,
  /// Emit assembly (`.s`).
  Assembly,
  /// Emit a runnable native executable (Linux only, for now).
  Executable,
}

/// Options controlling native compilation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CompilerOptions {
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
  /// Explicit output path. When `None`, a temp file is created for the chosen [`EmitKind`].
  pub output: Option<PathBuf>,
}

/// Backwards-compatible alias used by the parse-js based emitter.
pub type CompileOptions = CompilerOptions;

impl Default for CompilerOptions {
  fn default() -> Self {
    Self {
      opt_level: OptLevel::O2,
      emit: EmitKind::Object,
      target: None,
      debug: false,
      builtins: true,
      output: None,
    }
  }
}

/// A successful compilation result.
#[derive(Debug, Clone)]
pub struct Artifact {
  pub kind: EmitKind,
  pub path: PathBuf,
  /// Optional hint suitable for printing to stdout by CLIs.
  pub stdout_hint: Option<String>,
}

/// Compile a fully type-checked program starting from `entry`.
///
/// This is the "real" native compiler entry point: it consumes `typecheck-ts`'s checked HIR + type
/// tables, runs strict-subset validation, builds an LLVM module, and emits an artifact.
pub fn compile_program(
  program: &typecheck_ts::Program,
  entry: typecheck_ts::FileId,
  opts: &CompilerOptions,
) -> Result<Artifact, NativeJsError> {
  compiler::compile_program(program, entry, opts)
}

/// Parse and compile a single TypeScript module to LLVM IR.
///
/// This is a lightweight, `parse-js`-driven emitter that does *not* use `typecheck-ts`.
/// It exists mainly for debugging and for `native-js-cli` integration tests.
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

/// Create a stable LLVM symbol name for a definition.
///
/// The name is deterministic across runs and unique across files/scopes because
/// it includes the raw `DefId` (`u64`).
///
/// Format (stable, ASCII-only):
/// `__nativejs_def_<defid-hex>_<debug-name>`
pub fn llvm_symbol_for_def(program: &Program, def: DefId) -> String {
  let mut out = format!("__nativejs_def_{:016x}", def.0);

  if let Some(suffix) = debug_name_suffix_for_def(program, def) {
    out.push('_');
    out.push_str(&suffix);
  }

  out
}

/// Create a stable LLVM symbol name for a file/module initializer function.
pub fn llvm_symbol_for_file_init(file: FileId) -> String {
  format!("__nativejs_file_init_{:08x}", file.0)
}

fn debug_name_suffix_for_def(program: &Program, def: DefId) -> Option<String> {
  let lowered = program.hir_lowered(def.file())?;
  let idx = lowered.def_index.get(&def).copied()?;
  let data = lowered.defs.get(idx)?;
  let name = lowered.names.resolve(data.name).unwrap_or("");
  let sanitized = sanitize_symbol_suffix(name);
  (!sanitized.is_empty()).then_some(sanitized)
}

fn sanitize_symbol_suffix(name: &str) -> String {
  // Keep the suffix short and "C identifier-ish" so it can be used in LLVM
  // symbol names without quoting.
  const MAX_LEN: usize = 48;
  let mut out = String::new();
  for ch in name.chars() {
    if out.len() >= MAX_LEN {
      break;
    }
    match ch {
      'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '$' => out.push(ch),
      _ => out.push('_'),
    }
  }
  // Avoid leading digits (LLVM accepts it, but it is less portable across tools).
  if out.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
    out.insert(0, '_');
  }
  out
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
