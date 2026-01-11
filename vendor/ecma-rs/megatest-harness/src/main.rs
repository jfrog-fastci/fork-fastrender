use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use megatest_harness::{
  baseline_path, compute_baseline_entry, discover_fixtures, filter_fixtures, load_baseline,
  megatest_filter, parse_and_lower, read_source, source_sha256, write_baseline, Baseline,
  BASELINE_VERSION,
};
use std::collections::BTreeMap;

#[derive(Parser, Debug)]
#[command(name = "megatest-harness")]
struct Args {
  /// Regenerate `baselines/baseline.json` from the current compiler pipeline.
  #[arg(long)]
  update_baselines: bool,

  /// When checking baselines, also run `optimize-js` (slow; mirrors the ignored test).
  #[arg(long)]
  optimize: bool,

  /// Only run fixtures whose relative path contains this substring.
  ///
  /// Defaults to `MEGATEST_FILTER` when unset.
  #[arg(long)]
  filter: Option<String>,
}

fn main() -> Result<()> {
  let args = Args::parse();
  let filter = args.filter.clone().or_else(megatest_filter);
  let fixtures = filter_fixtures(discover_fixtures()?, filter.as_deref());

  if args.update_baselines {
    let mut files = BTreeMap::new();
    for fixture in fixtures {
      let source = read_source(&fixture.path)?;
      let entry = compute_baseline_entry(&source)
        .with_context(|| format!("compute baseline entry for {fixture}"))?;
      files.insert(fixture.name, entry);
    }
    let baseline = Baseline {
      version: BASELINE_VERSION,
      files,
    };
    write_baseline(&baseline)?;
    println!("wrote {}", baseline_path().display());
    return Ok(());
  }

  let baseline = load_baseline()?;
  for fixture in fixtures {
    let expected = baseline
      .files
      .get(&fixture.name)
      .ok_or_else(|| anyhow!("missing baseline entry for {fixture}"))?;

    let source = read_source(&fixture.path)?;
    let actual_source_sha256 = source_sha256(&source);
    if actual_source_sha256 != expected.source_sha256 {
      bail!(
        "source hash mismatch for {fixture}: expected {}, got {}",
        expected.source_sha256,
        actual_source_sha256
      );
    }

    let (parse, hir) = parse_and_lower(&source)?;
    if parse != expected.parse {
      bail!("parse mismatch for {fixture}: expected {:#?}, got {:#?}", expected.parse, parse);
    }
    if hir != expected.hir {
      bail!("hir mismatch for {fixture}: expected {:#?}, got {:#?}", expected.hir, hir);
    }

    if args.optimize {
      let optimize = megatest_harness::optimize(&source)?;
      if optimize != expected.optimize {
        bail!(
          "optimize mismatch for {fixture}: expected {:#?}, got {:#?}",
          expected.optimize,
          optimize
        );
      }
    }
  }

  println!(
    "OK ({} file(s)){}",
    baseline.files.len(),
    if args.optimize { " including optimize-js" } else { "" }
  );
  Ok(())
}

