use fastrender::dom::enumerate_dom_ids;
use fastrender::dom::parse_html;
use fastrender::dom::DomNode;
use fastrender::interaction::form_submission_get_url;

fn find_by_id<'a>(root: &'a DomNode, html_id: &str) -> Option<&'a DomNode> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(html_id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn node_id(root: &DomNode, html_id: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let node = find_by_id(root, html_id).expect("node");
  ids
    .get(&(node as *const DomNode))
    .copied()
    .expect("id present")
}

#[test]
fn get_submission_builds_query_and_resolves_action() {
  let dom = parse_html(
    r#"
    <html><body>
      <form action="/search">
        <input name="q" value="hi">
        <button id="submit" type="submit" name="btn" value="go">Go</button>
      </form>
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  let got = form_submission_get_url(
    &dom,
    submitter_id,
    "https://example.com/doc",
    "https://example.com/base/",
  )
  .unwrap();
  assert_eq!(got, "https://example.com/search?q=hi&btn=go");
}

#[test]
fn checkbox_and_radio_inclusion_rules() {
  let dom = parse_html(
    r#"
    <html><body>
      <form action="/submit">
        <input type="checkbox" name="a" checked>
        <input type="checkbox" name="b">
        <input type="checkbox" name="c" checked value="yes">
        <input type="radio" name="r" value="1" checked>
        <input type="radio" name="r" value="2">
        <button id="submit" type="submit">Go</button>
      </form>
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  let got = form_submission_get_url(&dom, submitter_id, "https://example.com/doc", "https://example.com/")
    .unwrap();
  assert_eq!(got, "https://example.com/submit?a=on&c=yes&r=1");
}

#[test]
fn select_multiple_serializes_multiple_pairs() {
  let dom = parse_html(
    r#"
    <html><body>
      <form action="/s">
        <select name="s" multiple>
          <option value="a" selected>A</option>
          <option value="b">B</option>
          <option selected>C</option>
        </select>
        <button id="submit" type="submit" name="go" value="1">Go</button>
      </form>
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  let got =
    form_submission_get_url(&dom, submitter_id, "https://example.com/doc", "https://example.com/")
      .unwrap();
  assert_eq!(got, "https://example.com/s?s=a&s=C&go=1");
}

#[test]
fn action_query_is_replaced() {
  let dom = parse_html(
    r#"
    <html><body>
      <form action="/search?existing=1">
        <input name="q" value="hi">
        <button id="submit" type="submit">Go</button>
      </form>
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  let got =
    form_submission_get_url(&dom, submitter_id, "https://example.com/doc", "https://example.com/")
      .unwrap();
  assert_eq!(got, "https://example.com/search?q=hi");
}

#[test]
fn action_fragment_is_stripped() {
  let dom = parse_html(
    r#"
    <html><body>
      <form action="/search#frag">
        <input name="q" value="hi">
        <button id="submit" type="submit">Go</button>
      </form>
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  let got =
    form_submission_get_url(&dom, submitter_id, "https://example.com/doc", "https://example.com/")
      .unwrap();
  assert_eq!(
    got,
    "https://example.com/search?q=hi",
    "form submission should discard fragments from the action URL"
  );
}

#[test]
fn includes_form_associated_controls_outside_form_subtree() {
  let dom = parse_html(
    r#"
    <html><body>
      <form id="f" action="/search">
        <input name="a" value="1">
        <button id="submit" type="submit">Go</button>
      </form>
      <input name="b" value="2" form="f">
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  let got = form_submission_get_url(
    &dom,
    submitter_id,
    "https://example.com/doc",
    "https://example.com/base/",
  )
  .unwrap();
  assert_eq!(got, "https://example.com/search?a=1&b=2");
}

#[test]
fn form_attribute_does_not_cross_shadow_root_boundary() {
  // Submitter is inside a shadow root and references `form="f"`. A light-DOM form with the same id
  // exists, but `form` associations are scoped to the submitter's tree root boundary (shadow root),
  // so the association should be ignored and submission should not occur.
  let dom = parse_html(
    r#"
    <html><body>
      <form id="f" action="/search">
        <input name="q" value="light">
      </form>
      <div>
        <template shadowrootmode="open">
          <button id="submit" type="submit" form="f">Go</button>
        </template>
      </div>
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  assert_eq!(
    form_submission_get_url(
      &dom,
      submitter_id,
      "https://example.com/doc",
      "https://example.com/base/",
    ),
    None
  );
}

#[test]
fn post_method_returns_none_mvp() {
  let dom = parse_html(
    r#"
    <html><body>
      <form action="/search" method="post">
        <input name="q" value="hi">
        <button id="submit" type="submit">Go</button>
      </form>
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  assert_eq!(
    form_submission_get_url(&dom, submitter_id, "https://example.com/doc", "https://example.com/"),
    None
  );
}

#[test]
fn get_submission_sanitizes_input_values() {
  let dom = parse_html(
    r#"
    <html><body>
      <form action="/submit">
        <input type="number" name="n" value="abc">
        <input type="date" name="d" value="2020-13-01">
        <input type="color" name="c" value="not-a-color">
        <input name="t" value="a&#10;b">
        <button id="submit" type="submit">Go</button>
      </form>
    </body></html>
    "#,
  )
  .unwrap();
  let submitter_id = node_id(&dom, "submit");

  let got = form_submission_get_url(&dom, submitter_id, "https://example.com/doc", "https://example.com/")
    .unwrap();
  assert_eq!(
    got,
    "https://example.com/submit?n=&d=&c=%23000000&t=ab",
    "HTML form submission uses each control's sanitized value"
  );
}
