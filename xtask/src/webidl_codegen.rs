use anyhow::{bail, Context, Result};
use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};
use xtask::webidl::load::{load_combined_webidl, WebIdlSource};

#[derive(Args, Debug)]
pub struct WebIdlCodegenArgs {
  /// Path to the WHATWG DOM Bikeshed source (`dom.bs`).
  #[arg(long, default_value = "specs/whatwg-dom/dom.bs", value_name = "FILE")]
  pub dom_source: PathBuf,

  /// Path to the WHATWG HTML spec source (`source`).
  #[arg(long, default_value = "specs/whatwg-html/source", value_name = "FILE")]
  pub html_source: PathBuf,

  /// Path to the WHATWG URL Bikeshed source (`url.bs`).
  #[arg(long, default_value = "specs/whatwg-url/url.bs", value_name = "FILE")]
  pub url_source: PathBuf,

  /// Path to the WHATWG Fetch Bikeshed source (`fetch.bs`).
  #[arg(long, default_value = "specs/whatwg-fetch/fetch.bs", value_name = "FILE")]
  pub fetch_source: PathBuf,
  /// Output Rust module path (relative to repo root unless absolute).
  #[arg(
    long,
    default_value = "src/webidl/generated/mod.rs",
    value_name = "FILE"
  )]
  pub out: PathBuf,

  /// Do not write files; instead, fail if the generated output differs.
  #[arg(long)]
  pub check: bool,
}

pub fn run_webidl_codegen(args: WebIdlCodegenArgs) -> Result<()> {
  let repo_root = crate::repo_root();
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let dom_source = to_repo_rel_source(&repo_root, &args.dom_source)
    .with_context(|| format!("resolve DOM source path {}", args.dom_source.display()))?;
  let html_source = to_repo_rel_source(&repo_root, &args.html_source)
    .with_context(|| format!("resolve HTML source path {}", args.html_source.display()))?;
  let url_source = to_repo_rel_source(&repo_root, &args.url_source)
    .with_context(|| format!("resolve URL source path {}", args.url_source.display()))?;
  let fetch_source = to_repo_rel_source(&repo_root, &args.fetch_source)
    .with_context(|| format!("resolve Fetch source path {}", args.fetch_source.display()))?;
  let out_path = absolutize(repo_root.clone(), args.out);

  let sources = [
    WebIdlSource {
      rel_path: dom_source.as_str(),
      label: "DOM",
    },
    WebIdlSource {
      rel_path: html_source.as_str(),
      label: "HTML",
    },
    WebIdlSource {
      rel_path: url_source.as_str(),
      label: "URL",
    },
    WebIdlSource {
      rel_path: fetch_source.as_str(),
      label: "Fetch",
    },
  ];

  let loaded = load_combined_webidl(&repo_root, &sources).context("load combined WebIDL sources")?;
  if !loaded.missing_sources.is_empty() {
    let mut message = String::from("missing WebIDL sources:\n");
    for (label, path) in &loaded.missing_sources {
      message.push_str(&format!("  - {label}: {}\n", path.display()));
    }
    message.push_str(
      "\nHint: ensure the spec submodules are checked out (e.g. `git submodule update --init \\\n\
       specs/whatwg-dom specs/whatwg-html specs/whatwg-url specs/whatwg-fetch`).\n",
    );
    bail!("{message}");
  }

  let parsed =
    xtask::webidl::parse_webidl(&loaded.combined_idl).context("parse extracted WebIDL")?;
  let resolved = xtask::webidl::resolve::resolve_webidl_world(&parsed);

  // Sanity-check that we actually pulled in the expected WHATWG URL + Fetch surfaces. This helps
  // catch accidental extractor regressions (e.g. missing `<pre class=idl>` blocks) that would
  // silently ship an incomplete snapshot.
  for iface in ["URL", "URLSearchParams", "Headers", "Request", "Response"] {
    if resolved.interfaces.get(iface).is_none() {
      bail!("expected WebIDL interface `{iface}` in generated world");
    }
  }
  if resolved
    .interface_mixins
    .get("WindowOrWorkerGlobalScope")
    .is_none()
  {
    bail!("expected WebIDL interface mixin `WindowOrWorkerGlobalScope` in generated world");
  }

  let generated = xtask::webidl::generate::generate_rust_module(&resolved, &rustfmt_config)
    .context("generate formatted WebIDL Rust module")?;

  if args.check {
    let existing = fs::read_to_string(&out_path)
      .with_context(|| format!("read generated file {}", out_path.display()))?;
    if existing != generated {
      bail!(
        "generated WebIDL bindings are out of date: run `cargo xtask webidl` (path={})",
        out_path.display()
      );
    }
    return Ok(());
  }

  if let Some(parent) = out_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("create output directory {}", parent.display()))?;
  }
  fs::write(&out_path, generated)
    .with_context(|| format!("write generated output {}", out_path.display()))?;

  Ok(())
}

fn absolutize(repo_root: PathBuf, path: PathBuf) -> PathBuf {
  if path.is_absolute() {
    path
  } else {
    repo_root.join(path)
  }
}

fn to_repo_rel_source(repo_root: &Path, path: &Path) -> Result<String> {
  if path.is_absolute() {
    let rel = path.strip_prefix(repo_root).with_context(|| {
      format!(
        "source path {} is absolute but not under repo root {}",
        path.display(),
        repo_root.display()
      )
    })?;
    return Ok(rel.to_string_lossy().into_owned());
  }
  Ok(path.to_string_lossy().into_owned())
}
