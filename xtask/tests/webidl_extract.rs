use xtask::webidl::{
  extract_webidl_blocks, extract_webidl_blocks_from_bikeshed, WebIdlSourceFormat,
};

#[test]
fn bikeshed_inline_idl_without_gt_is_captured() {
  // Regression test for WHATWG Fetch's Response block, which is written as:
  //   `<pre class=idl>[Exposed=(Window,Worker)]`
  let src = r#"
<pre class=idl>[Exposed=(Window,Worker)]
interface Response {
};
</pre>
"#;

  let blocks = extract_webidl_blocks_from_bikeshed(src);
  assert_eq!(blocks.len(), 1);
  assert!(
    blocks[0].starts_with("[Exposed=(Window,Worker)]"),
    "expected IDL block to start with Exposed attribute, got:\n{}",
    blocks[0]
  );
}

#[test]
fn whatwg_html_strips_inline_markup_in_idl_blocks() {
  let src = r#"<pre><code class="idl">interface <dfn>Foo</dfn> { attribute <span>DOMString</span> <span>bar</span>; };</code></pre>"#;
  let blocks = extract_webidl_blocks(src, WebIdlSourceFormat::WhatwgHtml);
  assert_eq!(blocks.len(), 1);
  assert!(
    blocks[0].contains("interface Foo { attribute DOMString bar; };"),
    "expected stripped IDL, got:\n{}",
    blocks[0]
  );
}

