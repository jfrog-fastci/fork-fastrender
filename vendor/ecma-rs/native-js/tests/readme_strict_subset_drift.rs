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

