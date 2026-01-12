//! Offline importer for a curated subset of WPT DOM `testharness.js` tests.
//!
//! This tool copies selected WPT DOM tests (e.g. `*.window.js`, `*.any.js`, `*.html`) from a
//! *local* upstream WPT checkout into `tests/wpt_dom/`, preserving the original relative paths
//! from the WPT repo.
//!
//! It also discovers a best-effort closure of referenced support files (via:
//! - JS `// META: script=...` directives
//! - HTML `<script src>` tags
//! - HTML `<link href>` tags where `rel` implies a fetch (e.g. `stylesheet`)
//! - CSS `url(...)` / `@import` (for any copied CSS support files)
//!
//! The importer never touches the network; it only reads files from `--wpt-root`.
//!
//! URL rewriting:
//! - Fully-qualified WPT origin URLs like `https://web-platform.test/resources/foo.js` are
//!   rewritten to their origin-absolute form (`/resources/foo.js`) so the offline runner can map
//!   them via `WptFs` rules.
//! - Other URLs are preserved as-is (aside from those rewrites).
use clap::Parser;
use glob::glob;
use regex::Regex;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use thiserror::Error;
use url::Url;

type Result<T> = std::result::Result<T, ImportError>;

fn main() {
  if let Err(err) = run() {
    eprintln!("import_wpt_dom: {err}");
    std::process::exit(1);
  }
}

fn run() -> Result<()> {
  let args = Args::parse();
  let config = ImportConfig::from_args(args)?;
  let summary = run_import(config.clone())?;

  if config.dry_run {
    println!("Dry run: no files were written");
  }

  println!(
    "Imported {} file(s), skipped {}, overwritten {}",
    summary.copied.len(),
    summary.skipped.len(),
    summary.overwritten.len()
  );

  Ok(())
}

/// Import testharness DOM tests from a local WPT checkout.
#[derive(Parser, Debug)]
#[command(name = "import_wpt_dom")]
struct Args {
  /// Path to a local WPT checkout
  #[arg(long)]
  wpt_root: PathBuf,

  /// Test suite glob(s) relative to the WPT root (e.g. `dom/nodes/*.window.js`, `dom/events/**`)
  #[arg(long)]
  suite: Vec<String>,

  /// A specific test file path relative to the WPT root (repeatable)
  #[arg(long)]
  test: Vec<String>,

  /// Output directory for imported tests (defaults to `tests/wpt_dom/tests`)
  #[arg(long, default_value = "tests/wpt_dom/tests")]
  out: PathBuf,

  /// Output directory for imported `/resources/...` files (defaults to `tests/wpt_dom/resources`)
  #[arg(long, default_value = "tests/wpt_dom/resources")]
  resources_out: PathBuf,

  /// Preview actions without writing files
  #[arg(long)]
  dry_run: bool,

  /// Allow overwriting existing files
  #[arg(long)]
  overwrite: bool,

  /// Additionally fail if rewritten text files still contain `http(s)://` or protocol-relative
  /// (`//...`) URLs (excluding `data:` URLs).
  #[arg(long)]
  strict_offline: bool,

  /// Sync upstream WPT `resources/testharness.js` + `resources/testharnessreport.js` into
  /// `--resources-out` (requires `--overwrite` if they differ).
  ///
  /// By default, we treat these as local corpus fixtures (FastRender currently uses a minimal
  /// compatible subset). This flag makes it possible to switch to verbatim upstream copies later.
  #[arg(long)]
  sync_harness: bool,
}

#[derive(Clone, Debug)]
struct ImportConfig {
  wpt_root: PathBuf,
  suites: Vec<String>,
  tests: Vec<String>,
  out_dir: PathBuf,
  resources_out_dir: PathBuf,
  dry_run: bool,
  overwrite: bool,
  strict_offline: bool,
  sync_harness: bool,
}

impl ImportConfig {
  fn from_args(args: Args) -> Result<Self> {
    if args.suite.is_empty() && args.test.is_empty() {
      return Err(ImportError::Message(
        "must provide at least one of --suite or --test".to_string(),
      ));
    }

    let wpt_root = canonical_existing_dir(&args.wpt_root).map_err(|_| {
      ImportError::Message(format!("WPT root not found: {}", args.wpt_root.display()))
    })?;

    let out_dir = absolutize(&args.out)?;
    let resources_out_dir = absolutize(&args.resources_out)?;

    Ok(Self {
      wpt_root,
      suites: args.suite,
      tests: args.test,
      out_dir,
      resources_out_dir,
      dry_run: args.dry_run,
      overwrite: args.overwrite,
      strict_offline: args.strict_offline,
      sync_harness: args.sync_harness,
    })
  }
}

