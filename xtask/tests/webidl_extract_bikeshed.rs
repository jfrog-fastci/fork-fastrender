use xtask::webidl::extract_webidl_blocks_from_bikeshed;

#[test]
fn bikeshed_extraction_continues_after_unclosed_idl_block() {
  // Regression test: the Bikeshed extractor must never stop scanning the whole file when it
  // encounters a malformed `<pre class=idl>` block (e.g. missing its closing `</pre>`).
  let src = concat!(
    "<pre class=\"idl\">",
    "interface Bad { attribute DOMString y; };",
    // Missing closing `</pre>` for the first block.
    "<p>interlude</p>",
    "<pre class=\"idl\">",
    "interface Good { attribute DOMString x; };",
    "</pre>",
  );

  let blocks = extract_webidl_blocks_from_bikeshed(src);
  assert!(
    blocks
      .iter()
      .any(|b| b.contains("interface Good { attribute DOMString x; };")),
    "expected to extract the later well-formed block, got:\n{blocks:#?}"
  );
}

#[test]
fn bikeshed_does_not_confuse_prefix_for_pre_when_scanning() {
  // Regression test: nested HTML tags whose tag name starts with "pre" (e.g. `<prefix>`) must not
  // be treated as `<pre>` tags.
  let src = concat!(
    "<prefix>ignored</prefix>",
    "<pre class=\"idl\">interface Foo { attribute DOMString x; }; </pre>",
  );

  let blocks = extract_webidl_blocks_from_bikeshed(src);
  assert_eq!(blocks.len(), 1);
  assert!(
    blocks[0].contains("interface Foo { attribute DOMString x; };"),
    "expected to extract Foo block, got:\n{}",
    blocks[0]
  );
}

