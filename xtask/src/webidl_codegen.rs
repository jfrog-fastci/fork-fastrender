use anyhow::{bail, Context, Result};
use clap::Args;
use std::fs;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct WebIdlCodegenArgs {
  /// Path to the WHATWG DOM Bikeshed source (`dom.bs`).
  #[arg(long, default_value = "specs/whatwg-dom/dom.bs", value_name = "FILE")]
  pub dom_source: PathBuf,

  /// Path to the WHATWG HTML spec source (`source`).
  #[arg(long, default_value = "specs/whatwg-html/source", value_name = "FILE")]
  pub html_source: PathBuf,

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

  let dom_source = absolutize(repo_root.clone(), args.dom_source);
  let html_source = absolutize(repo_root.clone(), args.html_source);
  let out_path = absolutize(repo_root, args.out);

  let dom_text = fs::read_to_string(&dom_source)
    .with_context(|| format!("read DOM spec source {}", dom_source.display()))?;
  let html_text = fs::read_to_string(&html_source)
    .with_context(|| format!("read HTML spec source {}", html_source.display()))?;

  let mut idl = String::new();
  for block in xtask::webidl::extract_webidl_blocks(&dom_text) {
    idl.push_str(&block);
    idl.push_str("\n\n");
  }
  for block in xtask::webidl::extract_webidl_blocks(&html_text) {
    idl.push_str(&block);
    idl.push_str("\n\n");
  }

  let parsed = xtask::webidl::parse_webidl(&idl).context("parse extracted WebIDL")?;
  let resolved = xtask::webidl::resolve::resolve_webidl_world(&parsed);

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
