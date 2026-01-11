use clap::Parser;
use native_js::{compile_typescript_to_llvm_ir, CompileOptions};
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};

#[derive(Parser, Debug)]
#[command(author, version)]
struct Cli {
  /// TypeScript input file to compile and run.
  #[arg(value_name = "PATH")]
  input: PathBuf,

  /// Disable recognizing builtin calls like `console.log`, `print`, and `assert`.
  #[arg(long)]
  no_builtins: bool,

  /// Keep the generated LLVM IR for debugging.
  #[arg(long, value_name = "PATH")]
  emit_llvm: Option<PathBuf>,
}

fn main() {
  let args = Cli::parse();

  let source = match std::fs::read_to_string(&args.input) {
    Ok(s) => s,
    Err(err) => {
      eprintln!("failed to read {}: {err}", args.input.display());
      exit(1);
    }
  };

  let mut opts = CompileOptions::default();
  opts.builtins = !args.no_builtins;

  let ir = match compile_typescript_to_llvm_ir(&source, opts) {
    Ok(ir) => ir,
    Err(err) => {
      eprintln!("{err}");
      exit(1);
    }
  };

  if let Some(path) = args.emit_llvm.as_deref() {
    if let Err(err) = std::fs::write(path, &ir) {
      eprintln!("failed to write {}: {err}", path.display());
      exit(1);
    }
  }

  let tmpdir = match tempfile::tempdir() {
    Ok(d) => d,
    Err(err) => {
      eprintln!("failed to create tempdir: {err}");
      exit(1);
    }
  };

  let ll_path = tmpdir.path().join("out.ll");
  if let Err(err) = std::fs::write(&ll_path, &ir) {
    eprintln!("failed to write {}: {err}", ll_path.display());
    exit(1);
  }

  let exe_path = tmpdir.path().join("out");
  let clang = match find_clang() {
    Some(clang) => clang,
    None => {
      eprintln!("failed to find clang (tried clang-18 and clang)");
      exit(1);
    }
  };
  let status = match Command::new(clang)
    .arg("-x")
    .arg("ir")
    .arg(&ll_path)
    .arg("-o")
    .arg(&exe_path)
    .status()
  {
    Ok(s) => s,
    Err(err) => {
      eprintln!("failed to invoke clang: {err}");
      exit(1);
    }
  };

  if !status.success() {
    exit(status.code().unwrap_or(1));
  }

  let status = match Command::new(&exe_path)
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .status()
  {
    Ok(s) => s,
    Err(err) => {
      eprintln!("failed to run {}: {err}", exe_path.display());
      exit(1);
    }
  };

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

