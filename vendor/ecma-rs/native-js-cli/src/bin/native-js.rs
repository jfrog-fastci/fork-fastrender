use clap::Parser;
use diagnostics::{host_error, Diagnostic, FileId, Severity, Span, TextRange};
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use target_lexicon::HOST;
use tempfile::tempdir;
use typecheck_ts::Program;
use wait_timeout::ChildExt;

#[path = "../bench.rs"]
mod bench;

#[path = "../cli_args.rs"]
mod cli_args;

#[path = "../host.rs"]
mod host;

#[path = "../output.rs"]
mod output;

#[path = "../diag.rs"]
mod diag;

#[path = "../emit.rs"]
mod emit;
#[path = "../type_libs.rs"]
mod type_libs;

#[path = "../project_load.rs"]
mod project_load;

use cli_args::{Cli, Commands};

fn emit_bench_json(payload: &bench::BenchJsonOutput) {
  let stdout = std::io::stdout();
  let mut handle = stdout.lock();
  let _ = serde_json::to_writer_pretty(&mut handle, payload);
  let _ = writeln!(&mut handle);
}

fn exit_bench_json_error(args: &cli_args::BenchArgs, message: String) -> ExitCode {
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
  ExitCode::from(2)
}

const ADDR2LINE_JSON_SCHEMA_VERSION: u32 = 1;

#[derive(serde::Serialize)]
struct Addr2LineJsonOutput {
  schema_version: u32,
  command: &'static str,
  exe: String,
  base: Option<String>,
  demangle: bool,
  stdin: bool,
  strict: bool,
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

  // Phase 7 profile flags: `--release` vs `--debug`.
  if cli.debug && cli.release {
    let message = "--debug and --release cannot be used together".to_string();
    if cli.json {
      if let Commands::Bench(args) = &cli.command {
        return exit_bench_json_error(args, message);
      }
    }
    eprintln!("{message}");
    return ExitCode::from(2);
  }

  // `--json` is reserved for machine-readable output on stdout. `run` inherits the program stdout,
  // so it would mix.
  if cli.json && matches!(&cli.command, Commands::Run(_)) {
    eprintln!("--json is not supported with `run` (it would mix with program stdout)");
    return ExitCode::from(2);
  }

  let target = match cli.target.as_deref() {
    Some(raw) => match raw.parse::<target_lexicon::Triple>() {
      Ok(triple) => Some(triple),
      Err(err) => {
        let message = format!("invalid target triple `{raw}`: {err}");
        if cli.json {
          // Ensure `native-js --json bench ...` always emits the bench schema, even on early errors
          // that happen before we can dispatch into `cmd_bench`.
          if let Commands::Bench(args) = &cli.command {
            return exit_bench_json_error(args, message);
          }
        }
        return exit_internal_without_program(cli.json, message);
      }
    },
    None => None,
  };

  // `run`/`bench` only make sense when the produced executable can run on the host.
  if target.as_ref().is_some_and(|target| target != &HOST)
    && matches!(&cli.command, Commands::Run(_) | Commands::Bench(_))
  {
    let message =
      "`run`/`bench` do not support cross-target execution; use `build --target ...`".to_string();
    if cli.json {
      if let Commands::Bench(args) = &cli.command {
        return exit_bench_json_error(args, message);
      }
    }
    eprintln!("{message}");
    return ExitCode::from(2);
  }

  let render = output::render_options(cli.color, cli.no_color);
  match &cli.command {
    Commands::Check(args) => cmd_check(&cli, &args.entry, render),
    Commands::Build(args) => cmd_build(&cli, args, &target, render),
    Commands::Run(args) => cmd_run(&cli, &args.entry, &args.args, &target, render),
    Commands::Bench(args) => cmd_bench(&cli, args, &target, render),
    Commands::Emit(args) => cmd_emit(&cli, args, &target, render),
    Commands::EmitIr(args) => cmd_emit_ir(&cli, &args.entry, &args.output, &target, render),
    Commands::Addr2Line(args) => cmd_addr2line(&cli, args),
  }
}

