//! Native (LLVM-backed) code generation for `ecma-rs`.
//!
//! This crate is still early: most of the real TS/HIR lowering is not implemented yet.
//!
//! It currently contains:
//! - A strict TypeScript subset validator + HIR-driven LLVM codegen used by the typechecked
//!   pipeline (`native-js-cli --pipeline checked` and the `native-js` binary).
//! - A tiny `parse-js`-driven LLVM IR emitter used by `native-js-cli --pipeline project`, tests,
//!   and debugging the native pipeline.
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
//! - [`link::LLVM_STACKMAPS_START_SYM`] / [`link::LLVM_STACKMAPS_STOP_SYM`] (linker-script-defined
//!   `__start_llvm_stackmaps` / `__stop_llvm_stackmaps`)
//! - `__stackmaps_start` / `__stackmaps_end` (generic alias used by tooling)
//!
//! Runtime usage (Rust):
//!
//! ```ignore
//! extern "C" {
//!   static __stackmaps_start: u8;
//!   static __stackmaps_end: u8;
//! }
//!
//! let ptr = unsafe { &__stackmaps_start as *const u8 };
//! let len = unsafe { (&__stackmaps_end as *const u8).offset_from(ptr) as usize };
//! let stackmaps = unsafe { std::slice::from_raw_parts(ptr, len) };
//! ```
//!
//! Note: when linking multiple compilation units, `.llvm_stackmaps` is not guaranteed to contain a
//! single StackMap table. Object-file linking typically concatenates multiple StackMap v3 blobs
//! back-to-back, while full LTO (`clang -flto`) tends to emit one merged blob. Runtime parsers must
//! iterate `stackmaps[..]` and parse blobs until the end of the range. See `docs/stackmaps.md`.

#[cfg(feature = "link-runtime-native")]
extern crate runtime_native as _;

pub mod builtins;
pub mod backend_ssa;
pub mod codegen;
pub mod codes;
pub mod compiler;
pub(crate) mod array_abi;
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
pub mod strict;
pub mod tail_calls;
pub mod toolchain;
pub mod ts_ir;
pub mod validate;

mod error;
mod stack_walking;

pub use error::NativeJsError;
pub use toolchain::Toolchain;
pub use project::compile_project_to_llvm_ir;
pub use resolve::Resolver;
pub use stack_walking::CodeGen;

use diagnostics::Severity;
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

/// Which internal code generation backend to use for the typechecked pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendKind {
  /// Existing HIR-driven backend (`native-js/src/codegen`).
  Hir,
  /// New SSA/analysis-driven backend built on `optimize-js` CFG/IL.
  Ssa,
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
  /// Optional override for the external toolchain used for linking.
  ///
  /// When `None`, native-js will attempt to auto-detect tools in PATH (preferring LLVM 18 tools).
  ///
  /// Note: most compilation modes do not require a toolchain. Tools are only needed for link steps
  /// such as `EmitKind::Executable`.
  pub toolchain: Option<Toolchain>,
  /// Print the exact tool invocations used during linking (clang/llvm-objcopy).
  pub print_commands: bool,
  /// Keep intermediate build artifacts (temporary directories) and print their paths.
  pub keep_temp: bool,
  /// Whether to produce a PIE executable when [`CompilerOptions::emit`] is [`EmitKind::Executable`]
  /// (Linux only).
  ///
  /// By default `native-js` links non-PIE on Linux for stackmaps compatibility (see `link` module
  /// docs). When set, the linker will produce an `ET_DYN` PIE binary.
  pub pie: bool,
  /// If true, recognize and lower small builtin APIs such as `console.log` and `assert`.
  pub builtins: bool,
  /// Which code generation backend to use for the typechecked pipeline.
  pub backend: BackendKind,
  /// Explicit output path. When `None`, a temp file is created for the chosen [`EmitKind`].
  pub output: Option<PathBuf>,
  /// If set, also write the generated textual LLVM IR (`.ll`) to this path in addition to the
  /// primary artifact specified by [`CompilerOptions::emit`].
  pub emit_ir: Option<PathBuf>,
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
      toolchain: None,
      print_commands: false,
      keep_temp: false,
      pie: false,
      builtins: true,
      backend: BackendKind::Hir,
      output: None,
      emit_ir: None,
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

