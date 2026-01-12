mod host;
mod output;
mod project_load;
mod type_libs;
mod diag;

use clap::{ArgAction, CommandFactory, Parser, Subcommand, ValueEnum};
use diagnostics::{host_error, Diagnostic, FileId};
use native_js::compiler::compile_llvm_ir_to_artifact;
use native_js::{
  compile_program, compile_project_to_llvm_ir, compile_typescript_to_llvm_ir, CompileOptions,
  EmitKind, NativeJsError, OptLevel,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

  /// Emit DWARF debug info and keep intermediate build artifacts.
  #[arg(long, global = true)]
  debug: bool,

  /// Which compilation pipeline to use.
  ///
  /// - `project`: legacy `parse-js` based emitter (keeps compiling even with TS type errors).
  /// - `checked`: typechecked `native_js::compile_program` pipeline (fails on type errors).
  #[arg(long, value_enum, default_value = "project", global = true)]
  pipeline: Pipeline,

  /// Emit JSON diagnostics to stdout (schema_version = 1).
  #[arg(long, global = true)]
  json: bool,

  /// Force-enable ANSI colors in diagnostics output.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  color: bool,

  /// Disable ANSI colors in diagnostics output.
  #[arg(long, global = true, action = ArgAction::SetTrue)]
  no_color: bool,
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
  std::process::exit(run(&cli));
}

