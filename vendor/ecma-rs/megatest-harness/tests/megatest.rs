use anyhow::{anyhow, Context, Result};
use megatest_harness::{
  discover_fixtures, filter_fixtures, load_baseline, megatest_filter, optimize, parse_and_lower,
  read_source, source_sha256,
};
use std::collections::BTreeSet;

#[test]
fn megatest_parse_and_lower_match_baseline() -> Result<()> {
  let baseline = load_baseline()?;
  let filter = megatest_filter();
  let fixtures = filter_fixtures(discover_fixtures()?, filter.as_deref());

  if filter.is_none() {
    let expected: BTreeSet<_> = baseline.files.keys().cloned().collect();
    let actual: BTreeSet<_> = fixtures.iter().map(|f| f.name.clone()).collect();
    if expected != actual {
      return Err(anyhow!(
        "baseline entries do not match discovered fixtures (run `bash scripts/cargo_agent.sh run -p megatest-harness -- --update-baselines`)\n\
 expected: {expected:#?}\n\
 actual: {actual:#?}",
      ));
    }
  }

  for fixture in fixtures {
    let expected = baseline
      .files
      .get(&fixture.name)
      .with_context(|| format!("missing baseline entry for {fixture}"))?;

    let source = read_source(&fixture.path)?;
    let actual_sha = source_sha256(&source);
    if actual_sha != expected.source_sha256 {
      return Err(anyhow!(
        "source hash mismatch for {fixture}: expected {}, got {}",
        expected.source_sha256,
        actual_sha
      ));
    }

    let (parse, hir) = parse_and_lower(&source)?;
    // Sanity: ensure parse+lower is deterministic within a single process run. This catches
    // accidental global-counter/non-deterministic ID allocation regressions even if the baseline
    // happened to match one of the outputs.
    let (parse2, hir2) = parse_and_lower(&source)?;
    if parse != parse2 {
      return Err(anyhow!(
        "non-deterministic parse output for {fixture}: first {:#?}, second {:#?}",
        parse,
        parse2
      ));
    }
    if hir != hir2 {
      return Err(anyhow!(
        "non-deterministic hir output for {fixture}: first {:#?}, second {:#?}",
        hir,
        hir2
      ));
    }
    if parse != expected.parse {
      return Err(anyhow!(
        "parse mismatch for {fixture}: expected {:#?}, got {:#?}",
        expected.parse,
        parse
      ));
    }
    if hir != expected.hir {
      return Err(anyhow!(
        "hir mismatch for {fixture}: expected {:#?}, got {:#?}",
        expected.hir,
        hir
      ));
    }
  }

  Ok(())
}

#[test]
#[ignore]
fn megatest_optimize_match_baseline() -> Result<()> {
  let baseline = load_baseline()?;
  let filter = megatest_filter();
  let fixtures = filter_fixtures(discover_fixtures()?, filter.as_deref());

  for fixture in fixtures {
    let expected = baseline
      .files
      .get(&fixture.name)
      .with_context(|| format!("missing baseline entry for {fixture}"))?;

    let source = read_source(&fixture.path)?;
    let actual_sha = source_sha256(&source);
    if actual_sha != expected.source_sha256 {
      return Err(anyhow!(
        "source hash mismatch for {fixture}: expected {}, got {}",
        expected.source_sha256,
        actual_sha
      ));
    }

    let actual = optimize(&source)?;
    let actual2 = optimize(&source)?;
    if actual != actual2 {
      return Err(anyhow!(
        "non-deterministic optimize output for {fixture}: first {:#?}, second {:#?}",
        actual,
        actual2
      ));
    }
    if actual != expected.optimize {
      return Err(anyhow!(
        "optimize mismatch for {fixture}: expected {:#?}, got {:#?}",
        expected.optimize,
        actual
      ));
    }
  }

  Ok(())
}
