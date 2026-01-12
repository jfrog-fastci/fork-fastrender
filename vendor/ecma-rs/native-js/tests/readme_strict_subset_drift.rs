use std::path::PathBuf;

fn readme_text() -> String {
  let readme_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
  std::fs::read_to_string(&readme_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", readme_path.display()))
}

fn section<'a>(text: &'a str, heading: &str) -> &'a str {
  let start = text
    .find(heading)
    .unwrap_or_else(|| panic!("missing heading {heading:?} in README"));

  // Find the next "## " heading (start-of-line) after this section's heading.
  let end = text[start + heading.len()..]
    .find("\n## ")
    .map(|rel| start + heading.len() + rel)
    .unwrap_or(text.len());

  &text[start..end]
}

#[test]
fn strict_subset_readme_does_not_claim_supported_ops_are_rejected() {
  let readme = readme_text();
  let strict_subset = section(&readme, "## Strict compilation subset (`native_js::validate`)");

  // `native_js::validate::validate_strict_subset` currently allows these ops, so the strict-subset README section
  // must not list them as rejected/unsupported.
  let forbidden_ops_in_rejected_list = ["`>>>`", "`&&`", "`||`", "comma operator"];

  let offending_lines: Vec<&str> = strict_subset
    .lines()
    // The README lists unsupported operators in a `binary:` bullet under the strict-subset section.
    .filter(|line| line.contains("binary:"))
    .filter(|line| forbidden_ops_in_rejected_list.iter().any(|op| line.contains(op)))
    .collect();

  assert!(
    offending_lines.is_empty(),
    "native-js README strict-subset section claims supported operators are rejected:\n{}",
    offending_lines.join("\n")
  );
}

#[test]
fn strict_subset_readme_matches_current_checked_backend_and_validator() {
  let readme = readme_text();
  let strict_subset = section(&readme, "## Strict compilation subset (`native_js::validate`)");

  // Catch obviously outdated claims that have regressed in the past.
  let forbidden_phrases = [
    "i32-only",
    "numeric literals that are not 32-bit signed integers",
  ];
  for phrase in forbidden_phrases {
    assert!(
      !strict_subset.contains(phrase),
      "native-js README strict-subset section contains outdated phrase {phrase:?}"
    );
  }

  // Keep documentation in sync with strict-subset soundness diagnostics around TS-only wrappers (`as`, `!`).
  for code in ["NJS0013", "NJS0014"] {
    assert!(
      strict_subset.contains(code),
      "native-js README strict-subset section must mention {code} (validator emits this diagnostic)"
    );
  }

  // `number` is currently lowered as `double` (`f64`) by the checked/HIR backend.
  assert!(
    strict_subset.contains("double") || strict_subset.contains("f64"),
    "native-js README strict-subset section must document `number` lowering as `double`/`f64`"
  );
}

#[test]
fn strict_subset_readme_mentions_object_literals_and_static_property_access() {
  let readme = readme_text();
  let strict_subset = section(&readme, "## Strict compilation subset (`native_js::validate`)");

  assert!(
    strict_subset.contains("object literals are supported"),
    "native-js README strict-subset section should mention that object literals are supported:\n{strict_subset}"
  );
  assert!(
    !strict_subset.contains("object literals and destructuring patterns"),
    "native-js README strict-subset section still claims object literals are rejected:\n{strict_subset}"
  );
  assert!(
    !strict_subset.contains("most property access (`obj.prop`, `obj[\"prop\"]`)"),
    "native-js README strict-subset section still claims most property access is rejected:\n{strict_subset}"
  );
  assert!(
    strict_subset.contains("`obj.foo`"),
    "native-js README strict-subset section should document static object property access (e.g. `obj.foo`):\n{strict_subset}"
  );
}
