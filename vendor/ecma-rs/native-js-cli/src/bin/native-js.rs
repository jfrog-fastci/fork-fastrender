use clap::{ArgAction, Args, Parser, Subcommand};
use diagnostics::{host_error, Diagnostic, FileId, Severity, Span, TextRange};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use typecheck_ts::Program;
use wait_timeout::ChildExt;

#[path = "../host.rs"]
mod host;

#[path = "../bench.rs"]
mod bench;

#[path = "../diag.rs"]
#[allow(dead_code)]
mod diag;

#[path = "../output.rs"]
mod output;

#[path = "../type_libs.rs"]
mod type_libs;

#[path = "../project_load.rs"]
mod project_load;

fn emit_bench_json(payload: &bench::BenchJsonOutput) {
  let stdout = std::io::stdout();
  let mut handle = stdout.lock();
  let _ = serde_json::to_writer_pretty(&mut handle, payload);
  let _ = writeln!(&mut handle);
}

#[derive(Parser, Debug)]
#[command(author, version, about = "Compile TypeScript to native executables via native-js (LLVM)")]
struct Cli {
  #[command(subcommand)]
  command: Commands,

  /// TypeScript project file (tsconfig.json) to load.
  #[arg(long, short = 'p', global = true)]
  project: Option<PathBuf>,

  /// Emit JSON output to stdout.
  ///
  /// - `check`/`build`/`emit-ir`: diagnostics JSON (`schema_version = 1`)
  /// - `bench`: benchmark JSON (`schema_version = 1`, `command = "bench"`)
  /// - `addr2line`: symbolization JSON (`schema_version = 1`, `command = "addr2line"`)
  #[arg(long, global = true)]
  json: bool,

  /// Force-enable ANSI colors in diagnostics output.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  color: bool,

  /// Disable ANSI colors in diagnostics output.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  no_color: bool,

  /// Optimization level (0-3).
  ///
  /// Defaults to `2` for most commands, but `bench` defaults to `3` unless explicitly overridden.
  ///
  /// When `--debug` is enabled and `--opt` is not explicitly provided, defaults to `0` for easier
  /// source-level debugging.
  #[arg(long, global = true)]
  opt: Option<u8>,

  /// Target triple to compile and link for (e.g. `x86_64-unknown-linux-gnu`).
  ///
  /// When provided, this is passed through to both LLVM codegen and the system linker driver
  /// (`clang -target <triple>`). Cross-compiling executables is not supported yet.
  #[arg(long, value_name = "TRIPLE", global = true)]
  target: Option<String>,

  /// Emit DWARF debug info in the generated executable (line tables / function names).
  #[arg(long, global = true)]
  debug: bool,

  /// Produce a PIE executable (ET_DYN) on Linux.
  ///
  /// By default native-js links non-PIE so LLVM stackmap relocations are resolved at link time.
  #[arg(long, global = true)]
  pie: bool,

  /// Override the `clang` used for linking.
  #[arg(long, value_name = "PATH", global = true)]
  clang: Option<PathBuf>,

  /// Override the `llvm-objcopy` used for stackmaps section rewriting (PIE + lld).
  #[arg(long, value_name = "PATH", global = true)]
  llvm_objcopy: Option<PathBuf>,

  /// Override the optional `llvm-objdump` used by debugging tools.
  #[arg(long, value_name = "PATH", global = true)]
  llvm_objdump: Option<PathBuf>,

  /// Pass `--sysroot=<PATH>` to clang during linking.
  #[arg(long, value_name = "PATH", global = true)]
  sysroot: Option<PathBuf>,

  /// Extra argument to pass to clang during linking (repeatable).
  #[arg(long, value_name = "ARG", global = true)]
  link_arg: Vec<String>,

  /// Print the exact tool invocations used during linking.
  #[arg(long, global = true, alias = "print-commands")]
  verbose: bool,

  /// Keep temporary build directories (for debugging) and print their paths.
  #[arg(long, global = true)]
  keep_temp: bool,

  /// Also run the legacy `native_js::strict::validate` checks.
  ///
  /// This is stricter than `validate_strict_subset` and may reject TypeScript-only,
  /// runtime-inert "escape hatches" like type assertions (`as`) and non-null assertions (`!`).
  #[arg(long, global = true)]
  extra_strict: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Type-check and validate the native-js strict subset.
  Check(CheckArgs),
  /// Compile an entry file to a native executable.
  Build(BuildArgs),
  /// Compile an entry file and run it immediately.
  Run(RunArgs),
  /// Compile an entry file and run it multiple times, reporting wall-clock timings.
  Bench(BenchArgs),
  /// Emit LLVM IR to a file.
  EmitIr(EmitIrArgs),
  /// Resolve instruction addresses to source locations using DWARF debug info.
  #[command(name = "addr2line")]
  Addr2Line(Addr2LineArgs),
}

