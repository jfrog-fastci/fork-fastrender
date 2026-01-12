use crate::image_compare::{compare_images, decode_png, CompareConfig, ImageDiff};
use std::fs;
use std::path::{Path, PathBuf};

/// Environment variable names controlling comparison strictness.
#[derive(Clone, Copy)]
pub(crate) struct CompareEnvVars<'a> {
  /// When set, use the fuzzy preset (tolerance 10, up to 1% different).
  pub(crate) fuzzy: &'a str,
  /// Optional per-channel tolerance override.
  pub(crate) tolerance: &'a str,
  /// Optional percentage override for the number of differing pixels.
  pub(crate) max_diff_percent: &'a str,
  /// When set, ignore alpha differences even without fuzzy mode.
  pub(crate) ignore_alpha: &'a str,
  /// Optional perceptual distance override.
  pub(crate) max_perceptual_distance: &'a str,
}

impl CompareEnvVars<'_> {
  /// Standard fixture env vars.
  pub(crate) const fn fixtures() -> Self {
    Self {
      fuzzy: "FIXTURE_FUZZY",
      tolerance: "FIXTURE_TOLERANCE",
      max_diff_percent: "FIXTURE_MAX_DIFFERENT_PERCENT",
      ignore_alpha: "FIXTURE_IGNORE_ALPHA",
      max_perceptual_distance: "FIXTURE_MAX_PERCEPTUAL_DISTANCE",
    }
  }

  /// Env vars for the offline page regression suite.
  pub(crate) const fn pages() -> Self {
    Self {
      fuzzy: "PAGES_FUZZY",
      tolerance: "PAGES_TOLERANCE",
      max_diff_percent: "PAGES_MAX_DIFFERENT_PERCENT",
      ignore_alpha: "PAGES_IGNORE_ALPHA",
      max_perceptual_distance: "PAGES_MAX_PERCEPTUAL_DISTANCE",
    }
  }
}

fn compare_config_from_vars(
  env: CompareEnvVars<'_>,
  get: impl Fn(&str) -> Option<String>,
) -> Result<CompareConfig, String> {
  let mut config = if get(env.fuzzy).is_some() {
    CompareConfig::fuzzy()
  } else {
    CompareConfig::strict()
  };

  if let Some(tolerance) = get(env.tolerance) {
    let parsed = tolerance
      .parse::<u8>()
      .map_err(|e| format!("Invalid {} '{}': {}", env.tolerance, tolerance, e))?;
    config = config.with_channel_tolerance(parsed);
  }

  if let Some(percent) = get(env.max_diff_percent) {
    let parsed = percent
      .parse::<f64>()
      .map_err(|e| format!("Invalid {} '{}': {}", env.max_diff_percent, percent, e))?;
    config = config.with_max_different_percent(parsed);
  }

  if get(env.ignore_alpha).is_some() {
    config = config.with_compare_alpha(false);
  }

  if let Some(distance) = get(env.max_perceptual_distance) {
    let parsed = distance.parse::<f64>().map_err(|e| {
      format!(
        "Invalid {} '{}': {}",
        env.max_perceptual_distance, distance, e
      )
    })?;
    config = config.with_max_perceptual_distance(Some(parsed));
  }

  // Always generate diff images to aid debugging.
  config.generate_diff_image = true;

  Ok(config)
}

/// Build a comparison config honoring common fuzz/tolerance env vars.
pub(crate) fn compare_config_from_env(env: CompareEnvVars<'_>) -> Result<CompareConfig, String> {
  compare_config_from_vars(env, |name| std::env::var(name).ok())
}

/// Artifact paths saved when a comparison fails.
pub(crate) struct ArtifactPaths {
  pub(crate) output_dir: PathBuf,
  pub(crate) actual: PathBuf,
  pub(crate) expected: PathBuf,
  pub(crate) diff: Option<PathBuf>,
}

/// Save actual/expected/diff PNGs for debugging a mismatch.
pub(crate) fn save_artifacts(
  name: &str,
  rendered_png: &[u8],
  golden_png: &[u8],
  diff: &ImageDiff,
  output_dir: &Path,
) -> Result<ArtifactPaths, String> {
  fs::create_dir_all(output_dir).map_err(|e| {
    format!(
      "Failed to create diff output directory {}: {}",
      output_dir.display(),
      e
    )
  })?;

  let actual_path = output_dir.join(format!("{}_actual.png", name));
  fs::write(&actual_path, rendered_png).map_err(|e| {
    format!(
      "Failed to write actual image to {}: {}",
      actual_path.display(),
      e
    )
  })?;

  let expected_path = output_dir.join(format!("{}_expected.png", name));
  fs::write(&expected_path, golden_png).map_err(|e| {
    format!(
      "Failed to write expected image to {}: {}",
      expected_path.display(),
      e
    )
  })?;

  let diff_path = output_dir.join(format!("{}_diff.png", name));
  let diff_png = diff
    .diff_png()
    .map_err(|e| format!("Failed to encode diff image for {}: {}", name, e))?;
  let saved_diff_path = if let Some(png) = diff_png {
    fs::write(&diff_path, png).map_err(|e| {
      format!(
        "Failed to write diff image to {}: {}",
        diff_path.display(),
        e
      )
    })?;
    Some(diff_path)
  } else {
    None
  };

  Ok(ArtifactPaths {
    output_dir: output_dir.to_path_buf(),
    actual: actual_path,
    expected: expected_path,
    diff: saved_diff_path,
  })
}