fn run(cli: &Cli) -> i32 {
  let flags = diag::DiagFlags {
    json: cli.json,
    color: cli.color,
    no_color: cli.no_color,
  };

  match cli.command.as_ref() {
    Some(Commands::Check { entries }) => {
      if entries.is_empty() {
        if flags.json {
          let _ = output::emit_json_diagnostics(
            None,
            vec![host_error(
              None,
              "native-js-cli check: expected at least one entry file",
            )],
          );
        } else {
          eprintln!("native-js-cli check: expected at least one entry file");
        }
        return 2;
      }

      match cli.pipeline {
        Pipeline::Project => {
          for entry in entries {
            if let Err(err) = compile_file_to_ir(cli, entry) {
              return emit_cli_error(err, flags).into();
            }
          }

          if flags.json {
            diag::emit_success_json();
          }
          0
        }
        Pipeline::Checked => {
          if let Err(code) = ensure_checked_pipeline_supported(cli, flags) {
            return code.into();
          }

          for entry in entries {
            let tmpdir = match tempfile::tempdir() {
              Ok(tmpdir) => tmpdir,
              Err(err) => {
                let diag = host_error(None, format!("failed to create tempdir: {err}"));
                if flags.json {
                  let _ = output::emit_json_diagnostics(None, vec![diag]);
                } else {
                  eprintln!("failed to create tempdir: {err}");
                }
                return 2;
              }
            };
            let out = tmpdir.path().join("out.ll");
            let _keep_tmpdir = tmpdir;
            if let Err(err) = compile_file_checked(cli, entry, EmitKind::LlvmIr, Some(out)) {
              return emit_cli_error(err, flags).into();
            }
          }

          if flags.json {
            diag::emit_success_json();
          }
          0
        }
      }
    }
    Some(Commands::Build { entry, output }) => match cli.pipeline {
      Pipeline::Project => {
        let ir = match compile_file_to_ir(cli, entry) {
          Ok(ir) => ir,
          Err(err) => return emit_cli_error(err, flags).into(),
        };
        if let Err(err) = write_ir_debug(cli, &ir) {
          return emit_cli_error(err, flags).into();
        }

        let mut opts = CompileOptions::default();
        opts.builtins = !cli.no_builtins;
        // The `project` pipeline is intended for quick iteration and can emit LLVM IR that fails
        // strict validation (it keeps compiling even with type errors). Keep compilation fast by
        // disabling LLVM optimizations.
        opts.opt_level = OptLevel::O0;
        opts.emit = EmitKind::Executable;
        opts.pie = cli.pie;
        opts.debug = cli.debug;

        if let Err(err) = compile_llvm_ir_to_artifact(&ir, opts, Some(output.clone())) {
          let diagnostics = diag::diagnostics_from_native_js_error(&err, FileId(0));
          let err = CliError::Source {
            source: diag::SingleFileSource::default(),
            diagnostics,
          };
          return emit_cli_error(err, flags).into();
        }

        if flags.json {
          diag::emit_success_json();
        }
        0
      }
      Pipeline::Checked => {
        if let Err(code) = ensure_checked_pipeline_supported(cli, flags) {
          return code.into();
        }

        if let Err(err) =
          compile_file_checked(cli, entry, EmitKind::Executable, Some(output.to_path_buf()))
        {
          return emit_cli_error(err, flags).into();
        }

        if flags.json {
          diag::emit_success_json();
        }
        0
      }
    },
    Some(Commands::Run { .. }) if flags.json => {
      let _ = output::emit_json_diagnostics(
        None,
        vec![host_error(
          None,
          "--json is not supported with `run` (it would mix with program stdout)",
        )],
      );
      2
    }
    Some(Commands::Run { entry, args }) => match cli.pipeline {
      Pipeline::Project => {
        let ir = match compile_file_to_ir(cli, entry) {
          Ok(ir) => ir,
          Err(err) => return emit_cli_error(err, flags).into(),
        };
        if let Err(err) = write_ir_debug(cli, &ir) {
          return emit_cli_error(err, flags).into();
        }

        let mut opts = CompileOptions::default();
        opts.builtins = !cli.no_builtins;
        opts.opt_level = OptLevel::O0;
        opts.emit = EmitKind::Executable;
        opts.pie = cli.pie;
        opts.debug = cli.debug;

        let out = match compile_llvm_ir_to_artifact(&ir, opts, None) {
          Ok(out) => out,
          Err(err) => {
            let diagnostics = diag::diagnostics_from_native_js_error(&err, FileId(0));
            let err = CliError::Source {
              source: diag::SingleFileSource::default(),
              diagnostics,
            };
            return emit_cli_error(err, flags).into();
          }
        };

        run_exe(&out.path, args)
      }
      Pipeline::Checked => {
        if let Err(code) = ensure_checked_pipeline_supported(cli, flags) {
          return code.into();
        }

        let tmpdir = match tempfile::tempdir() {
          Ok(tmpdir) => tmpdir,
          Err(err) => {
            eprintln!("failed to create tempdir: {err}");
            return 2;
          }
        };
        let exe = tmpdir.path().join("out");
        let _keep_tmpdir = tmpdir;

        if let Err(err) = compile_file_checked(cli, entry, EmitKind::Executable, Some(exe.clone()))
        {
          return emit_cli_error(err, flags).into();
        }

        run_exe(&exe, args)
      }
    },
    Some(Commands::EmitIr { entry, output }) => match cli.pipeline {
      Pipeline::Project => {
        if flags.json && output.is_none() {
          let _ = output::emit_json_diagnostics(
            None,
            vec![host_error(
              None,
              "--json is not supported when emitting IR to stdout (it would mix with IR output)",
            )],
          );
          return 2;
        }

        let ir = match compile_file_to_ir(cli, entry) {
          Ok(ir) => ir,
          Err(err) => return emit_cli_error(err, flags).into(),
        };
        if let Some(path) = output.as_deref() {
          if let Err(err) = fs::write(path, &ir) {
            let diag = host_error(None, format!("failed to write {}: {err}", path.display()));
            let err = CliError::Source {
              source: diag::SingleFileSource::default(),
              diagnostics: vec![diag],
            };
            return emit_cli_error(err, flags).into();
          }
          if flags.json {
            diag::emit_success_json();
          }
        } else {
          print!("{ir}");
        }
        0
      }
      Pipeline::Checked => {
        if let Err(code) = ensure_checked_pipeline_supported(cli, flags) {
          return code.into();
        }
        if flags.json && output.is_none() {
          let _ = output::emit_json_diagnostics(
            None,
            vec![host_error(
              None,
              "--json is not supported when emitting IR to stdout (it would mix with IR output)",
            )],
          );
          return 2;
        }

        if let Some(path) = output.as_deref() {
          if let Err(err) =
            compile_file_checked(cli, entry, EmitKind::LlvmIr, Some(path.to_path_buf()))
          {
            return emit_cli_error(err, flags).into();
          }
          if flags.json {
            diag::emit_success_json();
          }
          0
        } else {
          let tmpdir = match tempfile::tempdir() {
            Ok(tmpdir) => tmpdir,
            Err(err) => {
              let diag = host_error(None, format!("failed to create tempdir: {err}"));
              let err = CliError::Source {
                source: diag::SingleFileSource::default(),
                diagnostics: vec![diag],
              };
              return emit_cli_error(err, flags).into();
            }
          };
          let ll_path = tmpdir.path().join("out.ll");
          let _keep_tmpdir = tmpdir;

          let artifact =
            match compile_file_checked(cli, entry, EmitKind::LlvmIr, Some(ll_path.clone())) {
              Ok(artifact) => artifact,
              Err(err) => return emit_cli_error(err, flags).into(),
            };
          let text = match fs::read_to_string(&artifact.path) {
            Ok(text) => text,
            Err(err) => {
              let diag =
                host_error(None, format!("failed to read {}: {err}", artifact.path.display()));
              let err = CliError::Source {
                source: diag::SingleFileSource::default(),
                diagnostics: vec![diag],
              };
              return emit_cli_error(err, flags).into();
            }
          };
          print!("{text}");
          0
        }
      }
    },
    None => {
      let Some(input) = cli.input.as_deref() else {
        if flags.json {
          let _ = output::emit_json_diagnostics(
            None,
            vec![host_error(None, "native-js-cli: expected an input file path")],
          );
          return 2;
        }

        let mut cmd = Cli::command();
        let _ = cmd.print_help();
        println!();
        return 2;
      };

      if flags.json {
        match cli.pipeline {
          Pipeline::Project => match compile_file_to_ir(cli, input) {
            Ok(_) => {
              diag::emit_success_json();
              0
            }
            Err(err) => emit_cli_error(err, flags).into(),
          },
          Pipeline::Checked => {
            if let Err(code) = ensure_checked_pipeline_supported(cli, flags) {
              return code.into();
            }

            let tmpdir = match tempfile::tempdir() {
              Ok(tmpdir) => tmpdir,
              Err(err) => {
                let diag = host_error(None, format!("failed to create tempdir: {err}"));
                let err = CliError::Source {
                  source: diag::SingleFileSource::default(),
                  diagnostics: vec![diag],
                };
                return emit_cli_error(err, flags).into();
              }
            };
            let out = tmpdir.path().join("out.ll");
            let _keep_tmpdir = tmpdir;
            if let Err(err) = compile_file_checked(cli, input, EmitKind::LlvmIr, Some(out)) {
              return emit_cli_error(err, flags).into();
            }

            diag::emit_success_json();
            0
          }
        }
      } else {
        match cli.pipeline {
          Pipeline::Project => {
            let ir = match compile_file_to_ir(cli, input) {
              Ok(ir) => ir,
              Err(err) => return emit_cli_error(err, flags).into(),
            };
            if let Err(err) = write_ir_debug(cli, &ir) {
              return emit_cli_error(err, flags).into();
            }

            let mut opts = CompileOptions::default();
            opts.builtins = !cli.no_builtins;
            opts.opt_level = OptLevel::O0;
            opts.emit = EmitKind::Executable;
            opts.pie = cli.pie;
            opts.debug = cli.debug;

            let out = match compile_llvm_ir_to_artifact(&ir, opts, None) {
              Ok(out) => out,
              Err(err) => {
                let diagnostics = diag::diagnostics_from_native_js_error(&err, FileId(0));
                let err = CliError::Source {
                  source: diag::SingleFileSource::default(),
                  diagnostics,
                };
                return emit_cli_error(err, flags).into();
              }
            };
            run_exe(&out.path, &[])
          }
          Pipeline::Checked => {
            if let Err(code) = ensure_checked_pipeline_supported(cli, flags) {
              return code.into();
            }

            let tmpdir = match tempfile::tempdir() {
              Ok(tmpdir) => tmpdir,
              Err(err) => {
                let diag = host_error(None, format!("failed to create tempdir: {err}"));
                let err = CliError::Source {
                  source: diag::SingleFileSource::default(),
                  diagnostics: vec![diag],
                };
                return emit_cli_error(err, flags).into();
              }
            };
            let exe = tmpdir.path().join("out");
            let _keep_tmpdir = tmpdir;

            if let Err(err) =
              compile_file_checked(cli, input, EmitKind::Executable, Some(exe.clone()))
            {
              return emit_cli_error(err, flags).into();
            }

            run_exe(&exe, &[])
          }
        }
      }
    }
  }
}

