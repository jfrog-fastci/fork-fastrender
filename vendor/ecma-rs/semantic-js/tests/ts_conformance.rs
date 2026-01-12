use diagnostics::sort_diagnostics;
use hir_js::{lower_file, FileKind as HirFileKind};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use rand::{rngs::StdRng, seq::SliceRandom, SeedableRng};
use semantic_js::ts::from_hir_js::lower_to_ts_hir;
use semantic_js::ts::{
  bind_ts_program, DeclData, Diagnostic, FileId, ModuleRef, Namespace, Resolver, SymbolData,
  SymbolOrigin, SymbolTable, TsProgramSemantics,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
struct FixtureResolver {
  files: BTreeMap<String, FileId>,
}

impl FixtureResolver {
  fn new(files: BTreeMap<String, FileId>) -> Self {
    Self { files }
  }

  fn resolve_specifier(&self, specifier: &str) -> Option<FileId> {
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |candidate: String| {
      if !candidates.iter().any(|c| c == &candidate) {
        candidates.push(candidate);
      }
    };

    push(specifier.to_string());

    if let Some(without_dot) = specifier.strip_prefix("./") {
      push(without_dot.to_string());
    }

    if let Some(without_slash) = specifier.strip_prefix('/') {
      push(without_slash.to_string());
    }

    let mut base = specifier;
    if let Some(stripped) = base.strip_prefix("./") {
      base = stripped;
    }
    if let Some(stripped) = base.strip_prefix('/') {
      base = stripped;
    }

    if base != specifier {
      push(base.to_string());
    }

    let has_extension = base.ends_with(".d.ts")
      || base
        .rsplit_once('/')
        .map_or(base.contains('.'), |(_, tail)| tail.contains('.'));

    if !has_extension {
      push(format!("{base}.ts"));
      push(format!("{base}.d.ts"));
    }

    for candidate in candidates {
      if let Some(file) = self.files.get(&candidate) {
        return Some(*file);
      }
    }
    None
  }
}

impl Resolver for FixtureResolver {
  fn resolve(&self, _from: FileId, specifier: &str) -> Option<FileId> {
    self.resolve_specifier(specifier)
  }
}

fn export_snapshot(
  sem: &TsProgramSemantics,
  files: &[FileId],
) -> BTreeMap<FileId, Vec<(String, Namespace, semantic_js::ts::SymbolId)>> {
  let mut out = BTreeMap::new();
  for file in files {
    let mut entries = Vec::new();
    for (name, group) in sem.exports_of(*file).iter() {
      for ns in [Namespace::VALUE, Namespace::TYPE, Namespace::NAMESPACE] {
        if let Some(sym) = group.symbol_for(ns, sem.symbols()) {
          entries.push((name.clone(), ns, sym));
        }
      }
    }
    entries.sort_by(|a, b| {
      a.0
        .cmp(&b.0)
        .then_with(|| a.1.bits().cmp(&b.1.bits()))
        .then_with(|| a.2.cmp(&b.2))
    });
    out.insert(*file, entries);
  }
  out
}

fn symbol_table_snapshot(table: &SymbolTable) -> (Vec<SymbolData>, Vec<DeclData>) {
  let mut symbols: Vec<_> = table.symbols_iter().cloned().collect();
  symbols.sort_by_key(|s| s.id);

  let mut decls: Vec<_> = table.decls_iter().cloned().collect();
  decls.sort_by_key(|d| d.id);

  (symbols, decls)
}

fn split_multifile_fixture(source: &str) -> Vec<(String, String)> {
  let mut out = Vec::<(String, String)>::new();
  let mut current_name: Option<String> = None;
  let mut current_src = String::new();

  for line in source.split_inclusive('\n') {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("// @filename:") {
      if let Some(name) = current_name.take() {
        out.push((name, std::mem::take(&mut current_src)));
      }
      current_name = Some(rest.trim().to_string());
      continue;
    }

    if current_name.is_some() {
      current_src.push_str(line);
    }
  }

  if let Some(name) = current_name.take() {
    out.push((name, current_src));
  }

  out
}

fn fixture_case_path(rel: &str) -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../parse-js/tests/TypeScript/tests/cases")
    .join(rel)
}