#[derive(Debug, Default)]
struct ImportSummary {
  copied: Vec<PathBuf>,
  skipped: Vec<PathBuf>,
  overwritten: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct Reference {
  new_value: String,
  source_rel: PathBuf,
  dest_path: PathBuf,
  should_copy: bool,
}

#[derive(Debug, Error)]
enum ImportError {
  #[error("{0}")]
  Message(String),
  #[error("no tests matched: {0}")]
  NoMatches(String),
  #[error("failed to read {0}: {1}")]
  Io(PathBuf, #[source] io::Error),
  #[error("destination exists with different contents: {0}")]
  WouldOverwrite(PathBuf),
  #[error("referenced file is missing: {0}")]
  MissingReference(PathBuf),
  #[error("path escapes configured root: {0}")]
  OutsideRoot(PathBuf),
  #[error("invalid UTF-8 while reading {0}")]
  InvalidUtf8(PathBuf),
  #[error("network URL(s) remain in rewritten {0}: {1}")]
  NetworkUrlsRemaining(PathBuf, String),
  #[error("glob error: {0}")]
  Glob(#[from] glob::PatternError),
  #[error("glob iteration error: {0}")]
  Globwalk(#[from] glob::GlobError),
}

fn run_import(config: ImportConfig) -> Result<ImportSummary> {
  let tests = discover_tests(&config)?;
  let mut summary = ImportSummary::default();

  for test in tests {
    import_entrypoint(&config, &test, &mut summary)?;
  }

  // Optionally sync harness after importing tests so the user can do a single run.
  if config.sync_harness {
    sync_harness(&config, &mut summary)?;
  }

  Ok(summary)
}

fn discover_tests(config: &ImportConfig) -> Result<Vec<PathBuf>> {
  let mut results = Vec::new();
  let mut seen = HashSet::new();

  for test in &config.tests {
    let path = config.wpt_root.join(test);
    if !path.is_file() {
      return Err(ImportError::NoMatches(test.clone()));
    }
    if is_dom_test_file(&path) && seen.insert(path.clone()) {
      results.push(path);
    }
  }

  for suite in &config.suites {
    let pattern = config.wpt_root.join(suite);
    if has_glob_pattern(suite) {
      let pattern_str = pattern.to_string_lossy().to_string();
      let mut any = false;
      for entry in glob(&pattern_str)? {
        let path = entry?;
        any = true;
        if path.is_file() && is_dom_test_file(&path) && seen.insert(path.clone()) {
          results.push(path);
        } else if path.is_dir() {
          collect_dom_test_files(&path, &mut results, &mut seen)?;
        }
      }
      if !any {
        return Err(ImportError::NoMatches(suite.clone()));
      }
    } else if pattern.is_dir() {
      collect_dom_test_files(&pattern, &mut results, &mut seen)?;
    } else if pattern.is_file() {
      if is_dom_test_file(&pattern) && seen.insert(pattern.clone()) {
        results.push(pattern);
      }
    } else {
      return Err(ImportError::NoMatches(suite.clone()));
    }
  }

  if results.is_empty() {
    return Err(ImportError::NoMatches(
      config
        .suites
        .iter()
        .chain(config.tests.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join(", "),
    ));
  }

  results.sort();
  Ok(results)
}

fn collect_dom_test_files(
  dir: &Path,
  acc: &mut Vec<PathBuf>,
  seen: &mut HashSet<PathBuf>,
) -> Result<()> {
  let entries = fs::read_dir(dir).map_err(|e| ImportError::Io(dir.to_path_buf(), e))?;
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      collect_dom_test_files(&path, acc, seen)?;
    } else if path.is_file() && is_dom_test_file(&path) && seen.insert(path.clone()) {
      acc.push(path);
    }
  }
  Ok(())
}

fn import_entrypoint(
  config: &ImportConfig,
  src_path: &Path,
  summary: &mut ImportSummary,
) -> Result<()> {
  ensure_within_root(src_path, &config.wpt_root)?;
  let relative = src_path
    .strip_prefix(&config.wpt_root)
    .map_err(|_| ImportError::OutsideRoot(src_path.to_path_buf()))?;

  let dest_path = dest_path_for_rel(config, relative);

  let mut processed: HashSet<PathBuf> = HashSet::new();
  let mut queued: HashSet<PathBuf> = HashSet::new();
  let mut queue: VecDeque<(PathBuf, PathBuf)> = VecDeque::new();

  processed.insert(dest_path.clone());
  let refs = rewrite_and_copy(config, relative, src_path, &dest_path, summary)?;
  for r in refs {
    if r.should_copy && !processed.contains(&r.dest_path) && queued.insert(r.dest_path.clone()) {
      queue.push_back((r.source_rel, r.dest_path));
    }
  }

  while let Some((rel, dest)) = queue.pop_front() {
    if processed.contains(&dest) {
      continue;
    }
    processed.insert(dest.clone());
    let src = config.wpt_root.join(&rel);
    let refs = rewrite_and_copy(config, &rel, &src, &dest, summary)?;
    for r in refs {
      if r.should_copy && !processed.contains(&r.dest_path) && queued.insert(r.dest_path.clone()) {
        queue.push_back((r.source_rel, r.dest_path));
      }
    }
  }

  Ok(())
}

fn sync_harness(config: &ImportConfig, summary: &mut ImportSummary) -> Result<()> {
  let harness_files = [
    PathBuf::from("resources/testharness.js"),
    PathBuf::from("resources/testharnessreport.js"),
  ];

  for rel in harness_files {
    let src = config.wpt_root.join(&rel);
    if !src.is_file() {
      return Err(ImportError::MissingReference(src));
    }
    let dest = dest_path_for_rel(config, &rel);
    // Treat harness like any other file copy (honoring overwrite/dry-run).
    rewrite_and_copy(config, &rel, &src, &dest, summary)?;
  }

  // Record the upstream WPT commit used for the harness snapshot. We write it adjacent to the
  // `resources/` output dir so the corpus root stays self-describing.
  let commit = read_wpt_git_commit(&config.wpt_root)?;
  let commit_file = upstream_commit_file_path(config);
  write_file(
    &commit_file,
    format!("{commit}\n").as_bytes(),
    config,
    summary,
  )?;
  Ok(())
}

fn upstream_commit_file_path(config: &ImportConfig) -> PathBuf {
  config
    .resources_out_dir
    .parent()
    .unwrap_or(&config.resources_out_dir)
    .join("UPSTREAM_COMMIT.txt")
}

fn read_wpt_git_commit(wpt_root: &Path) -> Result<String> {
  let output = Command::new("git")
    .arg("-C")
    .arg(wpt_root)
    .arg("rev-parse")
    .arg("HEAD")
    .output()
    .map_err(|e| {
      ImportError::Message(format!(
        "failed to run `git rev-parse HEAD` in {}: {e}",
        wpt_root.display()
      ))
    })?;

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    return Err(ImportError::Message(format!(
      "failed to determine WPT commit via git: {stderr}"
    )));
  }

  let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
  if sha.len() < 7 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
    return Err(ImportError::Message(format!(
      "unexpected `git rev-parse HEAD` output: {sha}"
    )));
  }
  Ok(sha)
}

fn dest_path_for_rel(config: &ImportConfig, rel: &Path) -> PathBuf {
  let rel_str = normalize_to_forward_slashes(rel);
  if let Some(rest) = rel_str.strip_prefix("resources/") {
    normalize_path(config.resources_out_dir.join(rest))
  } else {
    normalize_path(config.out_dir.join(rel))
  }
}

fn rewrite_and_copy(
  config: &ImportConfig,
  source_rel: &Path,
  src_path: &Path,
  dest_path: &Path,
  summary: &mut ImportSummary,
) -> Result<Vec<Reference>> {
  ensure_within_root(src_path, &config.wpt_root)?;

  let url_path = format!("/{}", normalize_to_forward_slashes(source_rel));

  match file_kind(src_path) {
    FileKind::Html => {
      let content = fs::read_to_string(src_path).map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
          ImportError::InvalidUtf8(src_path.to_path_buf())
        } else {
          ImportError::Io(src_path.to_path_buf(), e)
        }
      })?;
      let (rewritten, refs) = rewrite_html(config, &url_path, &content)?;
      validate_offline(dest_path, FileKind::Html, &rewritten)?;
      if config.strict_offline {
        validate_strict_offline(dest_path, &rewritten)?;
      }
      write_file(dest_path, rewritten.as_bytes(), config, summary)?;
      Ok(refs)
    }
    FileKind::Js => {
      let content = fs::read_to_string(src_path).map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
          ImportError::InvalidUtf8(src_path.to_path_buf())
        } else {
          ImportError::Io(src_path.to_path_buf(), e)
        }
      })?;
      let (rewritten, refs) = rewrite_js(config, &url_path, &content)?;
      validate_offline(dest_path, FileKind::Js, &rewritten)?;
      if config.strict_offline {
        validate_strict_offline(dest_path, &rewritten)?;
      }
      write_file(dest_path, rewritten.as_bytes(), config, summary)?;
      Ok(refs)
    }
    FileKind::Css => {
      let content = fs::read_to_string(src_path).map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
          ImportError::InvalidUtf8(src_path.to_path_buf())
        } else {
          ImportError::Io(src_path.to_path_buf(), e)
        }
      })?;
      let (rewritten, refs) = rewrite_css(config, &url_path, &content)?;
      validate_offline(dest_path, FileKind::Css, &rewritten)?;
      if config.strict_offline {
        validate_strict_offline(dest_path, &rewritten)?;
      }
      write_file(dest_path, rewritten.as_bytes(), config, summary)?;
      Ok(refs)
    }
    FileKind::Other => {
      let data = fs::read(src_path).map_err(|e| ImportError::Io(src_path.to_path_buf(), e))?;
      write_file(dest_path, &data, config, summary)?;
      Ok(Vec::new())
    }
  }
}

