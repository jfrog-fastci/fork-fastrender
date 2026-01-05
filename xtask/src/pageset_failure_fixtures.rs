use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct PagesetAccuracyMetrics {
  pub diff_percent: f64,
  pub perceptual: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct PagesetProgressPage {
  pub stem: String,
  pub status: String,
  pub url: Option<String>,
  pub accuracy: Option<PagesetAccuracyMetrics>,
  pub progress_path: PathBuf,
  pub fixture_index_path: PathBuf,
  pub has_fixture: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagesetFailurePage {
  pub stem: String,
  pub status: String,
  pub url: Option<String>,
  pub progress_path: PathBuf,
  pub fixture_index_path: PathBuf,
  pub has_fixture: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagesetFailureFixturesPlan {
  pub failing_pages: Vec<PagesetFailurePage>,
  pub missing_fixtures: Vec<PagesetFailurePage>,
  pub existing_fixtures: Vec<PagesetFailurePage>,
}

/// Scan pageset progress artifacts and determine which failing pages are missing offline fixtures.
///
/// The repository's "offline repro" contract is:
/// - any `progress/pages/<stem>.json` entry whose `status != ok` should have an offline fixture at
///   `tests/pages/fixtures/<stem>/index.html`.
pub fn plan_missing_failure_fixtures(
  progress_pages_dir: &Path,
  fixtures_root: &Path,
) -> Result<PagesetFailureFixturesPlan> {
  let failing_pages = read_progress_pages(progress_pages_dir, fixtures_root)?
    .into_iter()
    .filter(|page| page.status != "ok")
    .map(|page| PagesetFailurePage {
      stem: page.stem,
      status: page.status,
      url: page.url,
      progress_path: page.progress_path,
      fixture_index_path: page.fixture_index_path,
      has_fixture: page.has_fixture,
    })
    .collect::<Vec<_>>();

  let mut existing_fixtures = Vec::new();
  let mut missing_fixtures = Vec::new();
  for page in &failing_pages {
    if page.has_fixture {
      existing_fixtures.push(page.clone());
    } else {
      missing_fixtures.push(page.clone());
    }
  }

  Ok(PagesetFailureFixturesPlan {
    failing_pages,
    missing_fixtures,
    existing_fixtures,
  })
}

#[derive(Debug, Deserialize)]
struct ProgressAccuracy {
  #[serde(default)]
  diff_percent: Option<f64>,
  #[serde(default)]
  perceptual: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct PageProgress {
  #[serde(default = "default_status")]
  status: String,
  #[serde(default)]
  url: Option<String>,
  #[serde(default)]
  accuracy: Option<ProgressAccuracy>,
}

fn default_status() -> String {
  "ok".to_string()
}

pub fn read_progress_pages(
  progress_pages_dir: &Path,
  fixtures_root: &Path,
) -> Result<Vec<PagesetProgressPage>> {
  let mut pages = Vec::new();

  for entry in fs::read_dir(progress_pages_dir).with_context(|| {
    format!(
      "failed to read progress directory {}",
      progress_pages_dir.display()
    )
  })? {
    let entry = entry.with_context(|| {
      format!(
        "failed to read directory entry in {}",
        progress_pages_dir.display()
      )
    })?;
    let path = entry.path();

    if !path.is_file() {
      continue;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
      continue;
    }

    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
      continue;
    };

    let contents =
      fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let progress: PageProgress = serde_json::from_str(&contents)
      .with_context(|| format!("failed to parse {}", path.display()))?;

    let accuracy = progress
      .accuracy
      .and_then(|accuracy| {
        accuracy
          .diff_percent
          .map(|diff_percent| (diff_percent, accuracy))
      })
      .filter(|(diff_percent, _)| diff_percent.is_finite())
      .map(|(diff_percent, accuracy)| PagesetAccuracyMetrics {
        diff_percent,
        perceptual: accuracy.perceptual.filter(|value| value.is_finite()),
      });
    let fixture_index_path = fixtures_root.join(stem).join("index.html");
    let has_fixture = fixture_index_path.is_file();

    pages.push(PagesetProgressPage {
      stem: stem.to_string(),
      status: progress.status,
      url: progress.url,
      accuracy,
      progress_path: path,
      fixture_index_path,
      has_fixture,
    });
  }

  pages.sort_by(|a, b| a.stem.cmp(&b.stem));
  Ok(pages)
}