/// Backwards-compatible compilation output type.
///
/// Note: the native pipeline has grown beyond this initial shape. Prefer
/// [`Artifact`] and [`compile_program`] for the typechecked backend.
#[derive(Clone, Debug)]
pub struct CompilationOutput {
  /// Path to the produced artifact (executable/object file).
  pub artifact: PathBuf,
  /// Optional textual LLVM IR (when requested by the caller).
  pub llvm_ir: Option<String>,
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

/// Backwards-compatible entrypoint for the native compiler API.
///
/// The native compiler now requires an explicit entry file. For the typechecked
/// pipeline, use [`compile_program`]. For a project-based workflow, use
/// [`compile_project_to_llvm_ir`].
///
/// This wrapper attempts to infer the entry file when the [`Program`] has
/// **exactly one** configured root file (i.e. it was created with `roots.len()
/// == 1`). If the program has multiple roots, this returns
/// [`NativeJsError::UnsupportedFeature`] and callers should use
/// [`compile_program`] and pass an explicit [`FileId`].
///
/// The returned [`CompilationOutput::llvm_ir`] is populated only when
/// `options.emit == EmitKind::LlvmIr` or when `options.emit_ir` is set (in which
/// case the IR is read back from the `.ll` file written to `emit_ir`).
pub fn compile(
  program: &Program,
  options: &CompilerOptions,
) -> Result<CompilationOutput, NativeJsError> {
  let diagnostics = program.check();
  if diagnostics
    .iter()
    .any(|diag| diag.severity == Severity::Error)
  {
    return Err(NativeJsError::TypecheckFailed { diagnostics });
  }

  let roots = program.roots();
  let [entry_key] = roots else {
    let message = if roots.is_empty() {
      "native_js::compile requires a Program with exactly one root file (got 0 roots); \
use native_js::compile_program(program, entry, opts) and pass an explicit FileId"
        .to_string()
    } else {
      format!(
        "native_js::compile requires a Program with exactly one root file (got {} roots); \
use native_js::compile_program(program, entry, opts) and pass an explicit FileId",
        roots.len()
      )
    };
    return Err(NativeJsError::UnsupportedFeature(message));
  };

  let entry = program.file_id(entry_key).ok_or_else(|| {
    NativeJsError::UnsupportedFeature(format!(
      "native_js::compile failed to resolve root file `{}` to a FileId; \
use native_js::compile_program(program, entry, opts) and pass an explicit FileId",
      entry_key.as_str()
    ))
  })?;

  // Avoid running `Program::check()` a second time (compile_program also checks
  // by default).
  let artifact = compiler::compile_program_checked(program, entry, options)?;
  let llvm_ir = if options.emit == EmitKind::LlvmIr {
    let path = artifact.path.clone();
    Some(std::fs::read_to_string(&path).map_err(|source| NativeJsError::Io { path, source })?)
  } else if let Some(path) = options.emit_ir.as_ref() {
    Some(
      std::fs::read_to_string(path).map_err(|source| NativeJsError::Io {
        path: path.to_path_buf(),
        source,
      })?,
    )
  } else {
    None
  };

  Ok(CompilationOutput {
    artifact: artifact.path,
    llvm_ir,
  })
}

/// Parse and compile a single TypeScript module to LLVM IR.
///
/// This is a lightweight, `parse-js`-driven emitter that does *not* use `typecheck-ts`.
/// It exists mainly for debugging and for in-tree tests/examples.
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
  Ok(codegen::emit_llvm_module(&ast, source, opts)?)
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