fn rewrite_html(
  config: &ImportConfig,
  base_url_path: &str,
  content: &str,
) -> Result<(String, Vec<Reference>)> {
  let mut references = Vec::new();
  let mut seen = HashSet::new();

  // Rewrite any `src=...` attribute (scripts, iframes, images, etc). We don't try to understand
  // HTML; this is a best-effort offline import, and we rely on validation + curation.
  let src_quoted = compile_regex(
    "html src quoted",
    r#"(?i)(?P<prefix>\ssrc\s*=\s*["'])(?P<url>[^"'>]+)(?P<suffix>["'])"#,
  )?;
  let src_unquoted = compile_regex(
    "html src unquoted",
    r#"(?i)(?P<prefix>\ssrc\s*=\s*)(?P<url>[^\s"'>]+)"#,
  )?;

  let mut rewritten = apply_rewrite(
    &src_quoted,
    content,
    config,
    base_url_path,
    &mut references,
    &mut seen,
  )?;
  rewritten = apply_rewrite_no_suffix(
    &src_unquoted,
    &rewritten,
    config,
    base_url_path,
    &mut references,
    &mut seen,
  )?;

  // Only rewrite `<link href=...>` when `rel` implies a fetch (avoid rewriting metadata like
  // `<link rel=help href=...>`).
  rewritten = apply_link_href_rewrite(
    &rewritten,
    config,
    base_url_path,
    &mut references,
    &mut seen,
  )?;

  Ok((rewritten, references))
}

