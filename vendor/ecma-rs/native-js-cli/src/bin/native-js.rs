use clap::{Args, Parser, Subcommand, ValueEnum};
use diagnostics::render::{render_diagnostic_with_options, RenderOptions, SourceProvider};
use diagnostics::{Diagnostic, FileId, Severity};
use inkwell::context::Context;
use inkwell::OptimizationLevel;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use typecheck_ts::resolve::{canonicalize_path, NodeResolver, ResolveOptions};
use typecheck_ts::Program;

#[path = "../tsconfig.rs"]
mod tsconfig;

#[path = "../host.rs"]
mod host;

#[path = "../type_libs.rs"]
mod type_libs;

#[derive(Parser, Debug)]
#[command(
  author,
  version,
  about = "Compile TypeScript to native executables via native-js (LLVM)"
)]
struct Cli {
  #[command(subcommand)]
  command: Commands,

  /// TypeScript project file (tsconfig.json) to load.
  #[arg(long, short = 'p', global = true)]
  project: Option<PathBuf>,

  /// Also emit an intermediate artifact.
  #[arg(long, value_enum, global = true)]
  emit: Option<EmitKind>,

  /// Path to write the emitted artifact to (required with --emit).
  #[arg(long, value_name = "PATH", global = true)]
  emit_path: Option<PathBuf>,

  /// Optimization level (0-3).
  #[arg(long, default_value_t = 2, global = true)]
  opt: u8,

  /// Best-effort debug build (passes `-g` to the system linker).
  #[arg(long, global = true)]
  debug: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Typecheck + validate an entry file (no executable is produced).
  Check(CheckArgs),
  /// Compile an entry file to a native executable.
  Build(BuildArgs),
  /// Compile an entry file and run it immediately.
  Run(RunArgs),
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
}

#[derive(Args, Debug)]
struct RunArgs {
  /// Entry TypeScript file (must export `main()`).
  #[arg(value_name = "PATH")]
  entry: PathBuf,