    let a = function.get_nth_param(0).expect("param 0").into_int_value();
    let b = function.get_nth_param(1).expect("param 1").into_int_value();

    let sum = builder.build_int_add(a, b, "sum").expect("build add");
    builder.build_return(Some(&sum)).expect("build ret");

    if let Err(err) = module.verify() {
      panic!(
        "LLVM module verification failed: {err}\n\nIR:\n{}",
        module.print_to_string()
      );
    }
  }

  #[test]
  fn compile_single_root_program() {
    use crate::llvm_symbol_for_def;
    use crate::{compile, CompilerOptions, EmitKind};
    use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
    use typecheck_ts::{FileKey, MemoryHost, Program};

    let mut host = MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
      ..Default::default()
    });
    let key = FileKey::new("main.ts");
    host.insert(
      key.clone(),
      r#"
export function main() {
  return 1 + 2;
}
"#,
    );

    let program = Program::new(host, vec![key.clone()]);
    let diags = program.check();
    assert!(
      diags.iter().all(|d| d.severity != super::Severity::Error),
      "expected sample to typecheck cleanly, got: {diags:#?}"
    );

    let file = program.file_id(&key).expect("file id");
    let def = program
      .exports_of(file)
      .get("main")
      .and_then(|e| e.def)
      .expect("exported def for `main`");
    let expected_symbol = llvm_symbol_for_def(&program, def);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let out_path = tmp.path().join("out.ll");

    let mut opts = CompilerOptions::default();
    opts.emit = EmitKind::LlvmIr;
    opts.output = Some(out_path.clone());

    let out = compile(&program, &opts).expect("compile");
    assert_eq!(out.artifact, out_path);

    let ir = out.llvm_ir.expect("llvm_ir");
    let needle = format!("@{expected_symbol}(");
    assert!(
      ir.lines()
        .any(|line| line.trim_start().starts_with("define") && line.contains(&needle)),
      "expected generated IR to define the TS entrypoint `{expected_symbol}`, got:\n{ir}"
    );
    assert!(
      ir.contains("__nativejs_file_init_"),
      "expected generated IR to contain a __nativejs_file_init_ symbol, got:\n{ir}"
    );
    assert!(
      ir.contains("define i32 @main()"),
      "expected generated IR to define a C ABI main() shim, got:\n{ir}"
    );
    assert!(
      ir.contains(&format!("call double @{expected_symbol}")),
      "expected main() shim to call the lowered TS main() `{expected_symbol}`, got:\n{ir}"
    );
  }

  fn find_line<'a>(ir: &'a str, needle: &str) -> &'a str {
    ir.lines()
      .find(|l| l.contains(needle))
      .unwrap_or_else(|| panic!("missing `{needle}` in IR:\n{ir}"))
  }

  fn find_define_line<'a>(ir: &'a str, needle: &str) -> &'a str {
    ir.lines()
      .find(|l| l.trim_start().starts_with("define") && l.contains(needle))
      .unwrap_or_else(|| panic!("missing `define` for `{needle}` in IR:\n{ir}"))
  }

  fn attr_group_on_line(line: &str) -> Option<u32> {
    let idx = line.find('#')?;
    let digits: String = line[idx + 1..]
      .chars()
      .take_while(|c| c.is_ascii_digit())
      .collect();
    digits.parse().ok()
  }

  fn attr_line_for_define<'a>(ir: &'a str, define_line: &str) -> &'a str {
    let group = attr_group_on_line(define_line)
      .unwrap_or_else(|| panic!("missing attribute group on define line:\n{define_line}\n\nIR:\n{ir}"));
    find_line(ir, &format!("attributes #{group} ="))
  }

  fn assert_stack_walking_attrs(ir: &str, define_line: &str) {
    let attrs = attr_line_for_define(ir, define_line);
    assert!(
      attrs.contains("\"frame-pointer\"=\"all\""),
      "expected stack-walking frame-pointer attr, got:\n{attrs}\n\nIR:\n{ir}"
    );
    assert!(
      attrs.contains("\"disable-tail-calls\"=\"true\"") || attrs.contains("disable-tail-calls"),
      "expected stack-walking disable-tail-calls attr, got:\n{attrs}\n\nIR:\n{ir}"
    );
  }

  fn assert_debug_function_attrs(ir: &str, define_line: &str) {
    let attrs = attr_line_for_define(ir, define_line);
    assert!(
      attrs.contains("optnone"),
      "expected `optnone` in attrs, got:\n{attrs}\n\nIR:\n{ir}"
    );
    assert!(
      attrs.contains("noinline"),
      "expected `noinline` in attrs, got:\n{attrs}\n\nIR:\n{ir}"
    );
  }

  fn assert_no_debug_function_attrs(ir: &str, define_line: &str) {
    let attrs = attr_line_for_define(ir, define_line);
    assert!(
      !attrs.contains("optnone") && !attrs.contains("noinline"),
      "did not expect debug-only attrs, got:\n{attrs}\n\nIR:\n{ir}"
    );
  }

  #[test]
  fn debug_build_applies_debuggable_function_attributes() {
    use crate::{compile, llvm_symbol_for_def, llvm_symbol_for_file_init, CompilerOptions, EmitKind};
    use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
    use typecheck_ts::{FileKey, MemoryHost, Program};

    let mut host = MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
      ..Default::default()
    });
    let key = FileKey::new("main.ts");
    host.insert(
      key.clone(),
      r#"
export function main() {
  return 1 + 2;
}
"#,
    );

    let program = Program::new(host, vec![key.clone()]);
    let diags = program.check();
    assert!(
      diags.iter().all(|d| d.severity != super::Severity::Error),
      "expected sample to typecheck cleanly, got: {diags:#?}"
    );

    let file = program.file_id(&key).expect("file id");
    let def = program
      .exports_of(file)
      .get("main")
      .and_then(|e| e.def)
      .expect("exported def for `main`");
    let expected_ts_main = llvm_symbol_for_def(&program, def);
    let expected_file_init = llvm_symbol_for_file_init(file);

    let tmp = tempfile::tempdir().expect("create tempdir");
    let out_path = tmp.path().join("debug.ll");

    let mut opts = CompilerOptions::default();
    opts.debug = true;
    opts.emit = EmitKind::LlvmIr;
    opts.output = Some(out_path);

    let out = compile(&program, &opts).expect("compile (debug)");
    let ir = out.llvm_ir.expect("llvm_ir");

    let ts_main_def = find_define_line(&ir, &format!("@{expected_ts_main}("));
    assert_stack_walking_attrs(&ir, ts_main_def);
    assert_debug_function_attrs(&ir, ts_main_def);

    let file_init_def = find_define_line(&ir, &format!("@{expected_file_init}("));
    assert_stack_walking_attrs(&ir, file_init_def);
    assert_debug_function_attrs(&ir, file_init_def);

    let c_main_def = find_define_line(&ir, "@main(");
    assert_stack_walking_attrs(&ir, c_main_def);
    assert_debug_function_attrs(&ir, c_main_def);

    let out_path = tmp.path().join("release.ll");
    let mut opts = CompilerOptions::default();
    opts.emit = EmitKind::LlvmIr;
    opts.output = Some(out_path);

    let out = compile(&program, &opts).expect("compile (non-debug)");
    let ir = out.llvm_ir.expect("llvm_ir");

    let ts_main_def = find_define_line(&ir, &format!("@{expected_ts_main}("));
    assert_no_debug_function_attrs(&ir, ts_main_def);

    let file_init_def = find_define_line(&ir, &format!("@{expected_file_init}("));
    assert_no_debug_function_attrs(&ir, file_init_def);

    let c_main_def = find_define_line(&ir, "@main(");
    assert_no_debug_function_attrs(&ir, c_main_def);
  }
}