fn rewrite_js(
  config: &ImportConfig,
  base_url_path: &str,
  content: &str,
) -> Result<(String, Vec<Reference>)> {
  let mut references = Vec::new();
  let mut seen = HashSet::new();

  // Rewrite `// META: script=...` directives.
  let meta_script = compile_regex(
    "meta script",
    r#"(?m)^(?P<prefix>\s*//\s*META:\s*script\s*=\s*)(?P<url>\S+)(?P<suffix>\s*)$"#,
  )?;

  let mut error: Option<ImportError> = None;
  let rewritten = meta_script
    .replace_all(content, |caps: &regex::Captures<'_>| {
      let Some(url) = caps_named(caps, "url") else {
        return caps_full(caps).to_string();
      };
      let prefix = caps_named(caps, "prefix").unwrap_or("");
      let suffix = caps_named(caps, "suffix").unwrap_or("");
      match rewrite_reference(config, base_url_path, url, &mut references, &mut seen) {
        Ok(Some(new_value)) => format!("{prefix}{new_value}{suffix}"),
        Ok(None) => caps_full(caps).to_string(),
        Err(err) => {
          error = Some(err);
          caps_full(caps).to_string()
        }
      }
    })
    .to_string();

  if let Some(err) = error {
    return Err(err);
  }

  Ok((rewritten, references))
}

fn rewrite_css(
  config: &ImportConfig,
  base_url_path: &str,
  content: &str,
) -> Result<(String, Vec<Reference>)> {
  let mut references = Vec::new();
  let mut seen = HashSet::new();

  let url_regex = compile_regex(
    "css url",
    r#"(?i)(?P<prefix>url\(\s*["']?)(?P<url>[^"')]+)(?P<suffix>["']?\s*\))"#,
  )?;
  let import_regex = compile_regex(
    "css @import",
    r#"(?i)(?P<prefix>@import\s+["'])(?P<url>[^"']+)(?P<suffix>["'])"#,
  )?;

  let mut rewritten = apply_rewrite(
    &url_regex,
    content,
    config,
    base_url_path,
    &mut references,
    &mut seen,
  )?;
  rewritten = apply_rewrite(
    &import_regex,
    &rewritten,
    config,
    base_url_path,
    &mut references,
    &mut seen,
  )?;

  Ok((rewritten, references))
}

fn compile_regex(label: &str, pattern: &str) -> Result<Regex> {
  Regex::new(pattern).map_err(|e| {
    ImportError::Message(format!(
      "internal error: failed to compile {label} regex: {e}"
    ))
  })
}

fn caps_full<'a>(caps: &'a regex::Captures<'a>) -> &'a str {
  caps.get(0).map(|m| m.as_str()).unwrap_or("")
}

fn caps_named<'a>(caps: &'a regex::Captures<'a>, name: &str) -> Option<&'a str> {
  caps.name(name).map(|m| m.as_str())
}

fn apply_rewrite(
  regex: &Regex,
  input: &str,
  config: &ImportConfig,
  base_url_path: &str,
  references: &mut Vec<Reference>,
  seen: &mut HashSet<PathBuf>,
) -> Result<String> {
  let mut error: Option<ImportError> = None;
  let rewritten = regex
    .replace_all(input, |caps: &regex::Captures<'_>| {
      let Some(url) = caps_named(caps, "url") else {
        return caps_full(caps).to_string();
      };
      let prefix = caps_named(caps, "prefix").unwrap_or("");
      let suffix = caps_named(caps, "suffix").unwrap_or("");
      match rewrite_reference(config, base_url_path, url, references, seen) {
        Ok(Some(new_value)) => format!("{prefix}{new_value}{suffix}"),
        Ok(None) => caps_full(caps).to_string(),
        Err(err) => {
          error = Some(err);
          caps_full(caps).to_string()
        }
      }
    })
    .to_string();

  if let Some(err) = error {
    return Err(err);
  }

  Ok(rewritten)
}

fn apply_rewrite_no_suffix(
  regex: &Regex,
  input: &str,
  config: &ImportConfig,
  base_url_path: &str,
  references: &mut Vec<Reference>,
  seen: &mut HashSet<PathBuf>,
) -> Result<String> {
  let mut error: Option<ImportError> = None;
  let rewritten = regex
    .replace_all(input, |caps: &regex::Captures<'_>| {
      let Some(url) = caps_named(caps, "url") else {
        return caps_full(caps).to_string();
      };
      let prefix = caps_named(caps, "prefix").unwrap_or("");
      match rewrite_reference(config, base_url_path, url, references, seen) {
        Ok(Some(new_value)) => format!("{prefix}{new_value}"),
        Ok(None) => caps_full(caps).to_string(),
        Err(err) => {
          error = Some(err);
          caps_full(caps).to_string()
        }
      }
    })
    .to_string();

  if let Some(err) = error {
    return Err(err);
  }

  Ok(rewritten)
}

