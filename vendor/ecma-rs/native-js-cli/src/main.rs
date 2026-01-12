mod host;
mod output;
mod project_load;
mod type_libs;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use native_js::compiler::compile_llvm_ir_to_artifact;
use native_js::{
  compile_program, compile_project_to_llvm_ir, compile_typescript_to_llvm_ir, CompileOptions,
  EmitKind, NativeJsError, OptLevel,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use typecheck_ts::Program;

#[derive(Parser, Debug)]
#[command(author, version)]
struct Cli {
  #[command(subcommand)]
  command: Option<Commands>,

  /// TypeScript input file to compile and run (default command).
  #[arg(value_name = "PATH")]
  input: Option<PathBuf>,

  /// TypeScript project file (`tsconfig.json`) to load.
  ///
  /// When set, module resolution honors `compilerOptions.baseUrl` / `paths`, and `typeRoots` /
  /// `types` packages are loaded (matching `native-js` behavior).
  ///
  /// The path can be either a directory (meaning `<dir>/tsconfig.json`) or an explicit
  /// `tsconfig.json` path.
  #[arg(long, short = 'p', value_name = "PATH|DIR", global = true)]
  project: Option<PathBuf>,

  /// Exported function in the entry module to call after module initialization.
  ///
  /// If omitted and the entry module exports `main()`, it is invoked automatically. Otherwise,
  /// only top-level module initializers are executed.
  ///
  /// This flag is only supported with `--pipeline project`.
  #[arg(long, value_name = "NAME", global = true)]
  entry_fn: Option<String>,

  /// Disable recognizing builtin calls like `console.log`, `print`, and `assert`.
  #[arg(long, global = true)]
  no_builtins: bool,

  /// Produce a PIE executable (ET_DYN) on Linux.
  ///
  /// By default native-js links non-PIE so LLVM stackmap relocations are resolved at link time.
  #[arg(long, global = true)]
  pie: bool,

  /// Keep the generated LLVM IR for debugging.
  #[arg(long, value_name = "PATH", global = true)]
  emit_llvm: Option<PathBuf>,

  /// Which compilation pipeline to use.
  ///
  /// - `project`: legacy `parse-js` based emitter (keeps compiling even with TS type errors).
  /// - `checked`: typechecked `native_js::compile_program` pipeline (fails on type errors).
  #[arg(long, value_enum, default_value = "project", global = true)]
  pipeline: Pipeline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Pipeline {
  Project,
  Checked,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Check that input sources can be compiled (no executable output).
  Check {
    /// Entry files to check.
    entries: Vec<PathBuf>,
  },

  /// Build an executable from TypeScript sources.
  Build {
    /// Entry file.
    entry: PathBuf,
    /// Output path for the executable.
    #[arg(short, long, value_name = "PATH")]
    output: PathBuf,
  },

  /// Build and run an executable from TypeScript sources.
  Run {
    /// Entry file.
    entry: PathBuf,
    /// Arguments forwarded to the produced binary after `--`.
    #[arg(last = true)]
    args: Vec<String>,
  },

  /// Emit LLVM IR for the given entry file.
  #[command(name = "emit-ir")]
  EmitIr {
    /// Entry file.
    entry: PathBuf,
    /// Output path for the `.ll` file (defaults to stdout).
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
  },
}

fn main() {
  let cli = Cli::parse();

  match cli.command.as_ref() {
    Some(Commands::Check { entries }) => {
      if entries.is_empty() {
        eprintln!("native-js-cli check: expected at least one entry file");
        exit(2);
      }

      match cli.pipeline {
        Pipeline::Project => {
          for entry in entries {
            let _ = compile_file_to_ir(&cli, entry);
          }
        }
        Pipeline::Checked => {
          ensure_checked_pipeline_supported(&cli);

          let mut ok = true;
          for entry in entries {
            let tmpdir = tempfile::tempdir().unwrap_or_else(|err| {
              eprintln!("failed to create tempdir: {err}");
              exit(1);
            });
            let out = tmpdir.path().join("out.ll");
            if compile_file_checked(&cli, entry, EmitKind::LlvmIr, Some(out)).is_err() {
              ok = false;
            }
          }

          if !ok {
            exit(1);
          }
        }
      }
    }
    Some(Commands::Build { entry, output }) => match cli.pipeline {
      Pipeline::Project => {
        let ir = compile_file_to_ir(&cli, entry);
        write_ir_debug(&cli, &ir);

        let mut opts = CompileOptions::default();
        opts.builtins = !cli.no_builtins;
        // The `project` pipeline is intended for quick iteration and can emit LLVM IR that fails
        // strict validation (it keeps compiling even with type errors). Keep compilation fast by
        // disabling LLVM optimizations.
        opts.opt_level = OptLevel::O0;
        opts.emit = EmitKind::Executable;
        opts.pie = cli.pie;

        if let Err(err) = compile_llvm_ir_to_artifact(&ir, opts, Some(output.clone())) {
          eprintln!("{err}");
          exit(1);
        }
      }
      Pipeline::Checked => {
        ensure_checked_pipeline_supported(&cli);

        let _ =
          compile_file_checked(&cli, entry, EmitKind::Executable, Some(output.to_path_buf()))
            .map_err(|()| exit(1));
      }
    },
    Some(Commands::Run { entry, args }) => match cli.pipeline {
      Pipeline::Project => {
        let ir = compile_file_to_ir(&cli, entry);
        write_ir_debug(&cli, &ir);

        let mut opts = CompileOptions::default();
        opts.builtins = !cli.no_builtins;
        opts.opt_level = OptLevel::O0;
        opts.emit = EmitKind::Executable;
        opts.pie = cli.pie;

        let code = {
          let out = match compile_llvm_ir_to_artifact(&ir, opts, None) {
            Ok(out) => out,
            Err(err) => {
              eprintln!("{err}");
              exit(1);
            }
          };
          run_exe(&out.path, args)
        };
        exit(code);
      }
      Pipeline::Checked => {
        ensure_checked_pipeline_supported(&cli);

        let tmpdir = tempfile::tempdir().unwrap_or_else(|err| {
          eprintln!("failed to create tempdir: {err}");
          exit(1);
        });
        let exe = tmpdir.path().join("out");
        let _keep_tmpdir = tmpdir;

        let _ = compile_file_checked(&cli, entry, EmitKind::Executable, Some(exe.clone()))
          .map_err(|()| exit(1));

        let code = run_exe(&exe, args);
        exit(code);
      }
    },
    Some(Commands::EmitIr { entry, output }) => match cli.pipeline {
      Pipeline::Project => {
        let ir = compile_file_to_ir(&cli, entry);
        if let Some(path) = output.as_deref() {
          if let Err(err) = fs::write(path, &ir) {
            eprintln!("failed to write {}: {err}", path.display());
            exit(1);
          }
        } else {
          print!("{ir}");
        }
      }
      Pipeline::Checked => {
        ensure_checked_pipeline_supported(&cli);

        if let Some(path) = output.as_deref() {
          let _ = compile_file_checked(&cli, entry, EmitKind::LlvmIr, Some(path.to_path_buf()))
            .map_err(|()| exit(1));
        } else {
          let tmpdir = tempfile::tempdir().unwrap_or_else(|err| {
            eprintln!("failed to create tempdir: {err}");
            exit(1);
          });
          let ll_path = tmpdir.path().join("out.ll");
          let _keep_tmpdir = tmpdir;

          let artifact = compile_file_checked(&cli, entry, EmitKind::LlvmIr, Some(ll_path.clone()))
            .unwrap_or_else(|()| exit(1));
          let text = fs::read_to_string(&artifact.path).unwrap_or_else(|err| {
            eprintln!("failed to read {}: {err}", artifact.path.display());
            exit(1);
          });
          print!("{text}");
        }
      }
    },
    None => {
      let Some(input) = cli.input.as_deref() else {
        let mut cmd = Cli::command();
        let _ = cmd.print_help();
        println!();
        exit(2);
      };

      match cli.pipeline {
        Pipeline::Project => {
          let ir = compile_file_to_ir(&cli, input);
          write_ir_debug(&cli, &ir);

            let mut opts = CompileOptions::default();
            opts.builtins = !cli.no_builtins;
            opts.opt_level = OptLevel::O0;
            opts.emit = EmitKind::Executable;
            opts.pie = cli.pie;

            let code = {
              let out = match compile_llvm_ir_to_artifact(&ir, opts, None) {
                Ok(out) => out,
              Err(err) => {
                eprintln!("{err}");
                exit(1);
              }
            };
            run_exe(&out.path, &[])
          };
          exit(code);
        }
        Pipeline::Checked => {
          ensure_checked_pipeline_supported(&cli);

          let tmpdir = tempfile::tempdir().unwrap_or_else(|err| {
            eprintln!("failed to create tempdir: {err}");
            exit(1);
          });
          let exe = tmpdir.path().join("out");
          let _keep_tmpdir = tmpdir;

          let _ = compile_file_checked(&cli, input, EmitKind::Executable, Some(exe.clone()))
            .map_err(|()| exit(1));

          let code = run_exe(&exe, &[]);
          exit(code);
        }
      }
    }
  }
}

fn ensure_checked_pipeline_supported(cli: &Cli) {
  if cli.entry_fn.is_some() {
    eprintln!(
      "--entry-fn is not supported with --pipeline checked (native-js uses an exported `main()` function as the entrypoint)"
    );
    exit(2);
  }
}

fn compile_file_to_ir(cli: &Cli, input: &Path) -> String {
  let mut opts = CompileOptions::default();
  opts.builtins = !cli.no_builtins;

  fn looks_like_module_source(source: &str) -> bool {
    fn starts_with_kw(line: &str, kw: &str) -> bool {
      let trimmed = line.trim_start();
      let Some(rest) = trimmed.strip_prefix(kw) else {
        return false;
      };
      // Ensure a word boundary so we don't treat `important` as `import`.
      match rest.chars().next() {
        None => true,
        Some(ch) => !ch.is_ascii_alphanumeric() && ch != '_',
      }
    }

    source
      .lines()
      .any(|line| starts_with_kw(line, "import") || starts_with_kw(line, "export"))
  }

  // Fast path: for common single-file smoke tests (no explicit `--entry-fn`), avoid constructing a
  // `typecheck-ts` program graph and instead use the pure `parse-js` emitter directly. This keeps
  // the CLI responsive and prevents the builtins integration tests from timing out when run under
  // heavy parallelism.
  if cli.entry_fn.is_none() && cli.project.is_none() {
    let source = match fs::read_to_string(input) {
      Ok(s) => s,
      Err(err) => {
        eprintln!("failed to read {}: {err}", input.display());
        exit(1);
      }
    };

    // Module syntax (`import`/`export`) requires the project compiler for module graph construction
    // and deterministic init ordering.
    if !looks_like_module_source(&source) {
      match compile_typescript_to_llvm_ir(&source, opts.clone()) {
        Ok(ir) => return ir,
        Err(NativeJsError::Codegen(native_js::codegen::CodegenError::UnsupportedStmt)) => {
          // Likely uses `import`/`export` constructs; fall back to the project compiler.
      }
      Err(NativeJsError::Codegen(native_js::codegen::CodegenError::TypeError(msg)))
        if msg.contains("`main` is reserved") =>
      {
        // The project compiler namespaces user functions and supports exporting `main()` as an
        // entrypoint; fall back to it.
      }
      Err(NativeJsError::Codegen(native_js::codegen::CodegenError::TypeError(msg)))
        if msg.contains("call to unknown function") =>
      {
        // The single-file emitter has no module graph, so imports show up as unknown functions.
        // Fall back to the project compiler which supports multi-file module linking.
      }
      Err(err) => {
          eprintln!("{err}");
          exit(1);
        }
      }
    }
  }

  let (program, entry_id) =
    project_load::load_program(cli.project.as_deref(), input, project_load::LoadMode::Project)
      .unwrap_or_else(|err| {
        eprintln!("{err}");
        exit(1);
      });

  // `Program::check()` is required to populate HIR lowerings, module resolution snapshots, and
  // export maps. The legacy `project` pipeline still tries to compile even when `typecheck-ts`
  // reports errors because the native-js backend is currently only a lightweight `parse-js` emitter
  // (not a real TS compiler).
  //
  // Use `--pipeline checked` to compile with `native_js::compile_program`, which fails on type
  // errors and enforces the strict subset validator.
  let _diagnostics = program.check();

  match compile_project_to_llvm_ir(&program, &program, entry_id, opts, cli.entry_fn.as_deref()) {
    Ok(ir) => ir,
    Err(err) => {
      eprintln!("{err}");
      exit(1);
    }
  }
}

fn compile_file_checked(
  cli: &Cli,
  input: &Path,
  emit: EmitKind,
  output: Option<PathBuf>,
) -> Result<native_js::Artifact, ()> {
  let (program, entry_id) =
    project_load::load_program(cli.project.as_deref(), input, project_load::LoadMode::Checked)
      .map_err(|err| {
        eprintln!("{err}");
      })?;

  let mut opts = CompileOptions::default();
  opts.builtins = !cli.no_builtins;
  opts.emit = emit;
  opts.output = output;
  opts.pie = cli.pie;
  // `native-js` supports emitting an extra `.ll` file regardless of the primary `EmitKind`. Use
  // that for the checked pipeline so `--emit-llvm` does not require compiling twice.
  if emit != EmitKind::LlvmIr {
    opts.emit_ir = cli.emit_llvm.clone();
  }

  match compile_program(&program, entry_id, &opts) {
    Ok(artifact) => Ok(artifact),
    Err(err) => {
      emit_compile_program_diagnostics(&program, &err);
      Err(())
    }
  }
}

fn emit_compile_program_diagnostics(program: &Program, err: &NativeJsError) {
  if let Some(diags) = err.diagnostics() {
    let render = output::render_options(false, false);
    if let Err(io_err) = output::emit_diagnostics(program, diags.to_vec(), false, render) {
      eprintln!("failed to write diagnostics: {io_err}");
    }
    return;
  }

  eprintln!("{err}");
}

fn write_ir_debug(cli: &Cli, ir: &str) {
  if let Some(path) = cli.emit_llvm.as_deref() {
    if let Err(err) = fs::write(path, ir) {
      eprintln!("failed to write {}: {err}", path.display());
      exit(1);
    }
  }
}

fn run_exe(exe_path: &Path, args: &[String]) -> i32 {
  let status = Command::new(exe_path)
    .args(args)
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .status()
    .unwrap_or_else(|err| {
      eprintln!("failed to run {}: {err}", exe_path.display());
      exit(1);
    });

  status.code().unwrap_or(1)
}