fn hir_kind_for_filename(name: &str) -> HirFileKind {
  if name.ends_with(".d.ts") {
    HirFileKind::Dts
  } else {
    HirFileKind::Ts
  }
}

struct TsConformanceCase {
  files_by_name: BTreeMap<String, FileId>,
  hir_by_id: Arc<HashMap<FileId, Arc<semantic_js::ts::HirFile>>>,
  resolver: FixtureResolver,
  all_files: Vec<FileId>,
}

impl TsConformanceCase {
  fn load(rel: &str) -> Self {
    let path = fixture_case_path(rel);
    let source = std::fs::read_to_string(&path)
      .unwrap_or_else(|e| panic!("failed to read fixture {path:?}: {e}"));
    let files = split_multifile_fixture(&source);
    assert!(
      !files.is_empty(),
      "fixture {path:?} did not contain any // @filename: sections"
    );

    let mut names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
    names.sort();
    names.dedup();

    let mut files_by_name = BTreeMap::<String, FileId>::new();
    for (idx, name) in names.into_iter().enumerate() {
      files_by_name.insert(name, FileId(idx as u32));
    }

    let mut hir_by_id = HashMap::<FileId, Arc<semantic_js::ts::HirFile>>::new();
    for (name, file_source) in files {
      let file_id = *files_by_name
        .get(&name)
        .unwrap_or_else(|| panic!("missing FileId assignment for {name:?}"));
      let ast = parse_with_options(
        &file_source,
        ParseOptions {
          dialect: Dialect::Ts,
          source_type: SourceType::Module,
        },
      )
      .unwrap_or_else(|e| panic!("failed to parse {name:?}: {e:?}"));
      let lower = lower_file(file_id, hir_kind_for_filename(&name), &ast);
      let ts_hir = lower_to_ts_hir(&ast, &lower);
      hir_by_id.insert(file_id, Arc::new(ts_hir));
    }

    let mut all_files: Vec<FileId> = files_by_name.values().copied().collect();
    all_files.sort();

    let resolver = FixtureResolver::new(files_by_name.clone());
    Self {
      files_by_name,
      hir_by_id: Arc::new(hir_by_id),
      resolver,
      all_files,
    }
  }

  fn file_id(&self, name: &str) -> FileId {
    *self
      .files_by_name
      .get(name)
      .unwrap_or_else(|| panic!("fixture missing file {name:?}"))
  }

  fn bind_with_roots(&self, roots: Vec<FileId>) -> (TsProgramSemantics, Vec<Diagnostic>) {
    let hir_by_id = Arc::clone(&self.hir_by_id);
    let resolver = self.resolver.clone();
    bind_ts_program(&roots, &resolver, |file| {
      hir_by_id
        .get(&file)
        .unwrap_or_else(|| panic!("missing lowered file for {file:?}"))
        .clone()
    })
  }

  fn bind_and_assert_deterministic(&self) -> (TsProgramSemantics, Vec<Diagnostic>) {
    let roots = self.all_files.clone();
    let (baseline_sem, mut baseline_diags) = self.bind_with_roots(roots.clone());
    sort_diagnostics(&mut baseline_diags);
    let baseline_exports = export_snapshot(&baseline_sem, &self.all_files);
    let baseline_symbols = symbol_table_snapshot(baseline_sem.symbols());

    let mut shuffled = roots.clone();
    shuffled.shuffle(&mut StdRng::seed_from_u64(0xfeed_beef));
    if shuffled == roots {
      shuffled.reverse();
    }

    let hir_by_id = Arc::clone(&self.hir_by_id);
    let resolver = self.resolver.clone();
    let all_files = self.all_files.clone();
    let handle = std::thread::spawn(move || {
      let (sem, mut diags) = bind_ts_program(&shuffled, &resolver, |file| {
        hir_by_id
          .get(&file)
          .unwrap_or_else(|| panic!("missing lowered file for {file:?}"))
          .clone()
      });
      sort_diagnostics(&mut diags);
      (export_snapshot(&sem, &all_files), symbol_table_snapshot(sem.symbols()), diags)
    });

    let (exports, symbols, diags) = handle.join().expect("determinism thread panicked");
    assert_eq!(
      exports, baseline_exports,
      "exports differ between root orders"
    );
    assert_eq!(
      symbols, baseline_symbols,
      "symbol table differs between root orders"
    );
    assert_eq!(
      diags, baseline_diags,
      "diagnostics differ between root orders"
    );

    (baseline_sem, baseline_diags)
  }
}

