use clap::{CommandFactory, Parser, Subcommand};
use diagnostics::paths::normalize_fs_path;
use native_js::compiler::compile_llvm_ir_to_artifact;
use native_js::{compile_project_to_llvm_ir, CompileOptions, EmitKind};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use std::sync::{Arc, Mutex};
use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::resolve::{canonicalize_path, NodeResolver, ResolveOptions};
use typecheck_ts::{FileId, FileKey, Host, HostError, Program};

const BUILTINS_D_TS: &str = r#"
declare function print(value: string | number | boolean): void;
declare function assert(cond: boolean, msg?: string): void;
declare function panic(msg?: string): void;
declare function trap(): void;
"#;

#[derive(Parser, Debug)]
#[command(author, version)]
struct Cli {
  #[command(subcommand)]
  command: Option<Commands>,

  /// TypeScript input file to compile and run (default command).
  #[arg(value_name = "PATH")]
  input: Option<PathBuf>,

  /// Exported function in the entry module to call after module initialization.
  ///
  /// If omitted and the entry module exports `main()`, it is invoked automatically. Otherwise,
  /// only top-level module initializers are executed.
  #[arg(long, value_name = "NAME", global = true)]
  entry_fn: Option<String>,

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

      let mut opts = CompileOptions::default();
      opts.builtins = !cli.no_builtins;
      opts.emit = EmitKind::Executable;

      if let Err(err) = compile_llvm_ir_to_artifact(&ir, opts, Some(output.clone())) {
        eprintln!("{err}");
        exit(1);
      }
    }
    Some(Commands::Run { entry, args }) => {
      let ir = compile_file_to_ir(&cli, entry);
      write_ir_debug(&cli, &ir);

      let mut opts = CompileOptions::default();
      opts.builtins = !cli.no_builtins;
      opts.emit = EmitKind::Executable;

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

      let mut opts = CompileOptions::default();
      opts.builtins = !cli.no_builtins;
      opts.emit = EmitKind::Executable;

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
  }
}

#[derive(Clone)]
struct DiskHost {
  state: Arc<Mutex<DiskState>>,
  resolver: NodeResolver,
  compiler_options: CompilerOptions,
  libs: Vec<LibFile>,
}

#[derive(Default)]
struct DiskState {
  path_to_key: BTreeMap<PathBuf, FileKey>,
  key_to_path: HashMap<FileKey, PathBuf>,
  key_to_kind: HashMap<FileKey, FileKind>,
  texts: HashMap<FileKey, Arc<str>>,
}

impl DiskHost {
  fn new(entry: &Path) -> Result<(Self, FileKey), String> {
    let canonical =
      canonicalize_path(entry).map_err(|err| format!("failed to read {}: {err}", entry.display()))?;

    let resolver = NodeResolver::new(ResolveOptions {
      node_modules: true,
      package_imports: true,
    });

    let compiler_options = CompilerOptions::default();

    let libs = vec![LibFile {
      key: FileKey::new("native-js://builtins.d.ts"),
      name: Arc::from("native-js builtins"),
      kind: FileKind::Dts,
      text: Arc::from(BUILTINS_D_TS),
    }];

    let state = Arc::new(Mutex::new(DiskState::default()));
    let host = DiskHost {
      state,
      resolver,
      compiler_options,
      libs,
    };

    let entry_key = {
      let mut guard = host.state.lock().unwrap();
      guard.intern_path(canonical)
    };

    Ok((host, entry_key))
  }

  fn path_for_key(&self, key: &FileKey) -> Option<PathBuf> {
    let state = self.state.lock().unwrap();
    state.key_to_path.get(key).cloned()
  }
}

impl DiskState {
  fn intern_path(&mut self, path: PathBuf) -> FileKey {
    if let Some(existing) = self.path_to_key.get(&path) {
      return existing.clone();
    }
    let key = FileKey::new(normalize_fs_path(&path));
    let kind = file_kind_for(&path);
    self.path_to_key.insert(path.clone(), key.clone());
    self.key_to_path.insert(key.clone(), path);
    self.key_to_kind.insert(key.clone(), kind);
    key
  }
}

impl Host for DiskHost {
  fn file_text(&self, file: &FileKey) -> Result<Arc<str>, HostError> {
    let mut state = self.state.lock().unwrap();
    if let Some(text) = state.texts.get(file) {
      return Ok(text.clone());
    }
    let path = state
      .key_to_path
      .get(file)
      .cloned()
      .ok_or_else(|| HostError::new(format!("unknown file {file}")))?;
    let text = fs::read_to_string(&path)
      .map_err(|err| HostError::new(format!("failed to read {}: {err}", path.display())))?;
    let arc: Arc<str> = Arc::from(text);
    state.texts.insert(file.clone(), arc.clone());
    Ok(arc)
  }

  fn resolve(&self, from: &FileKey, specifier: &str) -> Option<FileKey> {
    let base = self.path_for_key(from).or_else(|| {
      let candidate = PathBuf::from(from.as_str());
      candidate.is_file().then_some(candidate)
    })?;
    let resolved = self.resolver.resolve(&base, specifier)?;
    let resolved = canonicalize_path(&resolved).unwrap_or(resolved);
    let mut state = self.state.lock().unwrap();
    Some(state.intern_path(resolved))
  }

  fn compiler_options(&self) -> CompilerOptions {
    self.compiler_options.clone()
  }

  fn lib_files(&self) -> Vec<LibFile> {
    self.libs.clone()
  }

  fn file_kind(&self, file: &FileKey) -> FileKind {
    let state = self.state.lock().unwrap();
    state.key_to_kind.get(file).copied().unwrap_or(FileKind::Ts)
  }
}

fn file_kind_for(path: &Path) -> FileKind {
  let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
  let name = name.to_ascii_lowercase();
  if name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts") {
    return FileKind::Dts;
  }
  if name.ends_with(".tsx") {
    return FileKind::Tsx;
  }
  if name.ends_with(".ts") || name.ends_with(".mts") || name.ends_with(".cts") {
    return FileKind::Ts;
  }
  if name.ends_with(".jsx") {
    return FileKind::Jsx;
  }
  if name.ends_with(".js") || name.ends_with(".mjs") || name.ends_with(".cjs") {
    return FileKind::Js;
  }
  FileKind::Ts
}

fn compile_file_to_ir(cli: &Cli, input: &Path) -> String {
  let (host, entry_key) = DiskHost::new(input).unwrap_or_else(|err| {
    eprintln!("{err}");
    exit(1);
  });

  let program = Program::new(host.clone(), vec![entry_key.clone()]);

  // `Program::check()` is required to populate HIR lowerings, module resolution snapshots, and
  // export maps. The CLI still tries to compile even when typecheck-ts reports errors because the
  // native-js backend is currently only a lightweight `parse-js` emitter (not a real TS compiler).
  //
  // This keeps the CLI useful as a codegen smoke test while allowing `typecheck-ts` to be used for
  // module graph discovery.
  let _diagnostics = program.check();

  let entry_id: FileId = program
    .file_id(&entry_key)
    .expect("entry file should be loaded");

  let mut opts = CompileOptions::default();
  opts.builtins = !cli.no_builtins;

  match compile_project_to_llvm_ir(&program, &host, entry_id, opts, cli.entry_fn.as_deref()) {
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
