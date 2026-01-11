use clap::{ArgAction, Args, Parser, Subcommand};
use diagnostics::{host_error, Diagnostic, FileId, Severity, Span, TextRange};
use inkwell::context::Context;
use inkwell::OptimizationLevel;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use tempfile::tempdir;
use typecheck_ts::resolve::{canonicalize_path, NodeResolver, ResolveOptions};
use typecheck_ts::Program;

#[path = "../host.rs"]
mod host;

#[path = "../output.rs"]
mod output;

#[path = "../tsconfig.rs"]
mod tsconfig;

#[path = "../type_libs.rs"]
mod type_libs;

#[derive(Parser, Debug)]
#[command(author, version, about = "Compile TypeScript to native executables via native-js (LLVM)")]
struct Cli {
  #[command(subcommand)]
  command: Commands,

  /// TypeScript project file (tsconfig.json) to load.
  #[arg(long, short = 'p', global = true)]
  project: Option<PathBuf>,

  /// Emit JSON diagnostics to stdout (schema_version = 1).
  #[arg(long, global = true)]
  json: bool,

  /// Force-enable ANSI colors in diagnostics output.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  color: bool,

  /// Disable ANSI colors in diagnostics output.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  no_color: bool,

  /// Optimization level (0-3).
  #[arg(long, default_value_t = 2, global = true)]
  opt: u8,

  /// Best-effort debug build (passes `-g` to the system linker).
  #[arg(long, global = true)]
  debug: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Type-check and validate the native-js strict subset.
  Check(CheckArgs),
  /// Compile an entry file to a native executable.
  Build(BuildArgs),
  /// Compile an entry file and run it immediately.
  Run(RunArgs),
  /// Emit LLVM IR to a file.
  EmitIr(EmitIrArgs),
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
struct EmitIrArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  entry: PathBuf,

  /// Output path for the emitted LLVM IR.
  #[arg(short = 'o', long, value_name = "PATH.ll")]
  output: PathBuf,
}

fn main() -> ExitCode {
  let cli = Cli::parse();

  if cli.json && matches!(&cli.command, Commands::Run(_)) {
    eprintln!("--json is not supported with `run` (it would mix with program stdout)");
    return ExitCode::from(2);
  }

  let render = output::render_options(cli.color, cli.no_color);
  match &cli.command {
    Commands::Check(args) => cmd_check(&cli, &args.entry, render),
    Commands::Build(args) => cmd_build(
      &cli,
      &args.entry,
      &args.output,
      args.emit_ir.as_deref(),
      render,
    ),
    Commands::Run(args) => cmd_run(&cli, &args.entry, &args.args, render),
    Commands::EmitIr(args) => cmd_emit_ir(&cli, &args.entry, &args.output, render),
  }
}