enum CliError {
  Program {
    program: Program,
    diagnostics: Vec<Diagnostic>,
  },
  Source {
    source: diag::SingleFileSource,
    diagnostics: Vec<Diagnostic>,
  },
}

fn emit_cli_error(err: CliError, flags: diag::DiagFlags) -> u8 {
  match err {
    CliError::Program {
      program,
      diagnostics,
    } => diag::emit_diagnostics_for_program(&program, diagnostics, flags),
    CliError::Source {
      source,
      diagnostics,
    } => diag::emit_diagnostics_for_source(&source, diagnostics, flags),
  }
}

fn ensure_checked_pipeline_supported(cli: &Cli, flags: diag::DiagFlags) -> Result<(), u8> {
  if cli.entry_fn.is_none() {
    return Ok(());
  }

  let message = "--entry-fn is not supported with --pipeline checked (native-js uses an exported `main()` function as the entrypoint)";
  if flags.json {
    let _ = output::emit_json_diagnostics(None, vec![host_error(None, message)]);
  } else {
    eprintln!("{message}");
  }
  Err(2)
}

fn compile_file_to_ir(cli: &Cli, input: &Path) -> Result<String, CliError> {
  let mut opts = CompileOptions::default();
  opts.builtins = !cli.no_builtins;
  opts.debug = cli.debug;

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

  // Fast path: for common single-file smoke tests (no explicit `--entry-fn` and no `--project`),
  // avoid constructing a `typecheck-ts` program graph and instead use the pure `parse-js` emitter
  // directly.
  if cli.entry_fn.is_none() && cli.project.is_none() {
    let source = fs::read_to_string(input).map_err(|err| CliError::Source {
      source: diag::SingleFileSource {
        name: Some(input.display().to_string()),
        text: None,
      },
      diagnostics: vec![host_error(None, format!("failed to read {}: {err}", input.display()))],
    })?;

    // Module syntax (`import`/`export`) requires the project compiler for module graph construction
    // and deterministic init ordering.
         if !looks_like_module_source(&source) {
           match compile_typescript_to_llvm_ir(&source, opts.clone()) {
             Ok(ir) => return Ok(ir),
             Err(NativeJsError::Codegen(native_js::codegen::CodegenError::UnsupportedStmt { .. })) => {
               // Likely uses `import`/`export` constructs; fall back to the project compiler.
             }
             Err(NativeJsError::Codegen(native_js::codegen::CodegenError::TypeError { message, .. }))
               if message.contains("`main` is reserved") =>
             {
               // The project compiler namespaces user functions and supports exporting `main()` as an
               // entrypoint; fall back to it.
             }
             Err(NativeJsError::Codegen(native_js::codegen::CodegenError::TypeError { message, .. }))
               if message.contains("call to unknown function") =>
             {
               // The single-file emitter has no module graph, so imports show up as unknown functions.
               // Fall back to the project compiler which supports multi-file module linking.
             }
             Err(err) => {
               let diagnostics = diag::diagnostics_from_native_js_error(&err, FileId(0));
               return Err(CliError::Source {
                 source: diag::SingleFileSource {
                   name: Some(input.display().to_string()),
                   text: Some(source),
                 },
                 diagnostics,
               });
             }
           }
         }
       }

  let (program, entry_id) = project_load::load_program(
    cli.project.as_deref(),
    input,
    project_load::LoadMode::Project,
  )
  .map_err(|message| CliError::Source {
    source: diag::SingleFileSource {
      name: Some(input.display().to_string()),
      text: None,
    },
    diagnostics: vec![host_error(None, message)],
  })?;

  // `Program::check()` is required to populate HIR lowerings, module resolution snapshots, and
  // export maps. The legacy `project` pipeline still tries to compile even when `typecheck-ts`
  // reports errors because the native-js backend is currently only a lightweight `parse-js` emitter
  // (not a real TS compiler).
  //
  // Use `--pipeline checked` to compile with `native_js::compile_program`, which fails on type
  // errors and enforces the strict subset validator.
  let _diagnostics = program.check();

  match compile_project_to_llvm_ir(&program, &program, entry_id, opts, cli.entry_fn.as_deref()) {
    Ok(ir) => Ok(ir),
    Err(err) => Err(CliError::Program {
      program,
      diagnostics: diag::diagnostics_from_native_js_error(&err, entry_id),
    }),
  }
}

