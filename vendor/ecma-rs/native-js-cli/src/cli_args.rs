use clap::{ArgAction, Args, Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct DebugPrefixMap {
  pub from: PathBuf,
  pub to: PathBuf,
}

impl std::str::FromStr for DebugPrefixMap {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let (from, to) = s
      .split_once('=')
      .ok_or_else(|| format!("invalid --debug-prefix-map value `{s}` (expected FROM=TO)"))?;
    if from.is_empty() {
      return Err("invalid --debug-prefix-map: FROM must not be empty".to_string());
    }
    Ok(Self {
      from: PathBuf::from(from),
      to: PathBuf::from(to),
    })
  }
}

#[derive(Parser, Debug)]
#[command(
  author,
  version,
  about = "Compile TypeScript to native executables via native-js (LLVM)"
)]
pub struct Cli {
  #[command(subcommand)]
  pub command: Commands,

  /// TypeScript project file (tsconfig.json) to load.
  #[arg(long, short = 'p', global = true)]
  pub project: Option<PathBuf>,

  /// Emit JSON output to stdout.
  ///
  /// - `check`/`build`/`emit`/`emit-ir`: diagnostics JSON (`schema_version = 1`)
  /// - `bench`: benchmark JSON (`schema_version = 1`, `command = "bench"`)
  /// - `addr2line`: symbolization JSON (`schema_version = 1`, `command = "addr2line"`)
  #[arg(long, global = true)]
  pub json: bool,

  /// Force-enable ANSI colors in diagnostics output.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  pub color: bool,

  /// Disable ANSI colors in diagnostics output.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  pub no_color: bool,

  /// Build with full optimizations (defaults to `--opt-level 3` unless overridden).
  ///
  /// Conflicts with `--debug` (use one build profile preset at a time).
  #[arg(long, global = true, conflicts_with = "debug")]
  pub release: bool,

  /// Build with debug settings (defaults to `--opt-level 0` unless overridden).
  ///
  /// Enables `CompilerOptions.debug` (DWARF debug info emission) and keeps intermediate build
  /// artifacts where possible.
  #[arg(long, global = true)]
  pub debug: bool,

  /// Remap path prefixes embedded in emitted DWARF debug info.
  ///
  /// Repeatable. Format: `FROM=TO` (similar to `clang -fdebug-prefix-map` and `rustc --remap-path-prefix`).
  #[arg(long, value_name = "FROM=TO", global = true)]
  pub debug_prefix_map: Vec<DebugPrefixMap>,

  /// Optimization level (0-3).
  ///
  /// This overrides `--release`/`--debug` defaults.
  #[arg(
    long = "opt-level",
    visible_alias = "opt",
    value_name = "0-3",
    global = true
  )]
  pub opt_level: Option<u8>,

  /// Compilation target triple (defaults to the host).
  #[arg(long, value_name = "TRIPLE", global = true)]
  pub target: Option<String>,

  /// Produce a PIE executable (ET_DYN) on Linux.
  ///
  /// By default native-js links non-PIE so LLVM stackmap relocations are resolved at link time.
  #[arg(long, global = true)]
  pub pie: bool,

  /// Override the `clang` used for linking.
  #[arg(long, value_name = "PATH", global = true)]
  pub clang: Option<PathBuf>,

  /// Override the `llvm-objcopy` used for stackmaps section rewriting (PIE + lld).
  #[arg(long, value_name = "PATH", global = true)]
  pub llvm_objcopy: Option<PathBuf>,

  /// Override the optional `llvm-objdump` used by debugging tools.
  #[arg(long, value_name = "PATH", global = true)]
  pub llvm_objdump: Option<PathBuf>,

  /// Pass `--sysroot=<PATH>` to clang during linking.
  #[arg(long, value_name = "PATH", global = true)]
  pub sysroot: Option<PathBuf>,

  /// Extra argument to pass to clang during linking (repeatable).
  #[arg(long, value_name = "ARG", global = true)]
  pub link_arg: Vec<String>,

  /// Print the exact tool invocations used during linking.
  #[arg(long, global = true, alias = "print-commands")]
  pub verbose: bool,

  /// Keep temporary build directories (for debugging) and print their paths.
  #[arg(long, global = true)]
  pub keep_temp: bool,

  /// Also run the legacy `native_js::strict::validate` checks.
  ///
  /// This is stricter than `validate_strict_subset` and may reject TypeScript-only,
  /// runtime-inert "escape hatches" like type assertions (`as`) and non-null assertions (`!`).
  #[arg(long, global = true)]
  pub extra_strict: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
  /// Type-check and validate the native-js strict subset.
  Check(CheckArgs),

  /// Compile an entry file and emit compiler artifacts (defaults to an executable).
  Build(BuildArgs),

  /// Compile an entry file and run it immediately.
  Run(RunArgs),

  /// Compile an entry file and run it repeatedly, reporting timing results.
  Bench(BenchArgs),

  /// Emit one or more compiler artifacts for an entry file.
  Emit(EmitArgs),

  /// Emit LLVM IR to a file (deprecated; prefer `build --emit llvm`).
  #[command(hide = true)]
  EmitIr(EmitIrArgs),

  /// Resolve instruction addresses to source locations using DWARF debug info.
  #[command(name = "addr2line")]
  Addr2Line(Addr2LineArgs),
}