fn apply_link_href_rewrite(
  input: &str,
  config: &ImportConfig,
  base_url_path: &str,
  references: &mut Vec<Reference>,
  seen: &mut HashSet<PathBuf>,
) -> Result<String> {
  let link_tag = compile_regex("link tag", r#"(?is)<link\b[^>]*>"#)?;
  let rel_attr = compile_regex(
    "link rel attribute",
    r#"(?is)(?:^|\s)rel\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
  )?;
  let href_quoted = compile_regex(
    "link href quoted",
    r#"(?is)(?P<prefix>(?:^|\s)href\s*=\s*["'])(?P<url>[^"'>]+)(?P<suffix>["'])"#,
  )?;
  let href_unquoted = compile_regex(
    "link href unquoted",
    r#"(?is)(?P<prefix>(?:^|\s)href\s*=\s*)(?P<url>[^\s"'>]+)"#,
  )?;

  let mut error: Option<ImportError> = None;
  let rewritten = link_tag
    .replace_all(input, |caps: &regex::Captures<'_>| {
      if error.is_some() {
        return caps_full(caps).to_string();
      }
      let tag = caps_full(caps);

      let Some(rel_caps) = rel_attr.captures(tag) else {
        return tag.to_string();
      };
      let rel_value = rel_caps
        .get(1)
        .or_else(|| rel_caps.get(2))
        .or_else(|| rel_caps.get(3))
        .map(|m| m.as_str())
        .unwrap_or("");
      if !link_rel_requires_fetch(rel_value) {
        return tag.to_string();
      }

      let mut out = href_quoted
        .replace_all(tag, |caps: &regex::Captures<'_>| {
          let Some(url) = caps_named(caps, "url") else {
            return caps_full(caps).to_string();
          };
          let prefix = caps_named(caps, "prefix").unwrap_or("");
          let suffix = caps_named(caps, "suffix").unwrap_or("");
          match rewrite_reference(config, base_url_path, url, references, seen) {
            Ok(Some(new_value)) => format!("{prefix}{new_value}{suffix}"),
            Ok(None) => caps_full(caps).to_string(),
            Err(err) => {
              error = Some(err);
              caps_full(caps).to_string()
            }
          }
        })
        .to_string();

      out = href_unquoted
        .replace_all(&out, |caps: &regex::Captures<'_>| {
          let Some(url) = caps_named(caps, "url") else {
            return caps_full(caps).to_string();
          };
          let prefix = caps_named(caps, "prefix").unwrap_or("");
          match rewrite_reference(config, base_url_path, url, references, seen) {
            Ok(Some(new_value)) => format!("{prefix}{new_value}"),
            Ok(None) => caps_full(caps).to_string(),
            Err(err) => {
              error = Some(err);
              caps_full(caps).to_string()
            }
          }
        })
        .to_string();

      out
    })
    .to_string();

  if let Some(err) = error {
    return Err(err);
  }

  Ok(rewritten)
}

fn rewrite_reference(
  config: &ImportConfig,
  base_url_path: &str,
  url: &str,
  references: &mut Vec<Reference>,
  seen: &mut HashSet<PathBuf>,
) -> Result<Option<String>> {
  match resolve_reference(config, base_url_path, url)? {
    Some(reference) => {
      if reference.should_copy && seen.insert(reference.dest_path.clone()) {
        references.push(reference.clone());
      }
      Ok(Some(reference.new_value))
    }
    None => Ok(None),
  }
}