  /// Arguments to pass to the generated executable.
  #[arg(trailing_var_arg = true, value_name = "ARGS")]
  args: Vec<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EmitKind {
  LlvmIr,
  Bc,
  Obj,
  Asm,
}

fn main() {
  let cli = Cli::parse();
  if cli.emit.is_some() && cli.emit_path.is_none() {
    eprintln!("--emit-path is required when using --emit");
    exit(2);
  }

  match &cli.command {
    Commands::Check(args) => {
      if let Err(err) = check(&cli, &args.entry) {
        eprintln!("{err}");
        exit(1);
      }
    }
    Commands::Build(args) => {
      if let Err(err) = build(&cli, &args.entry, &args.output) {
        eprintln!("{err}");
        exit(1);
      }
    }
    Commands::Run(args) => {
      let tmp = tempfile::tempdir().unwrap_or_else(|err| {
        eprintln!("failed to create tempdir: {err}");
        exit(1);
      });
      let exe = tmp.path().join("out");
      if let Err(err) = build(&cli, &args.entry, &exe) {
        eprintln!("{err}");
        exit(1);
      }
      let status = Command::new(&exe)
        .args(&args.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .unwrap_or_else(|err| {
          eprintln!("failed to run {}: {err}", exe.display());
          exit(1);
        });
      exit(status.code().unwrap_or(1));
    }
  }
}

fn check(cli: &Cli, entry: &Path) -> Result<(), String> {
  let (program, host, entry_file) = load_program(cli, entry)?;

  let mut diagnostics = program.check();
  if diagnostics.iter().any(|d| d.severity == Severity::Error) {
    render_and_print(&program, &host, &diagnostics);
    return Err("TypeScript type checking failed".into());
  }

  diagnostics = native_js::strict::validate(&program, &program.reachable_files());
  if !diagnostics.is_empty() {
    render_and_print(&program, &host, &diagnostics);
    return Err("native-js strict validation failed".into());
  }

  let entrypoint = native_js::strict::entrypoint(&program, entry_file).map_err(|diags| {
    render_and_print(&program, &host, &diags);
    "invalid entrypoint".to_string()
  })?;

  let context = Context::create();
  let module = native_js::codegen::codegen(
    &context,
    &program,
    entry_file,
    entrypoint,
    native_js::codegen::CodegenOptions::default(),
  )
  .map_err(|diags| {
    render_and_print(&program, &host, &diags);
    "native-js code generation failed".to_string()
  })?;

  let opt = opt_level(cli.opt)?;

  if let Some(kind) = cli.emit {
    let Some(path) = cli.emit_path.as_deref() else {
      unreachable!("validated --emit-path earlier");
    };

    // Emit the object first so `rewrite-statepoints-for-gc` runs exactly once on
    // the module. This keeps `--emit obj/asm` deterministic and avoids assuming
    // the pass is idempotent.
    let mut target = native_js::emit::TargetConfig::default();
    target.opt_level = opt;
    let obj = native_js::emit::emit_object_with_statepoints(&module, target)
      .map_err(|err| err.to_string())?;

    emit_artifact(&module, kind, path, opt, &obj)?;
  } else {
    // Ensure the LLVM target machine path works (without linking an executable).
    let mut target = native_js::emit::TargetConfig::default();
    target.opt_level = opt;
    let _obj = native_js::emit::emit_object(&module, target);
  }

  Ok(())
}

fn build(cli: &Cli, entry: &Path, output: &Path) -> Result<(), String> {
  if let Some(parent) = output.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent).map_err(|err| {
        format!(
          "failed to create output directory {}: {err}",
          parent.display()
        )
      })?;
    }
  }

  let (program, host, entry_file) = load_program(cli, entry)?;

  let diagnostics = program.check();
  if diagnostics.iter().any(|d| d.severity == Severity::Error) {
    render_and_print(&program, &host, &diagnostics);
    return Err("TypeScript type checking failed".into());
  }

  if let Err(diags) = native_js::validate::validate_strict_subset(&program) {
    render_and_print(&program, &host, &diags);
    return Err("native-js strict subset validation failed".into());
  }

  let entrypoint = native_js::strict::entrypoint(&program, entry_file).map_err(|diags| {
    render_and_print(&program, &host, &diags);
    "invalid entrypoint".to_string()
  })?;

  let context = Context::create();
  let module = native_js::codegen::codegen(
    &context,
    &program,
    entry_file,
    entrypoint,
    native_js::codegen::CodegenOptions::default(),
  )
  .map_err(|diags| {
    render_and_print(&program, &host, &diags);
    "native-js code generation failed".to_string()
  })?;

  let opt = opt_level(cli.opt)?;

  // Emit the object first so `rewrite-statepoints-for-gc` runs exactly once on
  // the module. This avoids the (non-guaranteed) assumption that the pass is
  // idempotent when `--emit obj/asm` is used.
  let mut target = native_js::emit::TargetConfig::default();
  target.opt_level = opt;
  let obj = native_js::emit::emit_object_with_statepoints(&module, target)
    .map_err(|err| err.to_string())?;

  if let Some(kind) = cli.emit {
    let Some(path) = cli.emit_path.as_deref() else {
      unreachable!("validated --emit-path earlier");
    };
    emit_artifact(&module, kind, path, opt, &obj)?;
  }

  link_object(cli.debug, &obj, output)?;

  Ok(())
}

fn load_program(cli: &Cli, entry: &Path) -> Result<(Program, host::DiskHost, FileId), String> {
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

  let entry_canonical = canonicalize_path(entry)
    .map_err(|err| format!("failed to read entry {}: {err}", entry.display()))?;

  let mut root_paths = Vec::new();
  if let Some(cfg) = project.as_ref() {
    root_paths.extend(cfg.root_files.iter().cloned());
  }
  root_paths.push(entry_canonical.clone());
  root_paths.sort_by(|a, b| a.display().to_string().cmp(&b.display().to_string()));
  root_paths.dedup();

  let resolver = host::ModuleResolver {
    resolver: NodeResolver::new(resolve_options),
    tsconfig: project
      .as_ref()
      .and_then(host::TsconfigResolver::from_project),
  };

  let (host, roots) = host::DiskHost::new(
    &root_paths,
    resolver,
    compiler_options,
    extra_libs,
    type_roots,
  )?;
  let program = Program::new(host.clone(), roots);

  let entry_key = host
    .key_for_path(&entry_canonical)
    .ok_or_else(|| format!("entry file not part of program: {}", entry.display()))?;
  let entry_file = program
    .file_id(&entry_key)
    .ok_or_else(|| format!("entry file not loaded: {}", entry.display()))?;

  Ok((program, host, entry_file))
}