#[derive(Args, Debug)]
pub struct CheckArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  pub entry: PathBuf,
}

#[derive(Args, Debug)]
pub struct BuildArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  pub entry: PathBuf,

  /// Output path for the emitted artifact.
  ///
  /// - When emitting a single kind without `--out-dir`, this is the full output path.
  /// - When `--out-dir` is set, this is treated as the output *stem* (filename) used to name
  ///   outputs in the directory.
  #[arg(short = 'o', long, value_name = "PATH")]
  pub output: Option<PathBuf>,

  /// Also emit LLVM IR (`.ll`) to the given path.
  #[arg(long, value_name = "PATH.ll")]
  pub emit_ir: Option<PathBuf>,

  /// Which artifacts to emit (`llvm`, `bc`, `obj`, `asm`, `exe`, `hir`).
  ///
  /// If omitted, defaults to `exe` (the historical `build` behavior).
  #[arg(long, value_enum, action = ArgAction::Append, value_name = "KIND")]
  pub emit: Vec<crate::emit::EmitKindArg>,

  /// Output directory for emitted artifacts.
  ///
  /// Required when multiple `--emit` kinds are requested.
  #[arg(long, value_name = "DIR")]
  pub out_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  pub entry: PathBuf,

  /// Arguments to pass to the generated executable (after `--`).
  #[arg(trailing_var_arg = true, value_name = "ARGS")]
  pub args: Vec<OsString>,
}

#[derive(Args, Debug)]
pub struct BenchArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  pub entry: PathBuf,

  /// Number of warmup runs (not included in timings).
  #[arg(long, default_value_t = 1, value_name = "N")]
  pub warmup: u32,

  /// Number of measured iterations.
  #[arg(long, default_value_t = 10, value_name = "N")]
  pub iters: u32,

  /// Timeout per run, in milliseconds.
  #[arg(long, default_value_t = 5000, value_name = "N")]
  pub timeout_ms: u64,

  /// Arguments to pass to the generated executable (after `--`).
  #[arg(trailing_var_arg = true, value_name = "ARGS")]
  pub args: Vec<OsString>,
}

#[derive(Args, Debug)]
pub struct EmitIrArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  pub entry: PathBuf,

  /// Output path for the emitted LLVM IR.
  #[arg(short = 'o', long, value_name = "PATH.ll")]
  pub output: PathBuf,
}

#[derive(Args, Debug)]
pub struct EmitArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  pub entry: PathBuf,

  /// Which artifacts to emit (`llvm`, `bc`, `obj`, `asm`, `exe`, `hir`).
  #[arg(long, value_enum, action = ArgAction::Append, required = true, value_name = "KIND")]
  pub emit: Vec<crate::emit::EmitKindArg>,

  /// Output directory for emitted artifacts.
  ///
  /// Required when multiple `--emit` kinds are requested.
  #[arg(long, value_name = "DIR")]
  pub out_dir: Option<PathBuf>,

  /// Output path for the emitted artifact (single-emit mode) or output stem (when `--out-dir` is
  /// set).
  #[arg(short = 'o', long, value_name = "PATH")]
  pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct Addr2LineArgs {
  /// Executable or object file containing DWARF debug info.
  #[arg(value_name = "PATH")]
  pub exe: PathBuf,

  /// Instruction addresses to resolve (hex, with or without 0x prefix).
  ///
  /// If `--stdin` is passed, addresses can also be read from stdin.
  #[arg(value_name = "ADDR", required_unless_present = "stdin", num_args = 1..)]
  pub addrs: Vec<String>,

  /// Read instruction addresses from stdin.
  ///
  /// Lines are scanned for the first hex token (best-effort).
  #[arg(long, action = ArgAction::SetTrue)]
  pub stdin: bool,

  /// Base load address to subtract from each input address (hex).
  ///
  /// Useful when addresses come from a PIE binary in a running process.
  #[arg(long, value_name = "ADDR")]
  pub base: Option<String>,

  /// Demangle function names when possible (Rust symbols).
  #[arg(long, action = ArgAction::SetTrue)]
  pub demangle: bool,

  /// Exit with code 1 if any address cannot be resolved to a source line.
  ///
  /// This is useful in CI to validate that emitted debug info contains expected line tables.
  #[arg(long, action = ArgAction::SetTrue)]
  pub strict: bool,
}
