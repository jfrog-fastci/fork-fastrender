use crate::frontmatter::Frontmatter;
use crate::report::Variant;
use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use std::collections::HashSet;
use std::path::Path;

/// Separator marker inserted by [`assemble_source`] for `Variant::Module`.
///
/// Module tests need the harness prelude to run as a classic script to populate the global object
/// before the test body is evaluated as an ECMAScript module.
///
/// The marker is intentionally **optional**:
/// - When the effective harness mode is `none`, there is no harness prelude to separate.
/// - `flags: [raw]` tests must not have their source modified.
pub(crate) const MODULE_SEPARATOR_MARKER: &str = "\n/* test262-semantic:module */\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum HarnessMode {
  /// Prepend the standard test262 harness (`assert.js`, `sta.js`) plus any additional frontmatter
  /// includes.
  Test262,
  /// Prepend only the harness files explicitly listed in test frontmatter (`includes`).
  Includes,
  /// Prepend no harness files at all.
  None,
}

pub fn assemble_source(
  test262_dir: &Path,
  frontmatter: &Frontmatter,
  variant: Variant,
  body: &str,
  harness_mode: HarnessMode,
) -> Result<String> {
  let is_raw = frontmatter.flags.iter().any(|f| f == "raw");
  let is_async = frontmatter.flags.iter().any(|f| f == "async");
  // `flags: [raw]` requires the test source to be executed verbatim (no harness injection, no
  // module-separator marker, etc).
  let harness_mode = if is_raw {
    HarnessMode::None
  } else {
    harness_mode
  };

  let mut out = String::new();
  // Ensure strict-mode variants actually run in strict mode: the directive must appear before any
  // other statements (including harness includes), otherwise the directive prologue is already
  // terminated.
  //
  // Do not inject `'use strict'` for `flags: [raw]` tests: those tests must be executed exactly as
  // authored.
  if variant == Variant::Strict && !is_raw {
    out.push_str("'use strict';\n\n");
  }

  // Minimal async harness support: test262 async tests signal completion by calling `$DONE()`.
  //
  // The upstream `doneprintHandle.js` implementation uses a host-provided `print` to communicate
  // completion back to the runner. `test262-semantic` does not currently capture stdout from `vm-js`,
  // so instead we:
  // - mark completion via a global flag (`__test262AsyncDone__`), and
  // - treat `$DONE(error)` as a thrown exception so failures are observable by the executor.
  //
  // This is intentionally small: it is sufficient for Promise-based async tests that only rely on
  // microtasks (no timers/event loop).
  if is_async && !is_raw {
    out.push_str("var __test262AsyncDone__ = false;\n");
    out.push_str("function $DONE(error) {\n");
    out.push_str("  __test262AsyncDone__ = true;\n");
    out.push_str("  if (error) { throw error; }\n");
    out.push_str("}\n\n");
  }

  // In `none` mode, do not touch the filesystem at all.
  if harness_mode == HarnessMode::None {
    out.push_str(body);
    return Ok(out);
  }

  let harness_dir = test262_dir.join("harness");
  if !harness_dir.is_dir() {
    bail!(
      "test262 harness directory not found at {} (expected a tc39/test262 checkout)",
      harness_dir.display()
    );
  }

  let mut includes: Vec<String> = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();
  let mut maybe_push = |name: &str| {
    let name = name.to_string();
    if seen.insert(name.clone()) {
      includes.push(name);
    }
  };

  match harness_mode {
    HarnessMode::Test262 => {
      maybe_push("assert.js");
      maybe_push("sta.js");
      for include in &frontmatter.includes {
        maybe_push(include);
      }
    }
    HarnessMode::Includes => {
      for include in &frontmatter.includes {
        maybe_push(include);
      }
    }
    HarnessMode::None => {}
  }

  for include in includes {
    let path = harness_dir.join(&include);
    let content = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    out.push_str(&content);
    if !content.ends_with('\n') {
      out.push('\n');
    }
    out.push('\n');
  }

  if variant == Variant::Module {
    out.push_str(MODULE_SEPARATOR_MARKER);
  }
  out.push_str(body);
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::tempdir;

  fn setup_test262_dir() -> tempfile::TempDir {
    let temp = tempdir().unwrap();
    let harness_dir = temp.path().join("harness");
    fs::create_dir_all(&harness_dir).unwrap();
    fs::write(harness_dir.join("assert.js"), "/*assert*/\n").unwrap();
    fs::write(harness_dir.join("sta.js"), "/*sta*/\n").unwrap();
    fs::write(harness_dir.join("helper.js"), "/*helper*/\n").unwrap();
    temp
  }

  #[test]
  fn test262_mode_includes_default_harness_and_dedupes_frontmatter() {
    let temp = setup_test262_dir();
    let frontmatter = Frontmatter {
      // `assert.js` appears both as an implicit default include and explicitly in frontmatter; it
      // should appear only once.
      includes: vec!["assert.js".to_string(), "helper.js".to_string()],
      ..Frontmatter::default()
    };

    let body = "/*body*/\n";
    let source = assemble_source(
      temp.path(),
      &frontmatter,
      Variant::NonStrict,
      body,
      HarnessMode::Test262,
    )
    .unwrap();

    assert_eq!(source.match_indices("/*assert*/").count(), 1);
    assert_eq!(source.match_indices("/*sta*/").count(), 1);
    assert_eq!(source.match_indices("/*helper*/").count(), 1);

    let assert_pos = source.find("/*assert*/").unwrap();
    let sta_pos = source.find("/*sta*/").unwrap();
    let helper_pos = source.find("/*helper*/").unwrap();
    let body_pos = source.find("/*body*/").unwrap();
    assert!(assert_pos < sta_pos);
    assert!(sta_pos < helper_pos);
    assert!(helper_pos < body_pos);
  }

  #[test]
  fn includes_mode_omits_default_harness_when_not_explicitly_included() {
    let temp = setup_test262_dir();
    let frontmatter = Frontmatter::default();
    let body = "/*body*/\n";

    let source = assemble_source(
      temp.path(),
      &frontmatter,
      Variant::NonStrict,
      body,
      HarnessMode::Includes,
    )
    .unwrap();

    assert_eq!(source, body);
  }

  #[test]
  fn includes_mode_includes_frontmatter_includes_and_dedupes() {
    let temp = setup_test262_dir();
    let frontmatter = Frontmatter {
      includes: vec![
        "assert.js".to_string(),
        "assert.js".to_string(),
        "helper.js".to_string(),
      ],
      ..Frontmatter::default()
    };
    let body = "/*body*/\n";

    let source = assemble_source(
      temp.path(),
      &frontmatter,
      Variant::NonStrict,
      body,
      HarnessMode::Includes,
    )
    .unwrap();

    assert_eq!(source.match_indices("/*assert*/").count(), 1);
    assert_eq!(source.match_indices("/*helper*/").count(), 1);
    assert_eq!(source.match_indices("/*sta*/").count(), 0);

    let assert_pos = source.find("/*assert*/").unwrap();
    let helper_pos = source.find("/*helper*/").unwrap();
    let body_pos = source.find("/*body*/").unwrap();
    assert!(assert_pos < helper_pos);
    assert!(helper_pos < body_pos);
  }

  #[test]
  fn strict_variant_begins_with_use_strict_directive() {
    let dir = tempdir().unwrap();
    let harness_dir = dir.path().join("harness");
    fs::create_dir_all(&harness_dir).unwrap();

    // Intentionally include non-directive statements so the directive prologue would be terminated
    // if we appended 'use strict' after includes.
    fs::write(harness_dir.join("assert.js"), "var ASSERT_LOADED = true;").unwrap();
    fs::write(harness_dir.join("sta.js"), "var STA_LOADED = true;").unwrap();

    let src = assemble_source(
      dir.path(),
      &Frontmatter::default(),
      Variant::Strict,
      "body();",
      HarnessMode::Test262,
    )
    .unwrap();
    assert!(
      src.starts_with("'use strict';\n\n"),
      "strict source should begin with directive, got: {src:?}"
    );
  }

  #[test]
  fn non_strict_variant_does_not_begin_with_use_strict_directive() {
    let dir = tempdir().unwrap();
    let harness_dir = dir.path().join("harness");
    fs::create_dir_all(&harness_dir).unwrap();
    fs::write(harness_dir.join("assert.js"), "var ASSERT_LOADED = true;").unwrap();
    fs::write(harness_dir.join("sta.js"), "var STA_LOADED = true;").unwrap();

    let src = assemble_source(
      dir.path(),
      &Frontmatter::default(),
      Variant::NonStrict,
      "body();",
      HarnessMode::Test262,
    )
    .unwrap();
    assert!(
      !src.starts_with("'use strict';"),
      "non-strict source should not begin with directive, got: {src:?}"
    );
  }

  #[test]
  fn none_mode_does_not_require_harness_dir_and_still_inserts_use_strict() {
    let dir = tempdir().unwrap();
    let frontmatter = Frontmatter {
      // If `none` mode attempted to read includes, this would error because the harness directory
      // does not exist.
      includes: vec!["assert.js".to_string(), "missing.js".to_string()],
      ..Frontmatter::default()
    };

    let src = assemble_source(
      dir.path(),
      &frontmatter,
      Variant::Strict,
      "body();",
      HarnessMode::None,
    )
    .unwrap();
    assert_eq!(src, "'use strict';\n\nbody();");
  }

  #[test]
  fn includes_mode_requires_harness_dir() {
    let dir = tempdir().unwrap();
    let frontmatter = Frontmatter {
      includes: vec!["helper.js".to_string()],
      ..Frontmatter::default()
    };

    let err = assemble_source(
      dir.path(),
      &frontmatter,
      Variant::NonStrict,
      "body();",
      HarnessMode::Includes,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("test262 harness directory not found"),
      "expected missing harness directory error, got: {err:#}"
    );
  }

  #[test]
  fn module_variant_inserts_separator_marker_between_harness_and_body() {
    let temp = setup_test262_dir();
    let body = "/*body*/\n";
    let src = assemble_source(
      temp.path(),
      &Frontmatter::default(),
      Variant::Module,
      body,
      HarnessMode::Test262,
    )
    .unwrap();
    let (harness, module) = src
      .split_once(MODULE_SEPARATOR_MARKER)
      .expect("module source should contain separator marker");
    assert!(harness.contains("/*assert*/"));
    assert!(harness.contains("/*sta*/"));
    assert_eq!(module, body);
  }

  #[test]
  fn module_variant_in_none_mode_omits_separator_marker_without_fs_access() {
    let dir = tempdir().unwrap();
    let src = assemble_source(
      dir.path(),
      &Frontmatter::default(),
      Variant::Module,
      "body();",
      HarnessMode::None,
    )
    .unwrap();
    assert_eq!(src, "body();");
  }

  #[test]
  fn raw_flag_forces_harness_none_and_omits_module_separator() {
    let dir = tempdir().unwrap();
    let frontmatter = Frontmatter {
      flags: vec!["raw".to_string(), "module".to_string()],
      // If raw incorrectly attempted to load harness includes, this would error because the harness
      // directory does not exist.
      includes: vec!["assert.js".to_string(), "missing.js".to_string()],
      ..Frontmatter::default()
    };
    let body = "/*body*/\n";
    let source = assemble_source(
      dir.path(),
      &frontmatter,
      Variant::Module,
      body,
      HarnessMode::Test262,
    )
    .unwrap();
    assert_eq!(source, body);
  }

  #[test]
  fn async_flag_injects_done_harness_before_includes_and_body() {
    let temp = setup_test262_dir();
    let frontmatter = Frontmatter {
      flags: vec!["async".to_string()],
      ..Frontmatter::default()
    };
    let body = "/*body*/\n";
    let source = assemble_source(
      temp.path(),
      &frontmatter,
      Variant::NonStrict,
      body,
      HarnessMode::Test262,
    )
    .unwrap();
 
    let done_pos = source.find("function $DONE").expect("$DONE injected");
    let assert_pos = source.find("/*assert*/").expect("assert.js included");
    let body_pos = source.find("/*body*/").expect("body included");
    assert!(done_pos < assert_pos);
    assert!(assert_pos < body_pos);
  }
}