fn compile_file_checked(
  cli: &Cli,
  input: &Path,
  emit: EmitKind,
  output: Option<PathBuf>,
) -> Result<native_js::Artifact, CliError> {
  let (program, entry_id) = project_load::load_program(
    cli.project.as_deref(),
    input,
    project_load::LoadMode::Checked,
  )
  .map_err(|message| CliError::Source {
    source: diag::SingleFileSource {
      name: Some(input.display().to_string()),
      text: None,
    },
    diagnostics: vec![host_error(None, message)],
  })?;

  let mut opts = CompileOptions::default();
  opts.builtins = !cli.no_builtins;
  opts.emit = emit;
  opts.output = output;
  opts.pie = cli.pie;
  opts.debug = cli.debug;
  // `native-js` supports emitting an extra `.ll` file regardless of the primary `EmitKind`. Use
  // that for the checked pipeline so `--emit-llvm` does not require compiling twice.
  if emit != EmitKind::LlvmIr {
    opts.emit_ir = cli.emit_llvm.clone();
  }

  match compile_program(&program, entry_id, &opts) {
    Ok(artifact) => Ok(artifact),
    Err(err) => Err(CliError::Program {
      program,
      diagnostics: diag::diagnostics_from_native_js_error(&err, entry_id),
    }),
  }
}

fn write_ir_debug(cli: &Cli, ir: &str) -> Result<(), CliError> {
  if let Some(path) = cli.emit_llvm.as_deref() {
    fs::write(path, ir).map_err(|err| CliError::Source {
      source: diag::SingleFileSource::default(),
      diagnostics: vec![host_error(None, format!("failed to write {}: {err}", path.display()))],
    })?;
  }
  Ok(())
}

fn run_exe(exe_path: &Path, args: &[String]) -> i32 {
  let status = match Command::new(exe_path)
    .args(args)
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .status()
  {
    Ok(status) => status,
    Err(err) => {
      eprintln!("failed to run {}: {err}", exe_path.display());
      return 2;
    }
  };

  status.code().unwrap_or(1)
}
