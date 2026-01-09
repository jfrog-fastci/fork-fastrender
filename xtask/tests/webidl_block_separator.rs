use xtask::webidl::parse_webidl;
use xtask::webidl::resolve::resolve_webidl_world;

// `xtask::webidl::load::load_combined_webidl` inserts this sentinel between extracted blocks.
// `parse_webidl` splits on it before doing semicolon-based statement splitting so malformed blocks
// (e.g. unbalanced `{}` after stripping HTML comments/markup) can't swallow later definitions.
const WEBIDL_BLOCK_SEPARATOR: &str = "\n//__FASTR_WEBIDL_BLOCK_SEPARATOR__\n";

#[test]
fn block_separator_prevents_unbalanced_curly_from_swallowing_following_blocks() {
  // Deliberately malformed: missing closing `};` so `{` remains unmatched at end-of-block.
  let bad_block = "interface Bad { attribute long x;";
  let good_block = "interface Good { attribute long y; };";

  // Without the separator this becomes one statement and `Good` is dropped.
  let without_sep = format!("{bad_block}\n{good_block}");
  let parsed_without = parse_webidl(&without_sep).expect("parse without separator");
  let resolved_without = resolve_webidl_world(&parsed_without);
  assert!(
    resolved_without.interface("Good").is_none(),
    "expected malformed block to swallow later definitions without block separator"
  );

  let with_sep = format!("{bad_block}{WEBIDL_BLOCK_SEPARATOR}{good_block}");
  let parsed_with = parse_webidl(&with_sep).expect("parse with separator");
  let resolved_with = resolve_webidl_world(&parsed_with);
  assert!(
    resolved_with.interface("Good").is_some(),
    "expected block separator to isolate malformed blocks and preserve later definitions"
  );
}
