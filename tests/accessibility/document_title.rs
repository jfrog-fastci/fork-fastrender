use crate::common::accessibility::render_accessibility_tree;

#[test]
fn accessibility_document_name_uses_html_title() {
  let html = r##"
    <html>
      <head><title>  My   Page  </title></head>
      <body><div id="x">Hello</div></body>
    </html>
  "##;

  let tree = render_accessibility_tree(html);
  assert_eq!(tree.role, "document");
  assert_eq!(tree.name.as_deref(), Some("My Page"));
}

