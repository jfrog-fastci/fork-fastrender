use crate::common::accessibility::render_accessibility_tree;
use fastrender::accessibility_audit::audit_accessibility_tree;

#[test]
fn accessibility_audit_passes_named_button_via_aria_label() {
  let html = r##"
    <html>
      <body>
        <button aria-label="Back"></button>
      </body>
    </html>
  "##;

  let tree = render_accessibility_tree(html);
  let issues = audit_accessibility_tree(&tree);
  assert!(
    issues.is_empty(),
    "expected no accessibility audit issues, got: {issues:?}"
  );
}

#[test]
fn accessibility_audit_passes_named_button_via_text_content() {
  let html = r##"
    <html>
      <body>
        <button>Back</button>
      </body>
    </html>
  "##;

  let tree = render_accessibility_tree(html);
  let issues = audit_accessibility_tree(&tree);
  assert!(
    issues.is_empty(),
    "expected no accessibility audit issues, got: {issues:?}"
  );
}

#[test]
fn accessibility_audit_reports_unnamed_button() {
  let html = r##"
    <html>
      <body>
        <button></button>
      </body>
    </html>
  "##;

  let tree = render_accessibility_tree(html);
  let issues = audit_accessibility_tree(&tree);
  assert!(
    issues.iter().any(|issue| issue.role == "button"),
    "expected an audit issue for the unnamed button, got: {issues:?}"
  );
}