#[derive(Args, Debug)]
struct CheckArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  entry: PathBuf,
}

#[derive(Args, Debug)]
struct BuildArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  entry: PathBuf,

  /// Output executable path.
  #[arg(short = 'o', long, value_name = "PATH")]
  output: PathBuf,

  /// Also emit LLVM IR (`.ll`) to the given path.
  #[arg(long, value_name = "PATH.ll")]
  emit_ir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct RunArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  entry: PathBuf,

  /// Arguments to pass to the generated executable (after `--`).
  #[arg(trailing_var_arg = true, value_name = "ARGS")]
  args: Vec<OsString>,
}

#[derive(Args, Debug)]
struct BenchArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  entry: PathBuf,

  /// Number of warmup runs (not included in timings).
  #[arg(long, default_value_t = 1, value_name = "N")]
  warmup: u32,

  /// Number of measured iterations.
  #[arg(long, default_value_t = 10, value_name = "N")]
  iters: u32,

  /// Timeout per run, in milliseconds.
  #[arg(long, default_value_t = 5000, value_name = "N")]
  timeout_ms: u64,

  /// Arguments to pass to the generated executable (after `--`).
  #[arg(trailing_var_arg = true, value_name = "ARGS")]
  args: Vec<OsString>,
}

#[derive(Args, Debug)]
struct EmitIrArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  entry: PathBuf,

  /// Output path for the emitted LLVM IR.
  #[arg(short = 'o', long, value_name = "PATH.ll")]
  output: PathBuf,
}

#[derive(Args, Debug)]
struct Addr2LineArgs {
  /// Executable or object file containing DWARF debug info.
  #[arg(value_name = "PATH")]
  exe: PathBuf,

  /// Instruction addresses to resolve (hex, with or without 0x prefix).
  #[arg(value_name = "ADDR", required = true, num_args = 1..)]
  addrs: Vec<String>,

  /// Base load address to subtract from each input address (hex).
  ///
  /// Useful when addresses come from a PIE binary in a running process.
  #[arg(long, value_name = "ADDR")]
  base: Option<String>,

  /// Demangle function names when possible (Rust symbols).
  #[arg(long, action = ArgAction::SetTrue)]
  demangle: bool,
}

const ADDR2LINE_JSON_SCHEMA_VERSION: u32 = 1;

#[derive(serde::Serialize)]
struct Addr2LineJsonOutput {
  schema_version: u32,
  command: &'static str,
  exe: String,
  base: Option<String>,
  demangle: bool,
  results: Vec<Addr2LineJsonResult>,
  error: Option<String>,
  exit_code: u8,
}

#[derive(serde::Serialize)]
struct Addr2LineJsonResult {
  input: String,
  addr: String,
  probe: String,
  file: Option<String>,
  line: Option<u32>,
  col: Option<u32>,
  function: Option<String>,
  symbol: Option<String>,
}

fn main() -> ExitCode {
  let cli = Cli::parse();

  if cli.json && matches!(&cli.command, Commands::Run(_)) {
    eprintln!("--json is not supported with `run` (it would mix with program stdout)");
    return ExitCode::from(2);
  }

  let target = match cli.target.as_deref() {
    Some(raw) => match raw.parse::<target_lexicon::Triple>() {
      Ok(triple) => Some(triple),
      Err(err) => {
        let message = format!(
          "invalid --target={raw} (expected a target triple like `x86_64-unknown-linux-gnu`): {err}"
        );
        if cli.json {
          // Ensure `native-js --json bench ...` always emits the bench schema, even on early errors
          // that happen before we can dispatch into `cmd_bench`.
          if let Commands::Bench(args) = &cli.command {
            let bench_args: Vec<String> = args
              .args
              .iter()
              .map(|a| a.to_string_lossy().into_owned())
              .collect();
            let diagnostic = host_error(None, message.clone());
            let payload = bench::BenchJsonOutput {
              schema_version: bench::JSON_SCHEMA_VERSION,
              command: "bench",
              diagnostics: vec![diagnostic],
              error: Some(message),
              entry: args.entry.display().to_string(),
              args: bench_args,
              warmup: args.warmup,
              iters: args.iters,
              timeout_ms: args.timeout_ms,
              compile_time_ms: 0.0,
              run_times_ms: Vec::new(),
              run_exit_codes: Vec::new(),
              stats: bench::stats(&[]),
              exit_code: 2,
            };
            emit_bench_json(&payload);
            return ExitCode::from(2);
          }
        }
        return exit_internal_without_program(cli.json, message);
      }
    },
    None => None,
  };

  let render = output::render_options(cli.color, cli.no_color);
  match &cli.command {
    Commands::Check(args) => cmd_check(&cli, &args.entry, render),
    Commands::Build(args) => cmd_build(
      &cli,
      &args.entry,
      &args.output,
      args.emit_ir.as_deref(),
      &target,
      render,
    ),
    Commands::Run(args) => cmd_run(&cli, &args.entry, &args.args, &target, render),
    Commands::Bench(args) => cmd_bench(&cli, args, &target, render),
    Commands::EmitIr(args) => cmd_emit_ir(&cli, &args.entry, &args.output, &target, render),
    Commands::Addr2Line(args) => cmd_addr2line(&cli, args),
  }
}

