use std::path::Path;

use xtask::webidl::extract_webidl_blocks_from_whatwg_html;

#[test]
fn whatwg_html_extraction_continues_after_unclosed_idl_block() {
  // Regression test: the extractor must never stop scanning the whole file when it encounters a
  // malformed `<code class="idl">` block (e.g. missing its closing `</code>`).
  let src = concat!(
    "<pre><code class=\"idl\">",
    "interface Bad { attribute DOMString y; };",
    // Missing closing `</code>` for the first block.
    "<p>interlude</p>",
    "<pre><code class=\"idl\">",
    "interface Good { attribute DOMString x; };",
    "</code></pre>",
  );

  let blocks = extract_webidl_blocks_from_whatwg_html(src);
  assert!(
    blocks
      .iter()
      .any(|b| b.contains("interface Good { attribute DOMString x; };")),
    "expected to extract the later well-formed block, got:\n{blocks:#?}"
  );
}

#[test]
fn whatwg_html_does_not_confuse_codec_for_code_when_matching_closing_tags() {
  // Regression test: nested HTML tags whose tag name starts with "code" (e.g. `<codec>`) must not
  // be treated as nested `<code>` tags for depth tracking, otherwise the extractor may fail to find
  // the matching outer `</code>` and drop the whole IDL block.
  let src = concat!(
    "<pre><code class=\"idl\">",
    "interface Foo { <codec>attribute DOMString x; };",
    "</code></pre>",
  );

  let blocks = extract_webidl_blocks_from_whatwg_html(src);
  assert_eq!(blocks.len(), 1);
  assert!(
    blocks[0].contains("interface Foo { attribute DOMString x; };"),
    "expected nested <codec> tag to be ignored for depth tracking, got:\n{}",
    blocks[0]
  );
}

#[test]
fn whatwg_html_strips_html_comments_without_truncating_idl() {
  // Regression test: WHATWG HTML IDL blocks sometimes embed literal HTML comments inside
  // `<code class="idl">` (see HTMLSelectElement in the real spec). These comments can contain `'`
  // and `"` characters that must not confuse tag parsing / cause the rest of the IDL block to be
  // dropped.
  let src = concat!(
    "<pre><code class=\"idl\">",
    "interface Foo { attribute DOMString x;",
    "<!-- it's not a quote -->",
    "attribute DOMString y; };",
    "</code></pre>",
  );

  let blocks = extract_webidl_blocks_from_whatwg_html(src);
  assert_eq!(blocks.len(), 1);
  assert!(
    blocks[0].contains("attribute DOMString y;"),
    "expected IDL after the comment to be preserved, got:\n{}",
    blocks[0]
  );
  assert!(
    blocks[0].contains("};"),
    "expected closing brace to be preserved, got:\n{}",
    blocks[0]
  );
}

#[test]
fn whatwg_html_source_extracts_window_or_worker_globals_and_timer_handler() {
  // Optional integration test: verify our vendored WHATWG HTML `source` file contains the late-file
  // globals we care about (WindowOrWorkerGlobalScope, TimerHandler, etc).
  //
  // CI may skip spec submodules, so this test must be conditional on the file existing.
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let source_path = repo_root.join("specs/whatwg-html/source");
  if !source_path.exists() {
    eprintln!(
      "skipping WHATWG HTML extraction integration test: missing {}",
      source_path.display()
    );
    return;
  }

  let src = std::fs::read_to_string(&source_path).expect("read WHATWG HTML source");
  let blocks = extract_webidl_blocks_from_whatwg_html(&src);

  assert!(
    blocks
      .iter()
      .any(|b| b.contains("interface mixin WindowOrWorkerGlobalScope")),
    "expected extracted IDL to contain WindowOrWorkerGlobalScope mixin"
  );
  assert!(
    blocks
      .iter()
      .any(|b| b.contains("typedef (DOMString or Function or TrustedScript) TimerHandler")),
    "expected extracted IDL to contain TimerHandler typedef"
  );
}
