use xtask::webidl::{
  extract_webidl_blocks, extract_webidl_blocks_from_bikeshed, extract_webidl_blocks_from_whatwg_html,
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
  let blocks = extract_webidl_blocks_from_whatwg_html(src);
  assert_eq!(blocks.len(), 1);
  assert!(
    blocks[0].contains("interface Foo { attribute DOMString bar; };"),
    "expected stripped IDL, got:\n{}",
    blocks[0]
  );
}

#[test]
fn whatwg_html_handles_nested_code_tags_in_idl_blocks() {
  // The WHATWG HTML source nests `<code>...</code>` tags *inside* `<code class="idl">...</code>`
  // blocks for formatting (e.g. `static <code>Document</code> parse();`). Ensure the extractor
  // matches the outer closing tag instead of stopping at the first inner `</code>`.
  let src = concat!(
    "<pre><code class=\"idl\">",
    "partial interface <dfn>Document</dfn> {",
    " static <code>Document</code> <span>parse</span>();",
    "};</code></pre>",
  );
  let blocks = extract_webidl_blocks_from_whatwg_html(src);
  assert_eq!(blocks.len(), 1);
  assert!(
    blocks[0].contains("partial interface Document"),
    "expected nested tags to be stripped, got:\n{}",
    blocks[0]
  );
  assert!(
    blocks[0].contains("static Document parse()"),
    "expected nested <code> tags not to terminate the block, got:\n{}",
    blocks[0]
  );
}

#[test]
fn whatwg_html_extracts_timer_handler_typedef() {
  let src = r#"
    <pre><code class="idl extract">typedef (DOMString or <span class="t">Function</span>) <dfn typedef data-x="timer-handler">TimerHandler</dfn>;</code></pre>
  "#;

  let blocks = extract_webidl_blocks(src);
  assert_eq!(blocks.len(), 1);
  assert!(
    blocks[0].contains("typedef (DOMString or Function) TimerHandler;"),
    "expected stripped typedef, got:\n{}",
    blocks[0]
  );
}