fn cmd_check(cli: &Cli, entry: &Path, render: diagnostics::render::RenderOptions) -> ExitCode {
  let (program, entry_file) =
    match project_load::load_program(cli.project.as_deref(), entry, project_load::LoadMode::Checked)
    {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  let diagnostics = collect_check_diagnostics(&program, entry_file, cli.extra_strict);
  let exit_code = ExitCode::from(diag::exit_code_for_diagnostics(&diagnostics));
  match output::emit_diagnostics(&program, diagnostics, cli.json, render) {
    Ok(_) => exit_code,
    Err(err) => exit_internal(
      &program,
      cli.json,
      render,
      format!("failed to write diagnostics: {err}"),
    ),
  }
}

fn cmd_build(
  cli: &Cli,
  entry: &Path,
  output_exe: &Path,
  emit_ir: Option<&Path>,
  target: &Option<target_lexicon::Triple>,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let (program, entry_file) =
    match project_load::load_program(cli.project.as_deref(), entry, project_load::LoadMode::Checked)
    {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  // `compile_program` already runs typechecking, strict-subset validation, and entrypoint checks.
  // We only pre-run the legacy strict validator when explicitly requested.
  if cli.extra_strict {
    let diagnostics = collect_check_diagnostics(&program, entry_file, true);
    let exit_code = ExitCode::from(diag::exit_code_for_diagnostics(&diagnostics));
    if exit_code != ExitCode::SUCCESS {
      let _ = output::emit_diagnostics(&program, diagnostics, cli.json, render);
      return exit_code;
    }
  }

  let mut opts = native_js::CompilerOptions::default();
  opts.emit = native_js::EmitKind::Executable;
  opts.output = Some(output_exe.to_path_buf());
  opts.emit_ir = emit_ir.map(|p| p.to_path_buf());
  opts.debug = cli.debug;
  opts.print_commands = cli.verbose;
  opts.keep_temp = cli.keep_temp;
  opts.pie = cli.pie;
  opts.target = target.clone();
  let opt_raw = cli.opt.unwrap_or(if cli.debug { 0 } else { 2 });
  opts.opt_level = match opt_level(opt_raw) {
    Ok(level) => level,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };
  if cli.clang.is_some()
    || cli.llvm_objcopy.is_some()
    || cli.llvm_objdump.is_some()
    || cli.sysroot.is_some()
    || !cli.link_arg.is_empty()
  {
    let toolchain = native_js::Toolchain::detect_with_overrides(
      cli.clang.clone(),
      cli.llvm_objcopy.clone(),
      cli.llvm_objdump.clone(),
      cli.sysroot.clone(),
      cli.link_arg.clone(),
    );
    match toolchain {
      Ok(tc) => opts.toolchain = Some(tc),
      Err(err) => return exit_internal(&program, cli.json, render, err.to_string()),
    }
  }

  if let Err(err) = native_js::compile_program(&program, entry_file, &opts) {
    if let Some(diags) = err.diagnostics() {
      let _ = output::emit_diagnostics(&program, diags.to_vec(), cli.json, render);
      return ExitCode::from(diag::exit_code_for_diagnostics(diags));
    }
    return exit_internal(&program, cli.json, render, err.to_string());
  }

  // Emit JSON even on success so callers can depend on a stable output shape.
  if cli.json {
    let _ = output::emit_diagnostics(&program, Vec::new(), true, render);
  }

  ExitCode::SUCCESS
}

fn cmd_emit_ir(
  cli: &Cli,
  entry: &Path,
  output_ll: &Path,
  target: &Option<target_lexicon::Triple>,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let (program, entry_file) =
    match project_load::load_program(cli.project.as_deref(), entry, project_load::LoadMode::Checked)
    {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  if cli.extra_strict {
    let diagnostics = collect_check_diagnostics(&program, entry_file, true);
    let exit_code = ExitCode::from(diag::exit_code_for_diagnostics(&diagnostics));
    if exit_code != ExitCode::SUCCESS {
      let _ = output::emit_diagnostics(&program, diagnostics, cli.json, render);
      return exit_code;
    }
  }

  let mut opts = native_js::CompilerOptions::default();
  opts.emit = native_js::EmitKind::LlvmIr;
  opts.output = Some(output_ll.to_path_buf());
  opts.debug = cli.debug;
  opts.print_commands = cli.verbose;
  opts.keep_temp = cli.keep_temp;
  opts.target = target.clone();
  let opt_raw = cli.opt.unwrap_or(if cli.debug { 0 } else { 2 });
  opts.opt_level = match opt_level(opt_raw) {
    Ok(level) => level,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };

  if let Err(err) = native_js::compile_program(&program, entry_file, &opts) {
    if let Some(diags) = err.diagnostics() {
      let _ = output::emit_diagnostics(&program, diags.to_vec(), cli.json, render);
      return ExitCode::from(diag::exit_code_for_diagnostics(diags));
    }
    return exit_internal(&program, cli.json, render, err.to_string());
  }

  if cli.json {
    let _ = output::emit_diagnostics(&program, Vec::new(), true, render);
  }

  ExitCode::SUCCESS
}

fn cmd_run(
  cli: &Cli,
  entry: &Path,
  args: &[OsString],
  target: &Option<target_lexicon::Triple>,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let dir = match tempdir() {
    Ok(dir) => dir,
    Err(err) => {
      return exit_internal_without_program(cli.json, format!("failed to create tempdir: {err}"));
    }
  };
  let (exe_dir, _exe_dir_keepalive) = if cli.keep_temp {
    let path = dir.path().to_path_buf();
    let _ = dir.keep();
    eprintln!("kept tempdir: {}", path.display());
    (path, None)
  } else {
    (dir.path().to_path_buf(), Some(dir))
  };
  let exe = exe_dir.join("out");

  let build_exit = cmd_build(cli, entry, &exe, None, target, render);
  if build_exit != ExitCode::SUCCESS {
    return build_exit;
  }

  let status = match Command::new(&exe)
    .args(args)
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .status()
  {
    Ok(status) => status,
    Err(err) => {
      eprintln!("failed to run {}: {err}", exe.display());
      return ExitCode::from(2);
    }
  };

  if status.success() {
    ExitCode::SUCCESS
  } else {
    let code = status
      .code()
      .and_then(|code| u8::try_from(code).ok())
      .unwrap_or(1);
    ExitCode::from(code)
  }
}

fn cmd_bench(
  cli: &Cli,
  args: &BenchArgs,
  target: &Option<target_lexicon::Triple>,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let entry = args.entry.display().to_string();
  let bench_args: Vec<String> = args
    .args
    .iter()
    .map(|a| a.to_string_lossy().into_owned())
    .collect();

  let dir = match tempdir() {
    Ok(dir) => dir,
    Err(err) => {
      let message = format!("failed to create tempdir for benchmark output: {err}");
      if cli.json {
        let diagnostic = host_error(None, message.clone());
        let payload = bench::BenchJsonOutput {
          schema_version: bench::JSON_SCHEMA_VERSION,
          command: "bench",
          diagnostics: vec![diagnostic],
          error: Some(message),
          entry,
          args: bench_args,
          warmup: args.warmup,
          iters: args.iters,
          timeout_ms: args.timeout_ms,
          compile_time_ms: 0.0,
          run_times_ms: Vec::new(),
          run_exit_codes: Vec::new(),
          stats: bench::stats(&[]),
          exit_code: 2,
        };
        emit_bench_json(&payload);
      } else {
        eprintln!("{message}");
      }
      return ExitCode::from(2);
    }
  };
  let exe = dir.path().join("out");

  let compile_start = Instant::now();
  let (program, entry_file) = match project_load::load_program(
    cli.project.as_deref(),
    &args.entry,
    project_load::LoadMode::Checked,
  ) {
    Ok(res) => res,
    Err(err) => {
      let compile_time_ms = bench::duration_ms(compile_start.elapsed());
      if cli.json {
        let diagnostic = host_error(None, err.clone());
        let payload = bench::BenchJsonOutput {
          schema_version: bench::JSON_SCHEMA_VERSION,
          command: "bench",
          diagnostics: vec![diagnostic],
          error: Some(err),
          entry,
          args: bench_args,
          warmup: args.warmup,
          iters: args.iters,
          timeout_ms: args.timeout_ms,
          compile_time_ms,
          run_times_ms: Vec::new(),
          run_exit_codes: Vec::new(),
          stats: bench::stats(&[]),
          exit_code: 2,
        };
        emit_bench_json(&payload);
        return ExitCode::from(2);
      }
      return exit_internal_without_program(false, err);
    }
  };

    if cli.extra_strict {
      let diagnostics = collect_check_diagnostics(&program, entry_file, true);
      let exit_code = diag::exit_code_for_diagnostics(&diagnostics);
      if exit_code != 0 {
        if cli.json {
          let payload = bench::BenchJsonOutput {
            schema_version: bench::JSON_SCHEMA_VERSION,
          command: "bench",
          diagnostics,
          error: None,
          entry,
          args: bench_args,
          warmup: args.warmup,
          iters: args.iters,
          timeout_ms: args.timeout_ms,
          compile_time_ms: bench::duration_ms(compile_start.elapsed()),
          run_times_ms: Vec::new(),
          run_exit_codes: Vec::new(),
          stats: bench::stats(&[]),
          exit_code,
        };
        emit_bench_json(&payload);
      } else {
        let _ = output::emit_diagnostics(&program, diagnostics, false, render);
      }
      return ExitCode::from(exit_code);
    }
  }

  let opt_raw = cli.opt.unwrap_or(3);
  let opt = match opt_level(opt_raw) {
    Ok(level) => level,
    Err(err) => {
      if cli.json {
        let diagnostic = host_error(None, err.clone());
        let payload = bench::BenchJsonOutput {
          schema_version: bench::JSON_SCHEMA_VERSION,
          command: "bench",
          diagnostics: vec![diagnostic],
          error: Some(err),
          entry,
          args: bench_args,
          warmup: args.warmup,
          iters: args.iters,
          timeout_ms: args.timeout_ms,
          compile_time_ms: bench::duration_ms(compile_start.elapsed()),
          run_times_ms: Vec::new(),
          run_exit_codes: Vec::new(),
          stats: bench::stats(&[]),
          exit_code: 2,
        };
        emit_bench_json(&payload);
        return ExitCode::from(2);
      }
      return exit_internal(&program, false, render, err);
    }
  };

  let mut opts = native_js::CompilerOptions::default();
  opts.emit = native_js::EmitKind::Executable;
  opts.output = Some(exe.clone());
  // Bench builds should be representative "release" builds.
  opts.debug = false;
  opts.pie = cli.pie;
  opts.target = target.clone();
  opts.opt_level = opt;

  if let Err(err) = native_js::compile_program(&program, entry_file, &opts) {
    let compile_time_ms = bench::duration_ms(compile_start.elapsed());
    if let Some(diags) = err.diagnostics() {
      let exit_code = diag::exit_code_for_diagnostics(diags);
      if cli.json {
        let payload = bench::BenchJsonOutput {
          schema_version: bench::JSON_SCHEMA_VERSION,
          command: "bench",
          diagnostics: diags.to_vec(),
          error: None,
          entry,
          args: bench_args,
          warmup: args.warmup,
          iters: args.iters,
          timeout_ms: args.timeout_ms,
          compile_time_ms,
          run_times_ms: Vec::new(),
          run_exit_codes: Vec::new(),
          stats: bench::stats(&[]),
          exit_code,
        };
        emit_bench_json(&payload);
      } else {
        let _ = output::emit_diagnostics(&program, diags.to_vec(), false, render);
      }
      return ExitCode::from(exit_code);
    }

    let message = err.to_string();
    if cli.json {
      let diagnostic = host_error(None, message.clone());
      let payload = bench::BenchJsonOutput {
        schema_version: bench::JSON_SCHEMA_VERSION,
        command: "bench",
        diagnostics: vec![diagnostic],
        error: Some(message),
        entry,
        args: bench_args,
        warmup: args.warmup,
        iters: args.iters,
        timeout_ms: args.timeout_ms,
        compile_time_ms,
        run_times_ms: Vec::new(),
        run_exit_codes: Vec::new(),
        stats: bench::stats(&[]),
        exit_code: 2,
      };
      emit_bench_json(&payload);
      return ExitCode::from(2);
    }
    return exit_internal(&program, false, render, message);
  }

  let compile_time_ms = bench::duration_ms(compile_start.elapsed());

  let timeout = Duration::from_millis(args.timeout_ms);

  // Warmup (unmeasured).
  for _ in 0..args.warmup {
    let run = match run_bench_once(&exe, &args.args, timeout) {
      Ok(res) => res,
      Err(err) => {
        if cli.json {
          let diagnostic = host_error(None, err.clone());
          let payload = bench::BenchJsonOutput {
            schema_version: bench::JSON_SCHEMA_VERSION,
            command: "bench",
            diagnostics: vec![diagnostic],
            error: Some(err),
            entry,
            args: bench_args,
            warmup: args.warmup,
            iters: args.iters,
            timeout_ms: args.timeout_ms,
            compile_time_ms,
            run_times_ms: Vec::new(),
            run_exit_codes: Vec::new(),
            stats: bench::stats(&[]),
            exit_code: 2,
          };
          emit_bench_json(&payload);
        } else {
          eprintln!("{err}");
        }
        return ExitCode::from(2);
      }
    };
    if run.exit_code != 0 {
      let error = run
        .timed_out
        .then_some(format!("benchmark run timed out after {}ms", args.timeout_ms));
      if cli.json {
        let payload = bench::BenchJsonOutput {
          schema_version: bench::JSON_SCHEMA_VERSION,
          command: "bench",
          diagnostics: Vec::new(),
          error,
          entry,
          args: bench_args,
          warmup: args.warmup,
          iters: args.iters,
          timeout_ms: args.timeout_ms,
          compile_time_ms,
          run_times_ms: Vec::new(),
          run_exit_codes: Vec::new(),
          stats: bench::stats(&[]),
          exit_code: run.exit_code,
        };
        emit_bench_json(&payload);
      }
      return ExitCode::from(run.exit_code);
    }
  }

  // Measured runs.
  let mut run_times_ms = Vec::with_capacity(args.iters as usize);
  let mut run_exit_codes = Vec::with_capacity(args.iters as usize);
  let mut error: Option<String> = None;

  let mut exit_code = 0u8;
  for _ in 0..args.iters {
    let run_start = Instant::now();
    let run = match run_bench_once(&exe, &args.args, timeout) {
      Ok(res) => res,
      Err(err) => {
        if cli.json {
          let stats = bench::stats(&run_times_ms);
          let diagnostic = host_error(None, err.clone());
          let payload = bench::BenchJsonOutput {
            schema_version: bench::JSON_SCHEMA_VERSION,
            command: "bench",
            diagnostics: vec![diagnostic],
            error: Some(err),
            entry,
            args: bench_args,
            warmup: args.warmup,
            iters: args.iters,
            timeout_ms: args.timeout_ms,
            compile_time_ms,
            run_times_ms,
            run_exit_codes,
            stats,
            exit_code: 2,
          };
          emit_bench_json(&payload);
        } else {
          eprintln!("{err}");
        }
        return ExitCode::from(2);
      }
    };
    let elapsed_ms = bench::duration_ms(run_start.elapsed());
    run_times_ms.push(elapsed_ms);
    run_exit_codes.push(i32::from(run.exit_code));

    if run.exit_code != 0 {
      exit_code = run.exit_code;
      if run.timed_out {
        error = Some(format!("benchmark run timed out after {}ms", args.timeout_ms));
      }
      break;
    }
  }

  let stats = bench::stats(&run_times_ms);

  if cli.json {
    let payload = bench::BenchJsonOutput {
      schema_version: bench::JSON_SCHEMA_VERSION,
      command: "bench",
      diagnostics: Vec::new(),
      error,
      entry,
      args: bench_args,
      warmup: args.warmup,
      iters: args.iters,
      timeout_ms: args.timeout_ms,
      compile_time_ms,
      run_times_ms,
    run_exit_codes,
    stats,
    exit_code,
  };
    emit_bench_json(&payload);
  } else {
    println!("compile_time_ms: {compile_time_ms:.3}");
    println!("run_times_ms: {run_times_ms:?}");
    println!("mean_ms: {:.3}", stats.mean_ms);
    println!("median_ms: {:.3}", stats.median_ms);
    println!("min_ms: {:.3}", stats.min_ms);
    println!("max_ms: {:.3}", stats.max_ms);
  }

  ExitCode::from(exit_code)
}

#[derive(Debug, Clone, Copy)]
struct BenchRunResult {
  exit_code: u8,
  timed_out: bool,
}

fn run_bench_once(exe: &Path, args: &[OsString], timeout: Duration) -> Result<BenchRunResult, String> {
  let mut child = Command::new(exe)
    .args(args)
    .stdin(Stdio::inherit())
    // Bench output should be stable; don't mix benchmark stdout/stderr into the report output.
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .map_err(|err| format!("failed to run {}: {err}", exe.display()))?;

  match child.wait_timeout(timeout).map_err(|err| err.to_string())? {
    Some(status) => Ok(BenchRunResult {
      exit_code: status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1),
      timed_out: false,
    }),
    None => {
      let _ = child.kill();
      let _ = child.wait();
      Ok(BenchRunResult {
        // Use `timeout(1)`'s conventional exit status.
        exit_code: 124,
        timed_out: true,
      })
    }
  }
}

fn cmd_addr2line(cli: &Cli, args: &Addr2LineArgs) -> ExitCode {
  fn emit_json(payload: &Addr2LineJsonOutput) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = serde_json::to_writer_pretty(&mut handle, payload);
    let _ = writeln!(&mut handle);
  }

  let base = match args.base.as_deref() {
    Some(raw) => match parse_hex_u64(raw) {
      Ok(value) => Some(value),
      Err(err) => {
        if cli.json {
          let payload = Addr2LineJsonOutput {
            schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
            command: "addr2line",
            exe: args.exe.display().to_string(),
            base: None,
            demangle: args.demangle,
            results: Vec::new(),
            error: Some(err),
            exit_code: 2,
          };
          emit_json(&payload);
        } else {
          eprintln!("{err}");
        }
        return ExitCode::from(2);
      }
    },
    None => None,
  };

  let loader = match addr2line::Loader::new(&args.exe) {
    Ok(loader) => loader,
    Err(err) => {
      let msg = format!(
        "failed to load DWARF debug info from {}: {err}",
        args.exe.display()
      );
      if cli.json {
        let payload = Addr2LineJsonOutput {
          schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
          command: "addr2line",
          exe: args.exe.display().to_string(),
          base: base.map(|b| format!("0x{b:x}")),
          demangle: args.demangle,
          results: Vec::new(),
          error: Some(msg),
          exit_code: 2,
        };
        emit_json(&payload);
      } else {
        eprintln!("{msg}");
      }
      return ExitCode::from(2);
    }
  };

  let mut results = Vec::with_capacity(args.addrs.len());

  for raw_addr in &args.addrs {
    let addr_raw = match parse_hex_u64(raw_addr) {
      Ok(addr) => addr,
      Err(err) => {
        if cli.json {
          let payload = Addr2LineJsonOutput {
            schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
            command: "addr2line",
            exe: args.exe.display().to_string(),
            base: base.map(|b| format!("0x{b:x}")),
            demangle: args.demangle,
            results,
            error: Some(err),
            exit_code: 2,
          };
          emit_json(&payload);
        } else {
          eprintln!("{err}");
        }
        return ExitCode::from(2);
      }
    };

    let mut probe = addr_raw;
    if let Some(base) = base {
      probe = match probe.checked_sub(base) {
        Some(v) => v,
        None => {
          let msg = format!(
            "address {raw_addr} is below base 0x{base:x} (use a correct --base for PIE/ASLR offsets)"
          );
          if cli.json {
            let payload = Addr2LineJsonOutput {
              schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
              command: "addr2line",
              exe: args.exe.display().to_string(),
              base: Some(format!("0x{base:x}")),
              demangle: args.demangle,
              results,
              error: Some(msg),
              exit_code: 2,
            };
            emit_json(&payload);
          } else {
            eprintln!("{msg}");
          }
          return ExitCode::from(2);
        }
      };
    }

    let mut file: Option<String> = None;
    let mut line: Option<u32> = None;
    let mut col: Option<u32> = None;
    let mut function: Option<String> = None;
    let mut have_line = false;

    let symbol = loader.find_symbol(probe).map(|sym| {
      if args.demangle {
        addr2line::demangle_auto(std::borrow::Cow::Borrowed(sym), None).into_owned()
      } else {
        sym.to_string()
      }
    });

    let mut frames = match loader.find_frames(probe) {
      Ok(frames) => frames,
      Err(err) => {
        let msg = format!("failed to resolve 0x{probe:x}: {err}");
        if cli.json {
          let payload = Addr2LineJsonOutput {
            schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
            command: "addr2line",
            exe: args.exe.display().to_string(),
            base: base.map(|b| format!("0x{b:x}")),
            demangle: args.demangle,
            results,
            error: Some(msg),
            exit_code: 2,
          };
          emit_json(&payload);
        } else {
          eprintln!("{msg}");
        }
        return ExitCode::from(2);
      }
    };

    loop {
      let frame = match frames.next() {
        Ok(Some(frame)) => frame,
        Ok(None) => break,
        Err(err) => {
          let msg = format!("failed to resolve 0x{probe:x}: {err}");
          if cli.json {
            let payload = Addr2LineJsonOutput {
              schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
              command: "addr2line",
              exe: args.exe.display().to_string(),
              base: base.map(|b| format!("0x{b:x}")),
              demangle: args.demangle,
              results,
              error: Some(msg),
              exit_code: 2,
            };
            emit_json(&payload);
          } else {
            eprintln!("{msg}");
          }
          return ExitCode::from(2);
        }
      };

      if function.is_none() {
        if let Some(func) = frame.function {
          let rendered = if args.demangle {
            func.demangle().unwrap_or_else(|_| func.raw_name().unwrap_or_default())
          } else {
            func.raw_name().unwrap_or_default()
          };
          if !rendered.is_empty() {
            function = Some(rendered.to_string());
          }
        }
      }

      if !have_line {
        if let Some(loc) = frame.location {
          if let Some(f) = loc.file {
            if let Some(l) = loc.line {
              file = Some(f.to_string());
              line = Some(l);
              col = loc.column;
              have_line = true;
            } else if file.is_none() {
              // Some toolchains omit line info for prologues; prefer a concrete
              // line when available, but fall back to printing the file.
              file = Some(f.to_string());
              col = loc.column;
            }
          }
        }
      }
    }

    if cli.json {
      results.push(Addr2LineJsonResult {
        input: raw_addr.clone(),
        addr: format!("0x{addr_raw:x}"),
        probe: format!("0x{probe:x}"),
        file,
        line,
        col,
        function,
        symbol,
      });
    } else {
      let file = file.unwrap_or_else(|| "??".to_string());
      let line = line.unwrap_or(0);
      let mut out = format!("{file}:{line}");
      if let Some(col) = col {
        out.push(':');
        out.push_str(&col.to_string());
      }
      if let Some(function) = function {
        out.push(' ');
        out.push_str(&function);
      } else if let Some(symbol) = symbol.as_deref() {
        out.push(' ');
        out.push_str(symbol);
      }
      println!("{out}");
    }
  }

  if cli.json {
    let payload = Addr2LineJsonOutput {
      schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
      command: "addr2line",
      exe: args.exe.display().to_string(),
      base: base.map(|b| format!("0x{b:x}")),
      demangle: args.demangle,
      results,
      error: None,
      exit_code: 0,
    };
    emit_json(&payload);
  }

  ExitCode::SUCCESS
}

fn parse_hex_u64(raw: &str) -> Result<u64, String> {
  let raw = raw.trim();
  let no_prefix = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")).unwrap_or(raw);
  let no_underscores = no_prefix.replace('_', "");
  u64::from_str_radix(&no_underscores, 16)
    .map_err(|err| format!("invalid hex address `{raw}`: {err}"))
}

fn collect_check_diagnostics(program: &Program, entry_file: FileId, extra_strict: bool) -> Vec<Diagnostic> {
  let mut diagnostics = program.check();
  let has_type_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);

  if !has_type_errors {
    if let Err(diags) = native_js::validate::validate_strict_subset(program) {
      diagnostics.extend(diags);
    }

    if extra_strict {
      diagnostics.extend(native_js::strict::validate(program, &program.reachable_files()));
    }

    if let Err(diags) = native_js::strict::entrypoint(program, entry_file) {
      diagnostics.extend(diags);
    }
  }

  diagnostics::sort_diagnostics(&mut diagnostics);
  diagnostics
}

fn opt_level(raw: u8) -> Result<native_js::OptLevel, String> {
  match raw {
    0 => Ok(native_js::OptLevel::O0),
    1 => Ok(native_js::OptLevel::O1),
    2 => Ok(native_js::OptLevel::O2),
    3 => Ok(native_js::OptLevel::O3),
    other => Err(format!("invalid --opt={other} (expected 0,1,2,3)")),
  }
}

fn exit_internal_without_program(json: bool, message: String) -> ExitCode {
  if json {
    let diagnostic = host_error(None, message.clone());
    let _ = output::emit_json_diagnostics(None, vec![diagnostic]);
    ExitCode::from(2)
  } else {
    eprintln!("{message}");
    ExitCode::from(2)
  }
}

fn exit_internal(
  program: &Program,
  json: bool,
  render: diagnostics::render::RenderOptions,
  message: String,
) -> ExitCode {
  let diagnostic = host_error(Some(Span::new(FileId(0), TextRange::new(0, 0))), message);
  let _ = output::emit_diagnostics(program, vec![diagnostic], json, render);
  ExitCode::from(2)
}
