use clap::{ArgAction, Args, Parser, Subcommand};
use diagnostics::{host_error, Diagnostic, FileId, Severity, Span, TextRange};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use tempfile::tempdir;
use typecheck_ts::lib_support::{LibName, ScriptTarget};
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
  entry: &Path,
  output_exe: &Path,
  emit_ir: Option<&Path>,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let (program, entry_file) = match load_program(cli, entry) {
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
  opts.emit = native_js::EmitKind::Executable;
  opts.output = Some(output_exe.to_path_buf());
  opts.emit_ir = emit_ir.map(|p| p.to_path_buf());
  opts.debug = cli.debug;
  opts.opt_level = match opt_level(cli.opt) {
    Ok(level) => level,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };

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

fn cmd_emit_ir(
  cli: &Cli,
  entry: &Path,
  output_ll: &Path,
  render: diagnostics::render::RenderOptions,
) -> ExitCode {
  let (program, entry_file) = match load_program(cli, entry) {
    Ok(res) => res,
    Err(err) => return exit_internal_without_program(cli.json, err),
  };

  if cli.extra_strict {
    let diagnostics = collect_check_diagnostics(&program, entry_file, true);
    let exit_code = exit_code_for_diagnostics(&diagnostics);
    if exit_code != ExitCode::SUCCESS {
      let _ = output::emit_diagnostics(&program, diagnostics, cli.json, render);
      return exit_code;
    }
  }

  let mut opts = native_js::CompilerOptions::default();
  opts.emit = native_js::EmitKind::LlvmIr;
  opts.output = Some(output_ll.to_path_buf());
  opts.debug = cli.debug;
  opts.opt_level = match opt_level(cli.opt) {
    Ok(level) => level,
    Err(err) => return exit_internal(&program, cli.json, render, err),
  };

  if let Err(err) = native_js::compile_program(&program, entry_file, &opts) {
    if let Some(diags) = err.diagnostics() {
      let _ = output::emit_diagnostics(&program, diags.to_vec(), cli.json, render);
      return exit_code_for_diagnostics(diags);
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

fn load_program(cli: &Cli, entry: &Path) -> Result<(Program, FileId), String> {
  let project = match cli.project.as_deref() {
    Some(path) => Some(tsconfig::load_project_config(path)?),
    None => None,
  };
  let mut compiler_options = project
    .as_ref()
    .map(|cfg| cfg.compiler_options.clone())
    .unwrap_or_default();

  // TypeScript defaults to loading `dom` + an ES lib when `compilerOptions.lib` is not provided.
  // For native-js, the DOM lib is unnecessary (we're targeting native executables / Node-like
  // environments) and adds significant startup cost during typechecking.
  //
  // When the user did not specify `lib` and did not opt out via `no_default_lib`, default to the
  // target ES lib only.
  if compiler_options.libs.is_empty() && !compiler_options.no_default_lib {
    let es_lib = match compiler_options.target {
      ScriptTarget::Es3 | ScriptTarget::Es5 => "es5",
      ScriptTarget::Es2015 => "es2015",
      ScriptTarget::Es2016 => "es2016",
      ScriptTarget::Es2017 => "es2017",
      ScriptTarget::Es2018 => "es2018",
      ScriptTarget::Es2019 => "es2019",
      ScriptTarget::Es2020 => "es2020",
      ScriptTarget::Es2021 => "es2021",
      ScriptTarget::Es2022 => "es2022",
      ScriptTarget::EsNext => "esnext",
    };
    compiler_options.libs.push(
      LibName::parse(es_lib).expect("built-in ES lib name should parse as a LibName"),
    );
    if matches!(compiler_options.target, ScriptTarget::EsNext) {
      compiler_options.libs.push(
        LibName::parse("esnext.disposable")
          .expect("built-in ES lib name should parse as a LibName"),
      );
    }
  }
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
  let mut extra_libs = extra_libs;
  extra_libs.push(native_js::builtins::native_js_builtins_lib());
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

fn opt_level(raw: u8) -> Result<native_js::OptLevel, String> {
  match raw {
    0 => Ok(native_js::OptLevel::O0),
    1 => Ok(native_js::OptLevel::O1),
    2 => Ok(native_js::OptLevel::O2),
    3 => Ok(native_js::OptLevel::O3),
    other => Err(format!("invalid --opt={other} (expected 0,1,2,3)")),
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

fn exit_internal_without_program(json: bool, message: String) -> ExitCode {
  if json {
    let diagnostic = host_error(None, message.clone());
    if output::emit_json_diagnostics(vec![diagnostic]).is_err() {
      eprintln!("{message}");
    }
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