fn cmd_check(cli: &Cli, entry: &Path, render: diagnostics::render::RenderOptions) -> ExitCode {
  let (program, entry_file) = match load_program(cli, entry) {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  let diagnostics = collect_check_diagnostics(&program, entry_file);
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
  entry: &Path,
  output_exe: &Path,
  emit_ir: Option<&Path>,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  if let Some(parent) = output_exe.parent() {
    if !parent.as_os_str().is_empty() {
      if let Err(err) = fs::create_dir_all(parent) {
        return exit_internal_without_program(
          cli.json,
          format!("failed to create output directory {}: {err}", parent.display()),
        );
      }
    }
  }

  let (program, entry_file) = match load_program(cli, entry) {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  // Typecheck + strict validation + entrypoint diagnostics.
  let diagnostics = collect_check_diagnostics(&program, entry_file);
  let exit_code = exit_code_for_diagnostics(&diagnostics);
  if exit_code != ExitCode::SUCCESS {
    let _ = output::emit_diagnostics(&program, diagnostics, cli.json, render);
    return exit_code;
  }

  let entrypoint = match native_js::strict::entrypoint(&program, entry_file) {
    Ok(ep) => ep,
    Err(diags) => {
      let _ = output::emit_diagnostics(&program, diags, cli.json, render);
      return ExitCode::from(1);
    }
  };

  let context = Context::create();
  let module = match native_js::codegen::codegen(
    &context,
    &program,
    entry_file,
    entrypoint,
    native_js::codegen::CodegenOptions::default(),
  ) {
    Ok(module) => module,
    Err(diags) => {
      let _ = output::emit_diagnostics(&program, diags, cli.json, render);
      return ExitCode::from(1);
    }
  };

  if let Some(path) = emit_ir {
    if let Some(parent) = path.parent() {
      if !parent.as_os_str().is_empty() {
        if let Err(err) = fs::create_dir_all(parent) {
          return exit_internal(
            &program,
            cli.json,
            render,
            format!("failed to create output directory {}: {err}", parent.display()),
          );
        }
      }
    }
    let ir = native_js::emit::emit_llvm_ir(&module);
    if let Err(err) = fs::write(path, ir) {
      return exit_internal(
        &program,
        cli.json,
        render,
        format!("failed to write {}: {err}", path.display()),
      );
    }
  }

  let mut target = native_js::emit::TargetConfig::default();
  target.opt_level = match opt_level(cli.opt) {
    Ok(level) => level,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };
  let obj = match native_js::emit::emit_object_with_statepoints(&module, target) {
    Ok(obj) => obj,
    Err(err) => return exit_internal(&program, cli.json, render, err.to_string()),
  };

  if let Err(err) = link_object(cli.debug, &obj, output_exe) {
    return exit_internal(&program, cli.json, render, err);
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
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  if let Some(parent) = output_ll.parent() {
    if !parent.as_os_str().is_empty() {
      if let Err(err) = fs::create_dir_all(parent) {
        return exit_internal_without_program(
          cli.json,
          format!("failed to create output directory {}: {err}", parent.display()),
        );
      }
    }
  }

  let (program, entry_file) = match load_program(cli, entry) {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  let diagnostics = collect_check_diagnostics(&program, entry_file);
  let exit_code = exit_code_for_diagnostics(&diagnostics);
  if exit_code != ExitCode::SUCCESS {
    let _ = output::emit_diagnostics(&program, diagnostics, cli.json, render);
    return exit_code;
  }

  let entrypoint = match native_js::strict::entrypoint(&program, entry_file) {
    Ok(ep) => ep,
    Err(diags) => {
      let _ = output::emit_diagnostics(&program, diags, cli.json, render);
      return ExitCode::from(1);
    }
  };

  let context = Context::create();
  let module = match native_js::codegen::codegen(
    &context,
    &program,
    entry_file,
    entrypoint,
    native_js::codegen::CodegenOptions::default(),
  ) {
    Ok(module) => module,
    Err(diags) => {
      let _ = output::emit_diagnostics(&program, diags, cli.json, render);
      return ExitCode::from(1);
    }
  };

  let ir = native_js::emit::emit_llvm_ir(&module);
  if let Err(err) = fs::write(output_ll, ir) {
    return exit_internal(
      &program,
      cli.json,
      render,
      format!("failed to write {}: {err}", output_ll.display()),
    );
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
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let dir = match tempdir() {
    Ok(dir) => dir,
    Err(err) => {
      return exit_internal_without_program(cli.json, format!("failed to create tempdir: {err}"));
    }
  };
  let exe = dir.path().join("out");

  let build_exit = cmd_build(cli, entry, &exe, None, render);
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

  let code = status.code().unwrap_or(1);
  println!("{code}");
  ExitCode::SUCCESS
}

fn collect_check_diagnostics(program: &Program, entry_file: FileId) -> Vec<Diagnostic> {
  let mut diagnostics = program.check();
  let has_type_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);

  if !has_type_errors {
    if let Err(diags) = native_js::validate::validate_strict_subset(program) {
      diagnostics.extend(diags);
    }

    if let Err(diags) = native_js::strict::entrypoint(program, entry_file) {
      diagnostics.extend(diags);
    }
  }

  diagnostics::sort_diagnostics(&mut diagnostics);
  diagnostics
}

fn load_program(cli: &Cli, entry: &Path) -> Result<(Program, FileId), String> {
  let project = match cli.project.as_deref() {
    Some(path) => Some(tsconfig::load_project_config(path)?),
    None => None,
  };
  let mut compiler_options = project
    .as_ref()
    .map(|cfg| cfg.compiler_options.clone())
    .unwrap_or_default();
  let (type_roots, extra_libs) = match project.as_ref() {
    Some(cfg) => {
      let type_roots = cfg
        .type_roots
        .clone()
        .unwrap_or_else(|| type_libs::default_type_roots(&cfg.root_dir));
      let libs = type_libs::load_type_libs(cfg, &compiler_options, &type_roots)?;
      // The CLI loads `typeRoots`/`types` packages as host-provided libs (ambient `.d.ts` inputs),
      // matching `tsc` more closely. Clear the compiler option so `typecheck-ts` doesn't also try
      // to resolve them via module resolution.
      compiler_options.types.clear();
      (type_roots, libs)
    }
    None => (Vec::new(), Vec::new()),
  };
  let resolve_options = ResolveOptions {
    node_modules: true,
    package_imports: true,
  };

  let entry_canonical =
    canonicalize_path(entry).map_err(|err| format!("failed to read entry {}: {err}", entry.display()))?;

  let mut root_paths = Vec::new();
  if let Some(cfg) = project.as_ref() {
    root_paths.extend(cfg.root_files.iter().cloned());
  }
  root_paths.push(entry_canonical.clone());
  root_paths.sort_by(|a, b| a.display().to_string().cmp(&b.display().to_string()));
  root_paths.dedup();

  let resolver = host::ModuleResolver {
    resolver: NodeResolver::new(resolve_options),
    tsconfig: project.as_ref().and_then(host::TsconfigResolver::from_project),
  };

  let (host, roots) = host::DiskHost::new(&root_paths, resolver, compiler_options, extra_libs, type_roots)?;
  let entry_key = host
    .key_for_path(&entry_canonical)
    .ok_or_else(|| format!("entry file not loaded: {}", entry.display()))?;
  let program = Program::new(host, roots);
  let entry_file = program
    .file_id(&entry_key)
    .ok_or_else(|| format!("entry file not loaded: {}", entry.display()))?;

  Ok((program, entry_file))
}

fn opt_level(raw: u8) -> Result<OptimizationLevel, String> {
  match raw {
    0 => Ok(OptimizationLevel::None),
    1 => Ok(OptimizationLevel::Less),
    2 => Ok(OptimizationLevel::Default),
    3 => Ok(OptimizationLevel::Aggressive),
    other => Err(format!("invalid --opt={other} (expected 0,1,2,3)")),
  }
}

fn link_object(debug: bool, obj: &[u8], output: &Path) -> Result<(), String> {
  let tmpdir = tempfile::tempdir().map_err(|err| format!("failed to create tempdir: {err}"))?;
  let obj_path = tmpdir.path().join("out.o");
  fs::write(&obj_path, obj).map_err(|err| format!("failed to write {}: {err}", obj_path.display()))?;

  let clang = find_clang().ok_or_else(|| "failed to find clang (tried clang-18 and clang)".to_string())?;
  let mut cmd = Command::new(clang);
  cmd.arg(&obj_path).arg("-o").arg(output);
  if debug {
    cmd.arg("-g");
  }
  let status = cmd.status().map_err(|err| format!("failed to invoke clang: {err}"))?;
  if !status.success() {
    return Err(format!("clang failed with status {}", status.code().unwrap_or(1)));
  }
  Ok(())
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

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return Some(cand);
    }
  }
  None
}

fn exit_internal_without_program(json: bool, message: String) -> ExitCode {
  if json {
    // No program snapshot available; emit a minimal message to stderr so CI doesn't treat it as a
    // silent failure.
    eprintln!("{message}");
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
