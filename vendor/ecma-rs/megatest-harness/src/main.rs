use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use megatest_harness::{
  baseline_path, compute_baseline_entry, discover_fixtures, filter_fixtures, load_baseline,
  megatest_filter, parse_and_lower, read_source, source_sha256, write_baseline, BaselineEntry,
  Baseline, BASELINE_VERSION,
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

  if args.update_baselines {
    // For baseline updates, we always write a baseline that covers *all* fixtures.
    //
    // When a filter is set, only recompute entries matching the filter (plus any entries whose
    // source changed or were missing), and keep the rest from the existing baseline. This avoids
    // accidentally committing a partial `baseline.json` while keeping update iterations fast.
    let update_filter = args.filter.clone().or_else(megatest_filter);
    let fixtures = discover_fixtures()?;

    let existing: Option<Baseline> = load_baseline().ok();
    let mut files = BTreeMap::new();
    let mut updated = 0usize;

    for fixture in fixtures {
      let source = read_source(&fixture.path)?;
      let cur_source_sha256 = source_sha256(&source);

      let should_recompute_for_filter = update_filter
        .as_deref()
        .is_some_and(|filter| fixture.name.contains(filter));
      let existing_entry = existing
        .as_ref()
        .and_then(|baseline| baseline.files.get(&fixture.name))
        .cloned();
      let source_changed = existing_entry
        .as_ref()
        .is_some_and(|entry| entry.source_sha256 != cur_source_sha256);
      let missing_entry = existing_entry.is_none();

      let (entry, recomputed): (BaselineEntry, bool) =
        if should_recompute_for_filter || source_changed || missing_entry || existing.is_none() {
          let entry = compute_baseline_entry(&source)
            .with_context(|| format!("compute baseline entry for {fixture}"))?;
          (entry, true)
        } else {
          (existing_entry.expect("entry exists"), false)
        };
      if recomputed {
        updated += 1;
      }
      files.insert(fixture.name, entry);
    }
    let baseline = Baseline {
      version: BASELINE_VERSION,
      files,
    };
    write_baseline(&baseline)?;
    if let Some(filter) = update_filter {
      println!(
        "wrote {} (updated {updated} file(s), filter={filter})",
        baseline_path().display()
      );
    } else {
      println!(
        "wrote {} (updated {updated} file(s))",
        baseline_path().display()
      );
    }
    return Ok(());
  }

  let filter = args.filter.clone().or_else(megatest_filter);
  let fixtures = filter_fixtures(discover_fixtures()?, filter.as_deref());

  let baseline = load_baseline()?;
  let total = fixtures.len();
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
    total,
    if args.optimize { " including optimize-js" } else { "" }
  );
  Ok(())
}