fn cmd_check(cli: &Cli, entry: &Path, render: diagnostics::render::RenderOptions) -> ExitCode {
  let (program, entry_file) = match project_load::load_program(
    cli.project.as_deref(),
    entry,
    project_load::LoadMode::Checked,
  ) {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  let diagnostics = collect_check_diagnostics(&program, entry_file, cli.extra_strict);
  let exit_code = exit_code_for_diagnostics(&diagnostics);
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
  args: &cli_args::BuildArgs,
  target: &Option<target_lexicon::Triple>,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let (program, entry_file) = match project_load::load_program(
    cli.project.as_deref(),
    &args.entry,
    project_load::LoadMode::Checked,
  ) {
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

  let requested_emits: Vec<emit::EmitKindArg> = if args.emit.is_empty() {
    vec![emit::EmitKindArg::Executable]
  } else {
    args.emit.clone()
  };

  if args.emit_ir.is_some() && !requested_emits.contains(&emit::EmitKindArg::Executable) {
    return exit_internal(
      &program,
      cli.json,
      render,
      "`build --emit-ir <PATH.ll>` is only supported when emitting an executable; use `emit <ENTRY> --emit llvm -o <PATH.ll>` instead"
        .to_string(),
    );
  }

  let paths = match emit::compute_emit_paths(
    &requested_emits,
    args.out_dir.as_deref(),
    args.output.as_deref(),
    "out",
  ) {
    Ok(paths) => paths,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };

  // Ensure output directories exist before writing any artifacts.
  for path in paths.values() {
    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        if let Err(err) = std::fs::create_dir_all(parent) {
          return exit_internal(
            &program,
            cli.json,
            render,
            format!("failed to create {}: {err}", parent.display()),
          );
        }
      }
    }
  }
  if let Some(path) = args.emit_ir.as_deref() {
    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        if let Err(err) = std::fs::create_dir_all(parent) {
          return exit_internal(
            &program,
            cli.json,
            render,
            format!("failed to create {}: {err}", parent.display()),
          );
        }
      }
    }
  }

  // Emit HIR first so the dump is available even if LLVM emission fails later.
  if let Some(hir_path) = paths.get(&emit::EmitKindArg::Hir) {
    let diagnostics = program.check();
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
      let exit_code = ExitCode::from(diag::exit_code_for_diagnostics(&diagnostics));
      let _ = output::emit_diagnostics(&program, diagnostics, cli.json, render);
      return exit_code;
    }

    if let Err(err) = std::fs::write(hir_path, emit::format_hir_dump(&program)) {
      return exit_internal(
        &program,
        cli.json,
        render,
        format!("failed to write {}: {err}", hir_path.display()),
      );
    }
  }

  let opt = match opt_level_for_command(cli, native_js::OptLevel::O2) {
    Ok(level) => level,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };

  let wants_toolchain = paths.contains_key(&emit::EmitKindArg::Executable);
  let toolchain = if wants_toolchain
    && (cli.clang.is_some()
      || cli.llvm_objcopy.is_some()
      || cli.llvm_objdump.is_some()
      || cli.sysroot.is_some()
      || !cli.link_arg.is_empty())
  {
    match native_js::Toolchain::detect_with_overrides(
      cli.clang.clone(),
      cli.llvm_objcopy.clone(),
      cli.llvm_objdump.clone(),
      cli.sysroot.clone(),
      cli.link_arg.clone(),
    ) {
      Ok(tc) => Some(tc),
      Err(err) => return exit_internal(&program, cli.json, render, err.to_string()),
    }
  } else {
    None
  };

  let mut base_opts = native_js::CompilerOptions::default();
  base_opts.debug = cli.debug;
  base_opts.debug_path_prefix_map = cli
    .debug_prefix_map
    .iter()
    .cloned()
    .map(|m| (m.from, m.to))
    .collect();
  base_opts.print_commands = cli.verbose;
  base_opts.keep_temp = cli.keep_temp;
  base_opts.pie = cli.pie;
  base_opts.target = target.clone();
  base_opts.opt_level = opt;
  base_opts.toolchain = toolchain;

  // When `build --emit-ir <PATH>` is used, only write the extra `.ll` once even if multiple
  // primary emits are requested.
  let mut wrote_extra_ir = false;

  for kind in paths.keys().copied() {
    if kind == emit::EmitKindArg::Hir {
      continue;
    }
    let Some(native_kind) = kind.as_native_emit_kind() else {
      continue;
    };
    let out_path = paths
      .get(&kind)
      .expect("emit output path for requested kind")
      .to_path_buf();

    let mut opts = base_opts.clone();
    opts.emit = native_kind;
    opts.output = Some(out_path);
    if !wrote_extra_ir {
      opts.emit_ir = args.emit_ir.clone();
      wrote_extra_ir = opts.emit_ir.is_some();
    }

    if let Err(err) = native_js::compile_program(&program, entry_file, &opts) {
      if let Some(diags) = err.diagnostics() {
        let _ = output::emit_diagnostics(&program, diags.to_vec(), cli.json, render);
        return ExitCode::from(diag::exit_code_for_diagnostics(diags));
      }
      return exit_internal(&program, cli.json, render, err.to_string());
    }
  }

  // Emit JSON even on success so callers can depend on a stable output shape.
  if cli.json {
    let _ = output::emit_diagnostics(&program, Vec::new(), true, render);
  }

  ExitCode::SUCCESS
}