/// Decode two PNGs, compare them, and write artifacts on mismatch.
pub(crate) fn compare_pngs(
  name: &str,
  rendered_png: &[u8],
  golden_png: &[u8],
  config: &CompareConfig,
  output_dir: &Path,
) -> Result<(), String> {
  let actual = decode_png(rendered_png)
    .map_err(|e| format!("Failed to decode rendered PNG for {}: {}", name, e))?;
  let expected =
    decode_png(golden_png).map_err(|e| format!("Failed to decode golden PNG for {}: {}", name, e))?;

  let image_diff = compare_images(&actual, &expected, config);

  if image_diff.is_match() {
    return Ok(());
  }

  let artifact_result = save_artifacts(name, rendered_png, golden_png, &image_diff, output_dir);

  let mut message = format!("Image mismatch for '{}': {}", name, image_diff.summary());

  match artifact_result {
    Ok(paths) => {
      message.push_str(&format!(
        "\nSaved artifacts to {} (actual: {}, expected: {})",
        paths.output_dir.display(),
        paths.actual.display(),
        paths.expected.display()
      ));

      if let Some(diff_path) = paths.diff {
        message.push_str(&format!("\nDiff image: {}", diff_path.display()));
      } else if !image_diff.dimensions_match {
        message.push_str("\nDiff image not generated due to dimension mismatch");
      }
    }
    Err(e) => {
      message.push_str(&format!("\nFailed to save diff artifacts: {}", e));
    }
  }

  Err(message)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

  #[test]
  fn compare_config_from_vars_uses_fuzzy_preset() {
    let env = CompareEnvVars {
      fuzzy: "FUZZY",
      tolerance: "TOLERANCE",
      max_diff_percent: "MAX_DIFF_PERCENT",
      ignore_alpha: "IGNORE_ALPHA",
      max_perceptual_distance: "MAX_PERCEPTUAL_DISTANCE",
    };
    let vars = HashMap::from([(env.fuzzy, "1")]);
    let config = compare_config_from_vars(env, |name| {
      vars.get(name).map(|value| (*value).to_string())
    })
    .unwrap();

    assert_eq!(config.channel_tolerance, 10);
    assert_eq!(config.max_different_percent, 1.0);
    assert!(!config.compare_alpha);
    assert_eq!(config.max_perceptual_distance, Some(0.05));
    assert!(config.generate_diff_image);
  }

  #[test]
  fn compare_config_from_vars_rejects_bad_tolerance() {
    let env = CompareEnvVars {
      fuzzy: "FUZZY",
      tolerance: "TOLERANCE",
      max_diff_percent: "MAX_DIFF_PERCENT",
      ignore_alpha: "IGNORE_ALPHA",
      max_perceptual_distance: "MAX_PERCEPTUAL_DISTANCE",
    };
    let vars = HashMap::from([(env.tolerance, "nope")]);
    let err = compare_config_from_vars(env, |name| {
      vars.get(name).map(|value| (*value).to_string())
    })
    .unwrap_err();

    assert!(err.contains("Invalid TOLERANCE 'nope'"));
  }

  #[test]
  fn compare_config_from_vars_rejects_bad_max_diff_percent() {
    let env = CompareEnvVars {
      fuzzy: "FUZZY",
      tolerance: "TOLERANCE",
      max_diff_percent: "MAX_DIFF_PERCENT",
      ignore_alpha: "IGNORE_ALPHA",
      max_perceptual_distance: "MAX_PERCEPTUAL_DISTANCE",
    };
    let vars = HashMap::from([(env.max_diff_percent, "wat")]);
    let err = compare_config_from_vars(env, |name| {
      vars.get(name).map(|value| (*value).to_string())
    })
    .unwrap_err();

    assert!(err.contains("Invalid MAX_DIFF_PERCENT 'wat'"));
  }

  #[test]
  fn compare_config_from_vars_can_ignore_alpha() {
    let env = CompareEnvVars {
      fuzzy: "FUZZY",
      tolerance: "TOLERANCE",
      max_diff_percent: "MAX_DIFF_PERCENT",
      ignore_alpha: "IGNORE_ALPHA",
      max_perceptual_distance: "MAX_PERCEPTUAL_DISTANCE",
    };
    let vars = HashMap::from([(env.ignore_alpha, "1")]);
    let config = compare_config_from_vars(env, |name| {
      vars.get(name).map(|value| (*value).to_string())
    })
    .unwrap();

    assert!(!config.compare_alpha);
  }

  #[test]
  fn compare_config_from_vars_parses_perceptual_distance() {
    let env = CompareEnvVars {
      fuzzy: "FUZZY",
      tolerance: "TOLERANCE",
      max_diff_percent: "MAX_DIFF_PERCENT",
      ignore_alpha: "IGNORE_ALPHA",
      max_perceptual_distance: "MAX_PERCEPTUAL_DISTANCE",
    };
    let vars = HashMap::from([(env.max_perceptual_distance, "0.123")]);
    let config = compare_config_from_vars(env, |name| {
      vars.get(name).map(|value| (*value).to_string())
    })
    .unwrap();

    assert_eq!(config.max_perceptual_distance, Some(0.123));
  }
}
