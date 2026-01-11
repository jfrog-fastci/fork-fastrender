use clap::{CommandFactory, Parser, Subcommand};
use native_js::{compile_typescript_to_llvm_ir, CompileOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use tempfile::TempDir;

#[derive(Parser, Debug)]
#[command(author, version)]
struct Cli {
  #[command(subcommand)]
  command: Option<Commands>,

  /// TypeScript input file to compile and run (default command).
  #[arg(value_name = "PATH")]
  input: Option<PathBuf>,

  /// Disable recognizing builtin calls like `console.log`, `print`, and `assert`.
  #[arg(long, global = true)]
  no_builtins: bool,

  /// Keep the generated LLVM IR for debugging.
  #[arg(long, value_name = "PATH", global = true)]
  emit_llvm: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Type-check input sources (no code generation).
  Check {
    /// Entry files to type-check.
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
      for entry in entries {
        let _ = compile_file_to_ir(&cli, entry);
      }
    }
    Some(Commands::Build { entry, output }) => {
      let ir = compile_file_to_ir(&cli, entry);
      write_ir_debug(&cli, &ir);
      let (ll_path, tmpdir) = write_ir_to_tempfile(&ir);
      let _keep_tmpdir = tmpdir;
      let clang = find_clang().unwrap_or_else(|| {
        eprintln!("failed to find clang (tried clang-18 and clang)");
        exit(1);
      });

      let status = Command::new(clang)
        .arg("-x")
        .arg("ir")
        .arg(&ll_path)
        .arg("-o")
        .arg(output)
        .status()
        .unwrap_or_else(|err| {
          eprintln!("failed to invoke clang: {err}");
          exit(1);
        });

      if !status.success() {
        exit(status.code().unwrap_or(1));
      }
    }
    Some(Commands::Run { entry, args }) => {
      let ir = compile_file_to_ir(&cli, entry);
      write_ir_debug(&cli, &ir);
      let (exe_path, _tmpdir) = compile_ir_to_temp_exe(&ir);
      run_exe(&exe_path, args);
    }
    Some(Commands::EmitIr { entry, output }) => {
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
    None => {
      let Some(input) = cli.input.as_deref() else {
        let mut cmd = Cli::command();
        let _ = cmd.print_help();
        println!();
        exit(2);
      };

      let ir = compile_file_to_ir(&cli, input);
      write_ir_debug(&cli, &ir);
      let (exe_path, _tmpdir) = compile_ir_to_temp_exe(&ir);
      run_exe(&exe_path, &[]);
    }
  }
}

fn compile_file_to_ir(cli: &Cli, input: &Path) -> String {
  let source = match fs::read_to_string(input) {
    Ok(s) => s,
    Err(err) => {
      eprintln!("failed to read {}: {err}", input.display());
      exit(1);
    }
  };

  let mut opts = CompileOptions::default();
  opts.builtins = !cli.no_builtins;

  match compile_typescript_to_llvm_ir(&source, opts) {
    Ok(ir) => ir,
    Err(err) => {
      eprintln!("{err}");
      exit(1);
    }
  }
}

fn write_ir_debug(cli: &Cli, ir: &str) {
  if let Some(path) = cli.emit_llvm.as_deref() {
    if let Err(err) = fs::write(path, ir) {
      eprintln!("failed to write {}: {err}", path.display());
      exit(1);
    }
  }
}

fn write_ir_to_tempfile(ir: &str) -> (PathBuf, TempDir) {
  let tmpdir = tempfile::tempdir().unwrap_or_else(|err| {
    eprintln!("failed to create tempdir: {err}");
    exit(1);
  });

  let ll_path = tmpdir.path().join("out.ll");
  if let Err(err) = fs::write(&ll_path, ir) {
    eprintln!("failed to write {}: {err}", ll_path.display());
    exit(1);
  }

  (ll_path, tmpdir)
}

fn compile_ir_to_temp_exe(ir: &str) -> (PathBuf, TempDir) {
  let (ll_path, tmpdir) = write_ir_to_tempfile(ir);

  let exe_path = tmpdir.path().join("out");
  let clang = find_clang().unwrap_or_else(|| {
    eprintln!("failed to find clang (tried clang-18 and clang)");
    exit(1);
  });

  let status = Command::new(clang)
    .arg("-x")
    .arg("ir")
    .arg(&ll_path)
    .arg("-o")
    .arg(&exe_path)
    .status()
    .unwrap_or_else(|err| {
      eprintln!("failed to invoke clang: {err}");
      exit(1);
    });

  if !status.success() {
    exit(status.code().unwrap_or(1));
  }

  (exe_path, tmpdir)
}

fn run_exe(exe_path: &Path, args: &[String]) {
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

  exit(status.code().unwrap_or(1));
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