fn resolve_reference(
  config: &ImportConfig,
  base_url_path: &str,
  value: &str,
) -> Result<Option<Reference>> {
  const DATA_URL_PREFIX: &str = "data:";
  let original = value.to_string();
  let trimmed = value.trim();
  if trimmed.is_empty()
    || trimmed.starts_with('#')
    || trimmed
      .get(..DATA_URL_PREFIX.len())
      .map(|prefix| prefix.eq_ignore_ascii_case(DATA_URL_PREFIX))
      .unwrap_or(false)
    || trimmed.starts_with("about:")
    || trimmed.starts_with("javascript:")
    || trimmed
      .get(..7)
      .map(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
      .unwrap_or(false)
    || trimmed
      .get(..4)
      .map(|prefix| prefix.eq_ignore_ascii_case("tel:"))
      .unwrap_or(false)
  {
    return Ok(None);
  }

  // If this is a WPT-origin URL, rewrite it to an origin-absolute path (and treat it as a local
  // reference). Otherwise, leave it alone and (depending on validation) fail later.
  let mut rewrite_to_path: Option<(String, String)> = None;
  if trimmed.starts_with("http://") || trimmed.starts_with("https://") || trimmed.starts_with("//")
  {
    if let Some((path, suffix)) = map_wpt_absolute_origin(trimmed) {
      rewrite_to_path = Some((path, suffix));
    } else {
      return Ok(None);
    }
  }

  let (path_part, _suffix) = if let Some((path, suffix)) = rewrite_to_path.clone() {
    (path, suffix)
  } else {
    split_path_and_suffix(trimmed)
  };

  if path_part.is_empty() {
    return Ok(None);
  }

  let resolved_path = if path_part.starts_with('/') {
    normalize_url_path(&path_part)
  } else {
    resolve_relative_url_path(base_url_path, &path_part)
  };

  let (source_rel, dest_path) = if let Some(rest) = resolved_path.strip_prefix("/resources/") {
    (
      PathBuf::from("resources").join(rest),
      normalize_path(config.resources_out_dir.join(rest)),
    )
  } else {
    (
      PathBuf::from(resolved_path.trim_start_matches('/')),
      normalize_path(config.out_dir.join(resolved_path.trim_start_matches('/'))),
    )
  };

  let source_path = normalize_path(config.wpt_root.join(&source_rel));
  ensure_within_root(&source_path, &config.wpt_root)?;
  if !source_path.is_file() {
    return Err(ImportError::MissingReference(source_path));
  }

  // Protect against escaping the configured output roots.
  if resolved_path.starts_with("/resources/") {
    ensure_within_root(&dest_path, &config.resources_out_dir)?;
  } else {
    ensure_within_root(&dest_path, &config.out_dir)?;
  }

  let should_copy =
    if is_harness_resource(&resolved_path) && !config.sync_harness && dest_path.exists() {
      false
    } else {
      true
    };

  let new_value = if let Some((path, suffix)) = rewrite_to_path {
    format!("{path}{suffix}")
  } else {
    original.clone()
  };

  Ok(Some(Reference {
    new_value,
    source_rel,
    dest_path,
    should_copy,
  }))
}

fn is_harness_resource(resolved_path: &str) -> bool {
  resolved_path == "/resources/testharness.js" || resolved_path == "/resources/testharnessreport.js"
}

fn map_wpt_absolute_origin(value: &str) -> Option<(String, String)> {
  let candidate = if value.starts_with("//") {
    format!("http:{value}")
  } else {
    value.to_string()
  };
  let url = Url::parse(&candidate).ok()?;
  let host = url.host_str()?;
  let host = host.trim_end_matches('.').to_ascii_lowercase();
  if host != "web-platform.test" && host != "www.web-platform.test" {
    return None;
  }

  let path = url.path().to_string();
  let mut suffix = String::new();
  if let Some(query) = url.query() {
    suffix.push('?');
    suffix.push_str(query);
  }
  if let Some(fragment) = url.fragment() {
    suffix.push('#');
    suffix.push_str(fragment);
  }

  Some((path, suffix))
}

fn split_path_and_suffix(value: &str) -> (String, String) {
  if let Some(pos) = value.find(|c| c == '?' || c == '#') {
    (value[..pos].to_string(), value[pos..].to_string())
  } else {
    (value.to_string(), String::new())
  }
}

fn normalize_url_path(path: &str) -> String {
  // Always normalize as an origin-absolute path.
  let mut stack: Vec<&str> = Vec::new();
  for comp in path.split('/') {
    if comp.is_empty() || comp == "." {
      continue;
    }
    if comp == ".." {
      stack.pop();
      continue;
    }
    stack.push(comp);
  }
  if stack.is_empty() {
    "/".to_string()
  } else {
    format!("/{}", stack.join("/"))
  }
}

fn resolve_relative_url_path(base_url_path: &str, relative: &str) -> String {
  let base_dir = base_url_dir(base_url_path);
  let joined = if base_dir == "/" {
    format!("/{relative}")
  } else {
    format!("{base_dir}{relative}")
  };
  normalize_url_path(&joined)
}

fn base_url_dir(url_path: &str) -> String {
  // url_path is expected to be an absolute path like `/dom/nodes/test.html` or `/resources/a/b.js`.
  let url_path = if url_path.starts_with('/') {
    url_path
  } else {
    "/"
  };
  match url_path.rsplit_once('/') {
    Some(("", _)) => "/".to_string(),
    Some((dir, _)) => format!("{dir}/"),
    None => "/".to_string(),
  }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum FileKind {
  Html,
  Js,
  Css,
  Other,
}

fn file_kind(path: &Path) -> FileKind {
  let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
  if file_name.ends_with(".window.js")
    || file_name.ends_with(".any.js")
    || file_name.ends_with(".js")
  {
    return FileKind::Js;
  }
  match path
    .extension()
    .and_then(|e| e.to_str())
    .map(|s| s.to_ascii_lowercase())
  {
    Some(ext) if ext == "html" || ext == "htm" => FileKind::Html,
    Some(ext) if ext == "css" => FileKind::Css,
    _ => FileKind::Other,
  }
}

fn is_dom_test_file(path: &Path) -> bool {
  if !path.is_file() {
    return false;
  }
  let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
  if file_name.ends_with(".window.js") || file_name.ends_with(".any.js") {
    return true;
  }
  match path
    .extension()
    .and_then(|e| e.to_str())
    .map(|s| s.to_ascii_lowercase())
  {
    Some(ext) if ext == "html" || ext == "htm" => true,
    _ => false,
  }
}

fn link_rel_requires_fetch(rel: &str) -> bool {
  rel.split_ascii_whitespace().any(|token| {
    token.eq_ignore_ascii_case("stylesheet")
      || token.eq_ignore_ascii_case("preload")
      || token.eq_ignore_ascii_case("modulepreload")
      || token.eq_ignore_ascii_case("icon")
      || token.eq_ignore_ascii_case("mask-icon")
      || token.eq_ignore_ascii_case("manifest")
  })
}

fn validate_offline(dest_path: &Path, kind: FileKind, content: &str) -> Result<()> {
  let found = match kind {
    FileKind::Html | FileKind::Css => find_network_urls_html_css(content)?,
    FileKind::Js => find_network_urls_js_meta(content)?,
    FileKind::Other => Vec::new(),
  };
  if found.is_empty() {
    return Ok(());
  }
  let mut found = found;
  found.truncate(5);
  Err(ImportError::NetworkUrlsRemaining(
    dest_path.to_path_buf(),
    found.join(", "),
  ))
}

fn find_network_urls_js_meta(content: &str) -> Result<Vec<String>> {
  fn is_network_url(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed
      .get(..7)
      .map(|prefix| prefix.eq_ignore_ascii_case("http://"))
      .unwrap_or(false)
      || trimmed
        .get(..8)
        .map(|prefix| prefix.eq_ignore_ascii_case("https://"))
        .unwrap_or(false)
      || trimmed.starts_with("//")
  }

  let meta_script = compile_regex(
    "offline scan meta script",
    r#"(?m)^\s*//\s*META:\s*script\s*=\s*(?P<url>\S+)\s*$"#,
  )?;
  let mut urls = Vec::new();
  for caps in meta_script.captures_iter(content) {
    let Some(url) = caps.name("url").map(|m| m.as_str()) else {
      continue;
    };
    if is_network_url(url) {
      urls.push(url.to_string());
    }
  }
  urls.sort();
  urls.dedup();
  Ok(urls)
}

fn find_network_urls_html_css(content: &str) -> Result<Vec<String>> {
  fn is_network_url(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed
      .get(..7)
      .map(|prefix| prefix.eq_ignore_ascii_case("http://"))
      .unwrap_or(false)
      || trimmed
        .get(..8)
        .map(|prefix| prefix.eq_ignore_ascii_case("https://"))
        .unwrap_or(false)
      || trimmed.starts_with("//")
  }

  // Similar to `import_wpt.rs`: only scan URL-like values in fetchable contexts (avoid false
  // positives like SVG namespace URIs or strings embedded in data URLs).
  let src_attr_quoted = compile_regex(
    "offline scan src quoted",
    r#"(?i)\ssrc\s*=\s*["'](?P<url>[^"'>]+)["']"#,
  )?;
  let src_attr_unquoted = compile_regex(
    "offline scan src unquoted",
    r#"(?i)\ssrc\s*=\s*(?P<url>[^\s"'>]+)"#,
  )?;
  let css_url = compile_regex(
    "offline scan css url",
    r#"(?i)url\(\s*["']?(?P<url>[^"')]+)["']?\s*\)"#,
  )?;
  let css_import = compile_regex(
    "offline scan css @import",
    r#"(?i)@import\s+["'](?P<url>[^"']+)["']"#,
  )?;

  let mut urls = Vec::new();
  for regex in [&src_attr_quoted, &src_attr_unquoted, &css_url, &css_import] {
    for caps in regex.captures_iter(content) {
      let Some(url) = caps.name("url").map(|m| m.as_str()) else {
        continue;
      };
      if is_network_url(url) {
        urls.push(url.to_string());
      }
    }
  }

  // `href` is only fetchable in `<link>` contexts and only for some `rel` values.
  let link_tag = compile_regex("offline scan link tag", r#"(?is)<link\b[^>]*>"#)?;
  let rel_attr = compile_regex(
    "offline scan link rel",
    r#"(?is)(?:^|\s)rel\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
  )?;
  let href_attr = compile_regex(
    "offline scan link href",
    r#"(?is)(?:^|\s)href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
  )?;

  for m in link_tag.find_iter(content) {
    let tag = m.as_str();
    let Some(rel_caps) = rel_attr.captures(tag) else {
      continue;
    };
    let rel = rel_caps
      .get(1)
      .or_else(|| rel_caps.get(2))
      .or_else(|| rel_caps.get(3))
      .map(|m| m.as_str())
      .unwrap_or("");
    if !link_rel_requires_fetch(rel) {
      continue;
    }
    let Some(href_caps) = href_attr.captures(tag) else {
      continue;
    };
    let href = href_caps
      .get(1)
      .or_else(|| href_caps.get(2))
      .or_else(|| href_caps.get(3))
      .map(|m| m.as_str())
      .unwrap_or("");
    if is_network_url(href) {
      urls.push(href.to_string());
    }
  }

  urls.sort();
  urls.dedup();
  Ok(urls)
}

fn validate_strict_offline(dest_path: &Path, content: &str) -> Result<()> {
  let mut found = find_network_urls_strict(content)?;
  if found.is_empty() {
    return Ok(());
  }
  found.truncate(5);
  Err(ImportError::NetworkUrlsRemaining(
    dest_path.to_path_buf(),
    found.join(", "),
  ))
}

fn find_network_urls_strict(content: &str) -> Result<Vec<String>> {
  fn data_url_spans(content: &str) -> Result<Vec<std::ops::Range<usize>>> {
    // Best-effort: treat any quoted string starting with `data:` as a data URL span, plus
    // common CSS `url(data:...)` forms. This avoids false positives when the *payload* of a data
    // URL contains `http://` (e.g. SVG namespaces).
    let mut spans = Vec::new();

    let double_quoted = compile_regex(
      "data url span double quoted",
      r#"(?is)"(?P<url>data:[^"]*)""#,
    )?;
    let single_quoted = compile_regex(
      "data url span single quoted",
      r#"(?is)'(?P<url>data:[^']*)'"#,
    )?;
    let css_url_unquoted = compile_regex(
      "data url span css url()",
      r#"(?is)url\(\s*(?P<url>data:[^)]*)\)"#,
    )?;

    for caps in double_quoted.captures_iter(content) {
      if let Some(m) = caps.name("url") {
        spans.push(m.start()..m.end());
      }
    }
    for caps in single_quoted.captures_iter(content) {
      if let Some(m) = caps.name("url") {
        spans.push(m.start()..m.end());
      }
    }
    for caps in css_url_unquoted.captures_iter(content) {
      if let Some(m) = caps.name("url") {
        spans.push(m.start()..m.end());
      }
    }

    spans.sort_by_key(|span| span.start);
    Ok(spans)
  }

  fn is_within_data_url(data_spans: &[std::ops::Range<usize>], idx: usize) -> bool {
    data_spans
      .iter()
      .any(|span| idx >= span.start && idx < span.end)
  }

  let mut urls = Vec::new();
  let http_re = compile_regex(
    "strict offline http url",
    r#"(?i)https?://[^\s"'<>)]{1,200}"#,
  )?;
  let scheme_re = compile_regex("strict offline scheme url", r#"(?i)//[^\s"'<>)]{1,200}"#)?;

  let data_spans = data_url_spans(content)?;
  let content_bytes = content.as_bytes();

  for m in http_re.find_iter(content) {
    let idx = m.start();
    if is_within_data_url(&data_spans, idx) {
      continue;
    }
    urls.push(m.as_str().to_string());
  }

  for m in scheme_re.find_iter(content) {
    if m.start() > 0 && content_bytes[m.start() - 1] == b':' {
      continue;
    }
    let idx = m.start();
    if is_within_data_url(&data_spans, idx) {
      continue;
    }
    urls.push(m.as_str().to_string());
  }

  urls.sort();
  urls.dedup();
  Ok(urls)
}

fn write_file(
  dest_path: &Path,
  data: &[u8],
  config: &ImportConfig,
  summary: &mut ImportSummary,
) -> Result<()> {
  let existed_before = dest_path.exists();

  if existed_before {
    let existing = fs::read(dest_path).map_err(|e| ImportError::Io(dest_path.to_path_buf(), e))?;
    if existing == data {
      summary.skipped.push(make_relative_to_repo(dest_path));
      return Ok(());
    }

    if !config.overwrite {
      return Err(ImportError::WouldOverwrite(dest_path.to_path_buf()));
    }
  }

  if !config.dry_run {
    if let Some(parent) = dest_path.parent() {
      fs::create_dir_all(parent).map_err(|e| ImportError::Io(parent.to_path_buf(), e))?;
    }
    fs::write(dest_path, data).map_err(|e| ImportError::Io(dest_path.to_path_buf(), e))?;
  }

  let rel = make_relative_to_repo(dest_path);
  if existed_before {
    summary.overwritten.push(rel);
  } else {
    summary.copied.push(rel);
  }
  Ok(())
}

fn make_relative_to_repo(path: &Path) -> PathBuf {
  // Best-effort: keep summaries readable when run from the repo root.
  if let Ok(cwd) = std::env::current_dir() {
    if let Ok(rel) = path.strip_prefix(&cwd) {
      return rel.to_path_buf();
    }
  }
  path.to_path_buf()
}

fn has_glob_pattern(value: &str) -> bool {
  value
    .chars()
    .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
}

fn ensure_within_root(path: &Path, root: &Path) -> Result<()> {
  if !path.starts_with(root) {
    return Err(ImportError::OutsideRoot(path.to_path_buf()));
  }
  Ok(())
}

fn normalize_path(path: PathBuf) -> PathBuf {
  let mut normalized = PathBuf::new();
  for component in path.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        normalized.pop();
      }
      Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
      Component::Normal(part) => normalized.push(part),
    }
  }
  normalized
}

fn normalize_to_forward_slashes(path: &Path) -> String {
  path.to_string_lossy().replace('\\', "/")
}

fn absolutize(path: &Path) -> Result<PathBuf> {
  if path.is_absolute() {
    Ok(normalize_path(path.to_path_buf()))
  } else {
    Ok(normalize_path(
      std::env::current_dir()
        .map_err(|e| ImportError::Io(path.to_path_buf(), e))?
        .join(path),
    ))
  }
}

fn canonical_existing_dir(path: &Path) -> io::Result<PathBuf> {
  let canonical = fs::canonicalize(path)?;
  if !canonical.is_dir() {
    return Err(io::Error::new(io::ErrorKind::NotFound, "not a directory"));
  }
  Ok(canonical)
}