#[test]
fn ts_conformance_module_augmentation_imports_and_exports_1() {
  let case = TsConformanceCase::load("compiler/moduleAugmentationImportsAndExports1.ts");
  let (sem, diags) = case.bind_and_assert_deterministic();
  assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");

  let f1 = case.file_id("f1.ts");
  let f3 = case.file_id("f3.ts");

  let exports_f1 = sem.exports_of(f1);
  assert!(exports_f1.contains_key("A"), "f1.ts should export A");

  let symbols = sem.symbols();
  let a_group = exports_f1.get("A").expect("A export group exists");
  let a_symbol = a_group
    .symbol_for(Namespace::TYPE, symbols)
    .expect("A type symbol");

  let decl_files: BTreeSet<FileId> = sem
    .symbol_decls(a_symbol, Namespace::TYPE)
    .iter()
    .map(|decl| symbols.decl(*decl).file)
    .collect();
  assert!(
    decl_files.contains(&f1),
    "expected A type declarations to include f1.ts; got {decl_files:?}"
  );
  assert!(
    decl_files.contains(&f3),
    "expected A type declarations to include f3.ts module augmentation; got {decl_files:?}"
  );
}

#[test]
fn ts_conformance_module_augmentation_imports_and_exports_2() {
  let case = TsConformanceCase::load("compiler/moduleAugmentationImportsAndExports2.ts");
  let (_sem, diags) = case.bind_and_assert_deterministic();

  let ts2666 = diags.iter().filter(|d| d.code == "TS2666").count();
  let ts2667 = diags.iter().filter(|d| d.code == "TS2667").count();

  if ts2666 > 0 || ts2667 > 0 {
    assert_eq!(ts2666, 1, "expected TS2666 exactly once; got {diags:?}");
    assert_eq!(ts2667, 1, "expected TS2667 exactly once; got {diags:?}");
  }
}

#[test]
fn ts_conformance_module_augmentation_no_new_names() {
  let case = TsConformanceCase::load("compiler/moduleAugmentationNoNewNames.ts");
  let (sem, diags) = case.bind_and_assert_deterministic();
  assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");

  let observable = case.file_id("observable.ts");
  let map = case.file_id("map.ts");
  let main = case.file_id("main.ts");

  let exports_observable = sem.exports_of(observable);
  assert!(
    exports_observable.contains_key("Observable"),
    "observable.ts should export Observable"
  );

  let symbols = sem.symbols();
  let observable_sym = sem
    .resolve_export(observable, "Observable", Namespace::TYPE)
    .expect("Observable exported in type namespace");

  let decl_files: BTreeSet<FileId> = sem
    .symbol_decls(observable_sym, Namespace::TYPE)
    .iter()
    .map(|decl| symbols.decl(*decl).file)
    .collect();
  assert!(
    decl_files.contains(&observable),
    "expected Observable type declarations to include observable.ts; got {decl_files:?}"
  );
  assert!(
    decl_files.contains(&map),
    "expected Observable type declarations to include map.ts module augmentation; got {decl_files:?}"
  );

  let imported = sem
    .resolve_in_module(main, "Observable", Namespace::TYPE)
    .expect("main.ts should import Observable");
  match &symbols.symbol(imported).origin {
    SymbolOrigin::Import {
      from: ModuleRef::File(from),
      imported,
    } => {
      assert_eq!(*from, observable);
      assert_eq!(imported, "Observable");
    }
    other => panic!("expected imported Observable symbol origin, got {other:?}"),
  }
}