fn emit_artifact(
  module: &inkwell::module::Module<'_>,
  kind: EmitKind,
  path: &Path,
  opt: OptimizationLevel,
  obj: &[u8],
) -> Result<(), String> {
  if let Some(parent) = path.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent).map_err(|err| {
        format!(
          "failed to create output directory {}: {err}",
          parent.display()
        )
      })?;
    }
  }

  let mut target = native_js::emit::TargetConfig::default();
  target.opt_level = opt;

  match kind {
    EmitKind::LlvmIr => {
      let ir = native_js::emit::emit_llvm_ir(module);
      fs::write(path, ir).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    }
    EmitKind::Bc => {
      let bc = native_js::emit::emit_bitcode(module);
      fs::write(path, bc).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    }
    EmitKind::Obj => {
      fs::write(path, obj).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    }
    EmitKind::Asm => {
      // The module was already rewritten during object emission. Avoid rerunning
      // `rewrite-statepoints-for-gc` by using the raw asm emission helper.
      let asm = native_js::emit::emit_asm(module, target);
      fs::write(path, asm).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    }
  }

  Ok(())
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
  fs::write(&obj_path, obj)
    .map_err(|err| format!("failed to write {}: {err}", obj_path.display()))?;

  let clang =
    find_clang().ok_or_else(|| "failed to find clang (tried clang-18 and clang)".to_string())?;

  let mut cmd = Command::new(clang);
  cmd.arg(&obj_path).arg("-o").arg(output);
  if debug {
    cmd.arg("-g");
  }
  let status = cmd
    .status()
    .map_err(|err| format!("failed to invoke clang: {err}"))?;
  if !status.success() {
    return Err(format!(
      "clang failed with status {}",
      status.code().unwrap_or(1)
    ));
  }
  Ok(())
}

fn find_clang() -> Option<&'static str> {
  if Command::new("clang-18")
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
  {
    return Some("clang-18");
  }
  if Command::new("clang")
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
  {
    return Some("clang");
  }
  None
}

fn render_and_print(program: &Program, host: &host::DiskHost, diagnostics: &[Diagnostic]) {
  let snapshot = snapshot_from_program(program, host);
  let options = RenderOptions {
    context_lines: 1,
    ..RenderOptions::default()
  };
  for diag in diagnostics {
    eprintln!(
      "{}",
      render_diagnostic_with_options(&snapshot, diag, options)
    );
  }
}

struct ProgramSourceSnapshot {
  names: HashMap<FileId, String>,
  texts: HashMap<FileId, String>,
}

impl SourceProvider for ProgramSourceSnapshot {
  fn file_name(&self, file: FileId) -> Option<&str> {
    self.names.get(&file).map(|s| s.as_str())
  }

  fn file_text(&self, file: FileId) -> Option<&str> {
    self.texts.get(&file).map(|s| s.as_str())
  }
}

fn snapshot_from_program(program: &Program, host: &host::DiskHost) -> ProgramSourceSnapshot {
  let mut names = HashMap::new();
  let mut texts = HashMap::new();

  for file in program.files() {
    if let Some(key) = program.file_key(file) {
      let name = host
        .path_for_key(&key)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| key.to_string());
      names.insert(file, name);
    }
    if let Some(text) = program.file_text(file) {
      texts.insert(file, text.to_string());
    }
  }

  ProgramSourceSnapshot { names, texts }
}