fn cmd_emit(
  cli: &Cli,
  args: &cli_args::EmitArgs,
  target: &Option<target_lexicon::Triple>,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let (program, entry_file) = match project_load::load_program(
    cli.project.as_deref(),
    &args.entry,
    project_load::LoadMode::Checked,
  ) {
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

  let paths =
    match emit::compute_emit_paths(&args.emit, args.out_dir.as_deref(), args.output.as_deref(), "out") {
      Ok(paths) => paths,
      Err(err) => return exit_internal(&program, cli.json, render, err),
    };

  // Ensure output directories exist before writing any artifacts.
  for path in paths.values() {
    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        if let Err(err) = std::fs::create_dir_all(parent) {
          return exit_internal(
            &program,
            cli.json,
            render,
            format!("failed to create {}: {err}", parent.display()),
          );
        }
      }
    }
  }

  // HIR dump does not require LLVM codegen; run it first.
  if let Some(hir_path) = paths.get(&emit::EmitKindArg::Hir) {
    let diagnostics = program.check();
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
      let exit_code = ExitCode::from(diag::exit_code_for_diagnostics(&diagnostics));
      let _ = output::emit_diagnostics(&program, diagnostics, cli.json, render);
      return exit_code;
    }

    if let Err(err) = std::fs::write(hir_path, emit::format_hir_dump(&program)) {
      return exit_internal(
        &program,
        cli.json,
        render,
        format!("failed to write {}: {err}", hir_path.display()),
      );
    }
  }

  let opt = match opt_level_for_command(cli, native_js::OptLevel::O2) {
    Ok(level) => level,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };

  let wants_toolchain = paths.contains_key(&emit::EmitKindArg::Executable);
  let toolchain = if wants_toolchain
    && (cli.clang.is_some()
      || cli.llvm_objcopy.is_some()
      || cli.llvm_objdump.is_some()
      || cli.sysroot.is_some()
      || !cli.link_arg.is_empty())
  {
    match native_js::Toolchain::detect_with_overrides(
      cli.clang.clone(),
      cli.llvm_objcopy.clone(),
      cli.llvm_objdump.clone(),
      cli.sysroot.clone(),
      cli.link_arg.clone(),
    ) {
      Ok(tc) => Some(tc),
      Err(err) => return exit_internal(&program, cli.json, render, err.to_string()),
    }
  } else {
    None
  };

  let mut base_opts = native_js::CompilerOptions::default();
  base_opts.debug = cli.debug;
  base_opts.debug_path_prefix_map = cli
    .debug_prefix_map
    .iter()
    .cloned()
    .map(|m| (m.from, m.to))
    .collect();
  base_opts.print_commands = cli.verbose;
  base_opts.keep_temp = cli.keep_temp;
  base_opts.pie = cli.pie;
  base_opts.target = target.clone();
  base_opts.opt_level = opt;
  base_opts.toolchain = toolchain;

  for kind in paths.keys().copied() {
    if kind == emit::EmitKindArg::Hir {
      continue;
    }
    let Some(native_kind) = kind.as_native_emit_kind() else {
      continue;
    };
    let out_path = paths
      .get(&kind)
      .expect("emit output path for requested kind")
      .to_path_buf();

    let mut opts = base_opts.clone();
    opts.emit = native_kind;
    opts.output = Some(out_path);

    if let Err(err) = native_js::compile_program(&program, entry_file, &opts) {
      if let Some(diags) = err.diagnostics() {
        let _ = output::emit_diagnostics(&program, diags.to_vec(), cli.json, render);
        return ExitCode::from(diag::exit_code_for_diagnostics(diags));
      }
      return exit_internal(&program, cli.json, render, err.to_string());
    }
  }

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
  cmd_compile(
    cli,
    entry,
    native_js::EmitKind::LlvmIr,
    output_ll,
    None,
    target,
    render,
    native_js::OptLevel::O2,
  )
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

  let dir_path = dir.path().to_path_buf();
  let keep_dir = cli.keep_temp || cli.debug;
  let (dir_path, _keep_dir): (PathBuf, Option<tempfile::TempDir>) = if keep_dir {
    let path = dir_path.clone();
    let _ = dir.keep();
    eprintln!("kept tempdir: {}", path.display());
    (path, None)
  } else {
    (dir_path, Some(dir))
  };

  let exe = dir_path.join("out");
  let build_args = cli_args::BuildArgs {
    entry: entry.to_path_buf(),
    output: Some(exe.clone()),
    emit_ir: None,
    emit: Vec::new(),
    out_dir: None,
  };
  let build_exit = cmd_build(cli, &build_args, target, render);
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
  args: &cli_args::BenchArgs,
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

  let dir_path = dir.path().to_path_buf();
  let keep_dir = cli.keep_temp || cli.debug;
  let (dir_path, _keep_dir): (PathBuf, Option<tempfile::TempDir>) = if keep_dir {
    // Avoid emitting extra stderr in `--json` mode (tests expect stderr to be empty there).
    let path = dir_path.clone();
    let _ = dir.keep();
    if !cli.json {
      eprintln!("kept tempdir: {}", path.display());
    }
    (path, None)
  } else {
    (dir_path, Some(dir))
  };

  let exe = dir_path.join("out");

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
    let exit_code = exit_u8_for_diagnostics(&diagnostics);
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

  let opt = match opt_level_for_command(cli, native_js::OptLevel::O3) {
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
  opts.target = target.clone();
  opts.debug = cli.debug;
  opts.debug_path_prefix_map = cli
    .debug_prefix_map
    .iter()
    .cloned()
    .map(|m| (m.from, m.to))
    .collect();
  opts.print_commands = cli.verbose;
  opts.keep_temp = cli.keep_temp;
  opts.pie = cli.pie;
  opts.opt_level = opt;
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
      Err(err) => {
        let message = err.to_string();
        let compile_time_ms = bench::duration_ms(compile_start.elapsed());
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
    }
  }

  if let Err(err) = native_js::compile_program(&program, entry_file, &opts) {
    let compile_time_ms = bench::duration_ms(compile_start.elapsed());
    if let Some(diags) = err.diagnostics() {
      let exit_code = exit_u8_for_diagnostics(diags);
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

fn cmd_addr2line(cli: &Cli, args: &cli_args::Addr2LineArgs) -> ExitCode {
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
            stdin: args.stdin,
            strict: args.strict,
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
          stdin: args.stdin,
          strict: args.strict,
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

  let mut addr_inputs = args.addrs.clone();
  if args.stdin {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
      let line = match line {
        Ok(line) => line,
        Err(err) => {
          let msg = format!("failed to read stdin: {err}");
          if cli.json {
            let payload = Addr2LineJsonOutput {
              schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
              command: "addr2line",
              exe: args.exe.display().to_string(),
              base: base.map(|b| format!("0x{b:x}")),
              demangle: args.demangle,
              stdin: true,
              strict: args.strict,
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

      // Best-effort: scan each line for the first hex token.
      for tok in line.split_whitespace() {
        let tok = tok.trim_end_matches(|c: char| !c.is_ascii_hexdigit() && c != '_');
        if try_parse_hex_u64(tok).is_some() {
          addr_inputs.push(tok.to_string());
          break;
        }
      }
    }
  }

  if addr_inputs.is_empty() {
    let msg = if args.stdin {
      "no addresses found on stdin".to_string()
    } else {
      "no addresses provided".to_string()
    };
    if cli.json {
      let payload = Addr2LineJsonOutput {
        schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
        command: "addr2line",
        exe: args.exe.display().to_string(),
        base: base.map(|b| format!("0x{b:x}")),
        demangle: args.demangle,
        stdin: args.stdin,
        strict: args.strict,
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

  let mut results = Vec::with_capacity(addr_inputs.len());
  let mut had_unresolved = false;

  for raw_addr in &addr_inputs {
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
            stdin: args.stdin,
            strict: args.strict,
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
              stdin: args.stdin,
              strict: args.strict,
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
            stdin: args.stdin,
            strict: args.strict,
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
              stdin: args.stdin,
              strict: args.strict,
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

    if args.strict && (file.is_none() || line.is_none()) {
      had_unresolved = true;
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
    let exit_code = if args.strict && had_unresolved { 1 } else { 0 };
    let payload = Addr2LineJsonOutput {
      schema_version: ADDR2LINE_JSON_SCHEMA_VERSION,
      command: "addr2line",
      exe: args.exe.display().to_string(),
      base: base.map(|b| format!("0x{b:x}")),
      demangle: args.demangle,
      stdin: args.stdin,
      strict: args.strict,
      results,
      error: None,
      exit_code,
    };
    emit_json(&payload);
  }

  if args.strict && had_unresolved {
    ExitCode::from(1)
  } else {
    ExitCode::SUCCESS
  }
}

fn parse_hex_u64(raw: &str) -> Result<u64, String> {
  let raw = raw.trim();
  let no_prefix = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")).unwrap_or(raw);
  let no_underscores = no_prefix.replace('_', "");
  u64::from_str_radix(&no_underscores, 16)
    .map_err(|err| format!("invalid hex address `{raw}`: {err}"))
}

fn try_parse_hex_u64(raw: &str) -> Option<u64> {
  let raw = raw.trim();
  let raw = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")).unwrap_or(raw);

  let mut out: u64 = 0;
  let mut has_digit = false;

  for b in raw.bytes() {
    if b == b'_' {
      continue;
    }
    let digit = match b {
      b'0'..=b'9' => u64::from(b - b'0'),
      b'a'..=b'f' => u64::from(b - b'a') + 10,
      b'A'..=b'F' => u64::from(b - b'A') + 10,
      _ => return None,
    };
    has_digit = true;
    out = out.checked_mul(16)?;
    out = out.checked_add(digit)?;
  }

  has_digit.then_some(out)
}

fn cmd_compile(
  cli: &Cli,
  entry: &Path,
  emit: native_js::EmitKind,
  output_path: &Path,
  emit_ir: Option<&Path>,
  target: &Option<target_lexicon::Triple>,
  render: diagnostics::render::RenderOptions,
  default_opt: native_js::OptLevel,
) -> ExitCode {
  let (program, entry_file) = match project_load::load_program(
    cli.project.as_deref(),
    entry,
    project_load::LoadMode::Checked,
  ) {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  // `compile_program` already runs typechecking, strict-subset validation, and entrypoint checks.
  // We only pre-run the legacy strict validator when explicitly requested.
  if cli.extra_strict {
    let diagnostics = collect_check_diagnostics(&program, entry_file, true);
    let exit_code = exit_code_for_diagnostics(&diagnostics);
    if exit_code != ExitCode::SUCCESS {
      let _ = output::emit_diagnostics(&program, diagnostics, cli.json, render);
      return exit_code;
    }
  }

  let mut opts = native_js::CompilerOptions::default();
  opts.emit = emit;
  opts.output = Some(output_path.to_path_buf());
  opts.emit_ir = emit_ir.map(|p| p.to_path_buf());
  opts.target = target.clone();
  opts.debug = cli.debug;
  opts.debug_path_prefix_map = cli
    .debug_prefix_map
    .iter()
    .cloned()
    .map(|m| (m.from, m.to))
    .collect();
  opts.print_commands = cli.verbose;
  opts.keep_temp = cli.keep_temp;
  opts.pie = cli.pie;
  opts.opt_level = match opt_level_for_command(cli, default_opt) {
    Ok(level) => level,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };
  if matches!(emit, native_js::EmitKind::Executable)
    && (cli.clang.is_some()
      || cli.llvm_objcopy.is_some()
      || cli.llvm_objdump.is_some()
      || cli.sysroot.is_some()
      || !cli.link_arg.is_empty())
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
      return exit_code_for_diagnostics(diags);
    }
    return exit_internal(&program, cli.json, render, err.to_string());
  }

  // Emit JSON even on success so callers can depend on a stable output shape.
  if cli.json {
    let _ = output::emit_diagnostics(&program, Vec::new(), true, render);
  }

  ExitCode::SUCCESS
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
    other => Err(format!("invalid --opt-level={other} (expected 0,1,2,3)")),
  }
}

fn opt_level_for_command(cli: &Cli, default: native_js::OptLevel) -> Result<native_js::OptLevel, String> {
  if let Some(raw) = cli.opt_level {
    opt_level(raw)
  } else if cli.release {
    Ok(native_js::OptLevel::O3)
  } else if cli.debug {
    Ok(native_js::OptLevel::O0)
  } else {
    Ok(default)
  }
}

fn exit_code_for_diagnostics(diagnostics: &[Diagnostic]) -> ExitCode {
  let has_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);
  if !has_errors {
    return ExitCode::SUCCESS;
  }

  let has_internal = diagnostics.iter().any(|d| {
    d.severity == Severity::Error
      && (d.code.as_str().starts_with("ICE") || d.code.as_str().starts_with("HOST"))
  });
  if has_internal {
    ExitCode::from(2)
  } else {
    ExitCode::from(1)
  }
}

fn exit_u8_for_diagnostics(diagnostics: &[Diagnostic]) -> u8 {
  let has_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);
  if !has_errors {
    return 0;
  }

  let has_internal = diagnostics.iter().any(|d| {
    d.severity == Severity::Error
      && (d.code.as_str().starts_with("ICE") || d.code.as_str().starts_with("HOST"))
  });
  if has_internal { 2 } else { 1 }
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
