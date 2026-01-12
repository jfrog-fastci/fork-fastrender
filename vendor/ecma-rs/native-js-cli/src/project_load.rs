use crate::host;
use crate::type_libs;
use std::path::{Path, PathBuf};
use typecheck_ts::lib_support::{CompilerOptions, LibName, ScriptTarget};
use typecheck_ts::resolve::{canonicalize_path, NodeResolver, ResolveOptions};
use typecheck_ts::tsconfig;
use typecheck_ts::{FileId, Program};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadMode {
  Project,
  Checked,
}

pub fn load_program(
  project: Option<&Path>,
  entry: &Path,
  mode: LoadMode,
) -> Result<(Program, FileId), String> {
  let project = match project {
    Some(path) => Some(tsconfig::load_project_config(path)?),
    None => None,
  };

  let mut compiler_options = match (project.as_ref(), mode) {
    (Some(cfg), _) => cfg.compiler_options.clone(),
    // For the legacy `project` pipeline, `typecheck-ts` is only used for module graph discovery and
    // export maps. Avoid loading TypeScript's bundled standard library (`lib.dom.d.ts`, etc), which
    // is large and makes the CLI (and its integration tests) extremely slow.
    //
    // When compiling with a `tsconfig.json` we respect user-specified `lib` / `noLib` settings.
    (None, LoadMode::Project) => CompilerOptions {
      no_default_lib: true,
      ..Default::default()
    },
    // The checked pipeline runs real typechecking and strict-subset validation. The native-js
    // backend targets Node-like native executables, so the DOM lib is unnecessary and slow to load.
    // Match the `native-js` binary defaults: load only the target ES lib unless the user explicitly
    // configured libs.
    (None, LoadMode::Checked) => CompilerOptions::default(),
  };

  ensure_default_es_lib(&mut compiler_options);

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
  extra_libs.push(match mode {
    LoadMode::Project => native_js::builtins::project_builtins_lib(),
    LoadMode::Checked => native_js::builtins::checked_builtins_lib(),
  });

  let resolve_options = ResolveOptions {
    node_modules: true,
    package_imports: true,
  };
  let resolver = host::ModuleResolver {
    resolver: NodeResolver::new(resolve_options),
    tsconfig: project.as_ref().and_then(host::TsconfigResolver::from_project),
  };

  let entry_canonical = canonicalize_path(entry)
    .map_err(|err| format!("failed to read entry {}: {err}", entry.display()))?;

  let mut root_paths: Vec<PathBuf> = Vec::new();
  if let Some(cfg) = project.as_ref() {
    root_paths.extend(cfg.root_files.iter().cloned());
  }
  root_paths.push(entry_canonical.clone());
  root_paths.sort_by(|a, b| a.display().to_string().cmp(&b.display().to_string()));
  root_paths.dedup();

  let (host, roots) =
    host::DiskHost::new(&root_paths, resolver, compiler_options, extra_libs, type_roots)?;
  let entry_key = host
    .key_for_path(&entry_canonical)
    .ok_or_else(|| format!("entry file not loaded: {}", entry.display()))?;
  let program = Program::new(host, roots);
  let entry_file = program
    .file_id(&entry_key)
    .ok_or_else(|| format!("entry file not loaded: {}", entry.display()))?;

  Ok((program, entry_file))
}

fn ensure_default_es_lib(options: &mut CompilerOptions) {
  // TypeScript defaults to loading `dom` + an ES lib when `compilerOptions.lib` is not provided.
  // For native-js, the DOM lib is unnecessary (we're targeting native executables / Node-like
  // environments) and adds significant startup cost during typechecking.
  //
  // When the user did not specify `lib` and did not opt out via `no_default_lib`, default to the
  // target ES lib only.
  if options.libs.is_empty() && !options.no_default_lib {
    let es_lib = match options.target {
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
    options.libs.push(
      LibName::parse(es_lib).expect("built-in ES lib name should parse as a LibName"),
    );
    if matches!(options.target, ScriptTarget::EsNext) {
      options.libs.push(
        LibName::parse("esnext.disposable").expect("built-in ES lib name should parse as a LibName"),
      );
    }
  }
}
