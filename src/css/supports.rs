use super::properties::{
  is_global_keyword_str, is_known_style_property, parse_property_value,
  legacy_logical_offset_inset_property_alias, supports_parsed_declaration_is_valid,
  vendor_prefixed_property_alias,
};
use crate::style::var_resolution::contains_arbitrary_substitution_function;
use cssparser::Parser;
use cssparser::ParserInput;
use cssparser::Token;
use std::borrow::Cow;

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

/// Remove a trailing `!important` flag from a declaration value string.
///
/// `@supports` conditions accept full `<declaration>` syntax, including optional `!important`.
/// When evaluating support we want to validate the underlying value grammar, so we strip the
/// important flag if present.
///
/// Notes:
/// - CSS treats `important` as an ASCII case-insensitive identifier.
/// - Whitespace between `!` and `important` is allowed (`! important`).
fn strip_trailing_important(value: &str) -> &str {
  let trimmed = trim_ascii_whitespace(value);
  if !trimmed.as_bytes().contains(&b'!') {
    return trimmed;
  }

  #[derive(Copy, Clone, Debug, PartialEq, Eq)]
  enum SignificantToken {
    Bang,
    ImportantIdent,
    Other,
  }

  fn skip_nested_block<'i, 't>(parser: &mut Parser<'i, 't>) {
    let _ = parser.parse_nested_block(|_| Ok::<_, cssparser::ParseError<'i, ()>>(()));
  }

  let mut input = ParserInput::new(trimmed);
  let mut parser = Parser::new(&mut input);
  let start = parser.position();
  let mut second_last: Option<(SignificantToken, cssparser::ParserState)> = None;
  let mut last: Option<(SignificantToken, cssparser::ParserState)> = None;

  loop {
    let state = parser.state();
    let token = match parser.next_including_whitespace_and_comments() {
      Ok(token) => token,
      Err(_) => break,
    };

    let (kind, needs_skip) = match token {
      Token::WhiteSpace(_) | Token::Comment(_) => (None, false),
      Token::Delim('!') => (Some(SignificantToken::Bang), false),
      Token::Ident(ref ident) if ident.eq_ignore_ascii_case("important") => {
        (Some(SignificantToken::ImportantIdent), false)
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => (Some(SignificantToken::Other), true),
      _ => (Some(SignificantToken::Other), false),
    };

    if needs_skip {
      skip_nested_block(&mut parser);
    }

    if let Some(kind) = kind {
      second_last = last;
      last = Some((kind, state));
    }
  }

  if matches!(last, Some((SignificantToken::ImportantIdent, _)))
    && matches!(second_last, Some((SignificantToken::Bang, _)))
  {
    if let Some((SignificantToken::Bang, bang_state)) = second_last {
      parser.reset(&bang_state);
      return trim_ascii_whitespace(parser.slice_from(start));
    }
  }

  trimmed
}

/// Validates a (property, value) pair for use in @supports queries.
///
/// Returns true when the property is recognized and either the value is a CSS-wide keyword,
/// contains an arbitrary substitution function (`var()`/`if()`/`attr()`), or parses according to
/// the engine's supported grammar.
pub fn supports_declaration(property: &str, value: &str) -> bool {
  let trimmed_property = trim_ascii_whitespace(property);
  if trimmed_property.is_empty() {
    return false;
  }

  // Custom properties always accept any value.
  if trimmed_property.starts_with("--") {
    return true;
  }

  let normalized_property: Cow<'_, str> = if trimmed_property
    .as_bytes()
    .iter()
    .any(|b| b.is_ascii_uppercase())
  {
    Cow::Owned(trimmed_property.to_ascii_lowercase())
  } else {
    Cow::Borrowed(trimmed_property)
  };
  let raw_value = trim_ascii_whitespace(value).trim_end_matches(';');
  let value_without_important = strip_trailing_important(raw_value);

  // Tailwind v4 gates its `@layer properties` reset behind vendor-prefixed probes:
  // `(-webkit-hyphens:none)` and `(-moz-orient:inline)`. These should evaluate true so the global
  // `--tw-*` defaults are retained and participate in the cascade.
  //
  // Important: do not treat arbitrary vendor-prefixed properties as supported, since they are
  // frequently used inside `not(...)` and flipping them to true can invert unrelated feature tests.
  let normalized_property = match normalized_property.as_ref() {
    "-webkit-hyphens" => "hyphens",
    "-moz-orient" => {
      if value_without_important.eq_ignore_ascii_case("inline") {
        return true;
      }
      return false;
    }
    other => other,
  };

  let canonical_property = if is_known_style_property(normalized_property) {
    normalized_property
  } else if let Some(alias) = legacy_logical_offset_inset_property_alias(normalized_property) {
    alias
  } else if normalized_property.starts_with("-webkit-")
    || normalized_property.starts_with("-moz-")
    || normalized_property.starts_with("-ms-")
    || normalized_property.starts_with("-o-")
    || normalized_property.starts_with("-khtml-")
  {
    match vendor_prefixed_property_alias(normalized_property) {
      Some(alias) => alias,
      None => return false,
    }
  } else {
    return false;
  };

  if is_global_keyword_str(value_without_important) {
    return true;
  }

  // `@supports not (display: -ms-grid)` is a common pattern used to distinguish legacy IE/EdgeHTML
  // (which supported `-ms-grid`) from modern browsers. FastRender parses `display: -ms-grid` as an
  // alias for `grid` for compatibility with autoprefixed stylesheets, but in a support query we
  // want to behave like modern Chromium (Chrome baselines) so the `not (...)` branch matches.
  if canonical_property == "display"
    && (value_without_important.eq_ignore_ascii_case("-ms-grid")
      || value_without_important.eq_ignore_ascii_case("-ms-inline-grid"))
  {
    return false;
  }

  if contains_arbitrary_substitution_function(value_without_important) {
    return true;
  }

  let parsed = match parse_property_value(canonical_property, value_without_important) {
    Some(v) => v,
    None => return false,
  };

  supports_parsed_declaration_is_valid(canonical_property, value_without_important, &parsed)
}

/// Validates an at-rule for use in `@supports at-rule(...)` queries.
///
/// Returns `true` for at-rules that FastRender parses/uses and `false` otherwise.
pub fn supports_at_rule(rule: &str) -> bool {
  let trimmed = trim_ascii_whitespace(rule);
  if trimmed.is_empty() {
    return false;
  }

  let name = trimmed.strip_prefix('@').unwrap_or(trimmed);
  match name.to_ascii_lowercase().as_str() {
    "layer"
    | "container"
    | "supports"
    | "media"
    | "scope"
    | "starting-style"
    | "property"
    | "font-face"
    | "keyframes"
    | "font-palette-values"
    | "font-feature-values"
    | "counter-style"
    | "page"
    | "position-try" => true,
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::parser::{parse_stylesheet, parse_stylesheet_with_media};
  use crate::css::types::CssRule;
  use crate::dom;
  use crate::style::cascade::{apply_styles_with_media, StyledNode};
  use crate::style::media::{MediaContext, MediaQueryCache};

  fn find_first<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
    if let Some(name) = node.node.tag_name() {
      if name.eq_ignore_ascii_case(tag) {
        return Some(node);
      }
    }
    node
      .children
      .iter()
      .find_map(|child| find_first(child, tag))
  }

  fn render_div_display(css: &str) -> String {
    let dom = dom::parse_html(r#"<div></div>"#).unwrap();
    let stylesheet = parse_stylesheet(css).unwrap();
    let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
    let div = find_first(&styled, "div").expect("div");
    div.styles.display.to_string()
  }

  #[test]
  fn strip_trailing_important_strips_whitespace_comment_variants() {
    assert_eq!(strip_trailing_important("10px !important"), "10px");
    assert_eq!(strip_trailing_important("10px!important"), "10px");
    assert_eq!(strip_trailing_important("10px ! important"), "10px");
    assert_eq!(strip_trailing_important("10px !/*comment*/important"), "10px");
    assert_eq!(
      strip_trailing_important("10px /*keep*/ !/*comment*/important /*trailing*/"),
      "10px /*keep*/"
    );
  }

  #[test]
  fn strip_trailing_important_ignores_nested_blocks() {
    // `!important` inside a nested block should not be treated as the declaration's important flag.
    assert_eq!(
      strip_trailing_important("var(--x, !important)"),
      "var(--x, !important)"
    );

    // But `!important` after nested blocks should still be stripped.
    assert_eq!(
      strip_trailing_important("var(--x, !important) !important"),
      "var(--x, !important)"
    );
  }

  #[test]
  fn strip_trailing_important_keeps_non_trailing_tokens() {
    assert_eq!(
      strip_trailing_important("10px !important 0"),
      "10px !important 0"
    );
    assert_eq!(
      strip_trailing_important("10px !importantish"),
      "10px !importantish"
    );
  }

  #[test]
  fn supports_legacy_box_display_values() {
    assert!(supports_declaration("display", "-webkit-box"));
    assert!(supports_declaration("display", "-webkit-inline-box"));
    assert!(supports_declaration("display", "-moz-box"));
    assert!(supports_declaration("display", "-moz-inline-box"));
  }

  #[test]
  fn supports_vendor_prefixed_flex_display_values() {
    assert!(supports_declaration("display", "-webkit-flex"));
    assert!(supports_declaration("display", "-webkit-inline-flex"));
    // Modern Chromium treats legacy IE/EdgeHTML display keywords as unsupported (even though our
    // parser aliases them for autoprefixed stylesheets). Match that behavior so feature queries like
    // `@supports not (display: -ms-flexbox)` behave consistently with browser baselines.
    assert!(!supports_declaration("display", "-ms-flexbox"));
    assert!(!supports_declaration("display", "-ms-inline-flexbox"));
  }

  #[test]
  fn supports_vendor_prefixed_grid_display_values() {
    assert!(!supports_declaration("display", "-ms-grid"));
    assert!(!supports_declaration("display", "-ms-inline-grid"));
  }

  #[test]
  fn supports_legacy_offset_logical_inset_alias_properties() {
    assert!(supports_declaration("offset-inline-start", "0"));
    assert!(supports_declaration("offset-inline-end", "0"));
    assert!(supports_declaration("offset-block-start", "0"));
    assert!(supports_declaration("offset-block-end", "0"));

    assert!(
      !supports_declaration("offset-inline-start", "bogus"),
      "invalid values should make the support query false"
    );
  }

  #[test]
  fn supports_declaration_accepts_targeted_vendor_properties() {
    assert!(supports_declaration("-webkit-hyphens", "none"));
    assert!(supports_declaration("-WEBKIT-HYPHENS", "none"));
    assert!(supports_declaration("-moz-orient", "inline"));
    assert!(supports_declaration("-MoZ-OrIeNt", "inline"));
    assert!(supports_declaration("-webkit-appearance", "none"));
    assert!(supports_declaration("-moz-appearance", "none"));
    assert!(supports_declaration("-ms-appearance", "none"));
    assert!(supports_declaration("-o-appearance", "none"));
    assert!(supports_declaration(
      "-webkit-column-break-before",
      "always"
    ));
    assert!(supports_declaration("-webkit-column-break-inside", "avoid"));
    assert!(supports_declaration("-webkit-page-break-before", "always"));
    assert!(supports_declaration("-webkit-page-break-inside", "avoid"));
    assert!(supports_declaration("page-break-before", "always"));
    assert!(!supports_declaration("-webkit-not-a-real-prop", "none"));
    assert!(!supports_declaration("-moz-not-a-real-prop", "none"));
    assert!(!supports_declaration("-ms-not-a-real-prop", "none"));
    assert!(!supports_declaration("-o-not-a-real-prop", "none"));
    assert!(supports_declaration("-ms-grid-row", "1"));
    assert!(
      !supports_declaration("-ms-grid-row", "0"),
      "legacy -ms-grid-row requires a positive integer value"
    );
    assert!(
      !supports_declaration("-ms-filter", "none"),
      "legacy IE -ms-filter must not alias to modern filter() syntax"
    );
    assert!(
      !supports_declaration("page-break-before", "column"),
      "legacy page-break properties should reject modern break keywords"
    );
    assert!(
      !supports_declaration("-webkit-column-break-before", "page"),
      "legacy column-break properties should reject page-only break keywords"
    );
    assert!(
      !supports_declaration("-webkit-column-break-inside", "avoid-column"),
      "legacy column-break-inside should reject modern break-inside keywords"
    );
    assert!(
      !supports_declaration("-webkit-page-break-inside", "avoid-page"),
      "legacy page-break-inside should reject modern break-inside keywords"
    );
  }

  #[test]
  fn supports_vendor_properties_prevent_pruning_tailwind_reset_blocks() {
    let css = r#"
      @supports (-webkit-hyphens:none) or (-moz-orient:inline) {
        @layer properties {
          :root { --tw-test: 1; }
        }
      }
    "#;

    let media_ctx = MediaContext::screen(800.0, 600.0);
    let mut cache = MediaQueryCache::default();
    let sheet =
      parse_stylesheet_with_media(css, &media_ctx, Some(&mut cache)).expect("parse stylesheet");

    let supports_rule = sheet
      .rules
      .iter()
      .find_map(|rule| match rule {
        CssRule::Supports(rule) => Some(rule),
        _ => None,
      })
      .expect("@supports block should not be pruned");

    let layer_rule = supports_rule
      .rules
      .iter()
      .find_map(|rule| match rule {
        CssRule::Layer(rule) => Some(rule),
        _ => None,
      })
      .expect("@layer rule should survive inside @supports block");

    assert!(
      layer_rule
        .rules
        .iter()
        .any(|rule| matches!(rule, CssRule::Style(_))),
      "expected style rule inside @layer block"
    );
  }

  #[test]
  fn supports_not_vendor_properties_do_not_invert_feature_queries() {
    let css = r#"
      @supports (-webkit-appearance: none)
        and (not (-moz-not-a-real-prop: none))
        and (not (-ms-filter: none))
        and (text-size-adjust: none) {
        .a { color: red; }
      }
    "#;

    let media_ctx = MediaContext::screen(800.0, 600.0);
    let mut cache = MediaQueryCache::default();
    let sheet =
      parse_stylesheet_with_media(css, &media_ctx, Some(&mut cache)).expect("parse stylesheet");

    let supports_rule = sheet
      .rules
      .iter()
      .find_map(|rule| match rule {
        CssRule::Supports(rule) => Some(rule),
        _ => None,
      })
      .expect("@supports block should not be pruned");

    assert!(supports_rule.condition.matches());
    assert!(
      supports_rule
        .rules
        .iter()
        .any(|rule| matches!(rule, CssRule::Style(_))),
      "expected style rule inside @supports block"
    );
  }

  #[test]
  fn supports_webkit_box_orient_keywords_only() {
    assert!(supports_declaration("-webkit-box-orient", "horizontal"));
    assert!(supports_declaration("-webkit-box-orient", "vertical"));
    assert!(!supports_declaration("-webkit-box-orient", "diagonal"));

    assert!(supports_declaration("box-orient", "horizontal"));
    assert!(supports_declaration("box-orient", "vertical"));
    assert!(!supports_declaration("box-orient", "diagonal"));
  }

  #[test]
  fn supports_legacy_webkit_box_alignment_and_order_properties() {
    assert!(supports_declaration("-webkit-box-pack", "center"));
    assert!(supports_declaration("box-pack", "justify"));
    assert!(supports_declaration("box-pack", "space-evenly"));
    assert!(supports_declaration("box-pack", "normal"));
    assert!(supports_declaration("box-pack", "stretch"));

    assert!(supports_declaration("-webkit-box-align", "baseline"));
    assert!(supports_declaration("box-align", "stretch"));
    assert!(supports_declaration("-webkit-box-align", "top"));
    assert!(supports_declaration("box-align", "flex-end"));
    assert!(supports_declaration("box-align", "normal"));
    assert!(!supports_declaration("box-align", "auto"));

    assert!(supports_declaration("-webkit-box-direction", "reverse"));
    assert!(supports_declaration("box-direction", "normal"));
    assert!(!supports_declaration("box-direction", "sideways"));

    assert!(supports_declaration("-webkit-box-lines", "multiple"));
    assert!(supports_declaration("box-lines", "single"));
    assert!(!supports_declaration("box-lines", "wrap"));

    assert!(supports_declaration("-webkit-box-flex", "1"));
    assert!(supports_declaration("box-flex", "calc(1 + 1)"));
    assert!(!supports_declaration("box-flex", "-1"));

    assert!(supports_declaration("-webkit-box-ordinal-group", "1"));
    assert!(supports_declaration("box-ordinal-group", "2"));
    assert!(!supports_declaration("box-ordinal-group", "0"));
  }

  #[test]
  fn supports_word_break_auto_phrase_is_true() {
    assert!(supports_declaration("word-break", "auto-phrase"));
    assert!(supports_declaration("word-break", "break-word"));
  }

  #[test]
  fn supports_text_wrap_keywords_and_shorthand_pairs() {
    assert!(supports_declaration("text-wrap", "wrap"));
    assert!(supports_declaration("text-wrap", "nowrap"));
    assert!(supports_declaration("text-wrap", "auto"));
    assert!(supports_declaration("text-wrap", "balance"));
    assert!(supports_declaration("text-wrap", "pretty"));
    assert!(supports_declaration("text-wrap", "stable"));
    assert!(supports_declaration("text-wrap", "avoid-orphans"));

    assert!(supports_declaration("text-wrap", "wrap balance"));
    assert!(supports_declaration("text-wrap", "balance wrap"));
    assert!(supports_declaration("text-wrap", "nowrap balance"));

    assert!(!supports_declaration("text-wrap", "wrap nowrap"));
    assert!(!supports_declaration("text-wrap", "balance pretty"));
    assert!(!supports_declaration("text-wrap", "bogus"));
  }

  #[test]
  fn supports_opacity_percentage_values() {
    assert!(supports_declaration("opacity", "0%"));
    assert!(supports_declaration("opacity", "50%"));
    assert!(supports_declaration("opacity", "100%"));
    assert!(supports_declaration("opacity", "150%"));
    assert!(supports_declaration("opacity", "-10%"));

    assert!(!supports_declaration("opacity", "50px"));
  }

  #[test]
  fn supports_khtml_opacity_alias() {
    assert!(supports_declaration("-khtml-opacity", "0.5"));
    assert!(!supports_declaration("-khtml-opacity", "50px"));
  }

  #[test]
  fn supports_animation_duration_accepts_auto_and_rejects_invalid_values() {
    assert!(supports_declaration("animation-duration", "auto"));
    assert!(supports_declaration("animation-duration", "1s"));
    assert!(supports_declaration("animation-duration", "auto, 2s"));
    assert!(supports_declaration(
      "animation-duration",
      "calc(1s + 500ms)"
    ));

    assert!(supports_declaration("-webkit-animation-duration", "auto"));
    assert!(supports_declaration("-webkit-animation-duration", "1s"));
    assert!(supports_declaration(
      "-webkit-animation-duration",
      "auto, 2s"
    ));
    assert!(supports_declaration(
      "-webkit-animation-duration",
      "calc(1s + 500ms)"
    ));

    assert!(!supports_declaration("animation-duration", "bogus"));
    assert!(!supports_declaration("animation-duration", "auto 2s"));
    assert!(!supports_declaration("animation-duration", "1s,"));

    assert!(!supports_declaration("-webkit-animation-duration", "bogus"));
    assert!(!supports_declaration(
      "-webkit-animation-duration",
      "auto 2s"
    ));
    assert!(!supports_declaration("-webkit-animation-duration", "1s,"));
  }

  #[test]
  fn supports_sizing_properties_require_valid_length_syntax() {
    assert!(supports_declaration("height", "100px"));
    assert!(supports_declaration("height", "0"));
    assert!(supports_declaration("height", "calc(100svh - 10px)"));
    assert!(supports_declaration("height", "min-content"));
    assert!(supports_declaration("height", "fit-content(10px)"));
    assert!(supports_declaration("height", "stretch"));
    assert!(supports_declaration("height", "fill-available"));
    assert!(supports_declaration("height", "-webkit-fill-available"));
    assert!(supports_declaration("height", "-moz-available"));

    // Non-zero unitless numbers are invalid for sizing properties.
    assert!(!supports_declaration("height", "10"));

    // Viewport-relative units from CSS Values and Units Level 4 should be recognized.
    assert!(supports_declaration("height", "100vi"));
    assert!(supports_declaration("height", "100vb"));
    assert!(supports_declaration("height", "100dvi"));
    assert!(supports_declaration("height", "100dvb"));

    // Unsupported/unknown units should make the query false.
    assert!(!supports_declaration("height", "100bogusunit"));

    // `none` is only valid for max-* properties.
    assert!(supports_declaration("max-width", "none"));
    assert!(!supports_declaration("width", "none"));
  }

  #[test]
  fn supports_container_type_keywords_only() {
    assert!(supports_declaration("container-type", "inline-size"));
    assert!(supports_declaration("container-type", "scroll-state"));
    assert!(supports_declaration("container-type", "size scroll-state"));
    assert!(supports_declaration("container-type", "scroll-state size"));
    assert!(supports_declaration(
      "container-type",
      "inline-size scroll-state"
    ));
    assert!(supports_declaration(
      "container-type",
      "scroll-state inline-size"
    ));

    assert!(!supports_declaration("container-type", "size size"));
    assert!(!supports_declaration(
      "container-type",
      "scroll-state scroll-state"
    ));
    assert!(!supports_declaration("container-type", "size inline-size"));
    assert!(!supports_declaration("container-type", "normal size"));

    // Ensure comment/escape tokenization matches the computed-style parser.
    assert!(supports_declaration("container-type", "size/*comment*/"));
    assert!(supports_declaration("container-type", "s\\69ze"));
  }

  #[test]
  fn supports_overflow_overlay_legacy_alias() {
    assert!(supports_declaration("overflow", "overlay"));
    assert!(supports_declaration("overflow-x", "overlay"));
    assert!(supports_declaration("overflow-y", "overlay"));
  }

  #[test]
  fn supports_alignment_auto_keywords_only_where_computed_style_accepts_them() {
    // `auto` is valid for self-alignment, and FastRender also accepts it for `justify-items` as a
    // compatibility alias (mapped to `stretch` during computed style resolution).
    assert!(supports_declaration("align-self", "auto"));
    assert!(supports_declaration("justify-self", "auto"));
    assert!(supports_declaration("justify-items", "auto"));

    // `auto` is not valid for the container-alignment properties we currently parse.
    assert!(!supports_declaration("align-items", "auto"));

    // Avoid claiming support for unsupported keywords in @supports queries.
    assert!(!supports_declaration("justify-content", "baseline"));
  }

  #[test]
  fn supports_align_content_legacy_left_right_keywords() {
    assert!(supports_declaration("align-content", "left"));
    assert!(supports_declaration("align-content", "right"));
    assert!(supports_declaration("-webkit-align-content", "left"));
    assert!(supports_declaration("-webkit-align-content", "right"));
  }

  #[test]
  fn supports_env_and_constant_in_calc_lengths() {
    assert!(supports_declaration(
      "padding-left",
      "calc(10px + env(safe-area-inset-left))"
    ));
    assert!(supports_declaration(
      "padding-left",
      "calc(22px + constant(safe-area-inset-left))"
    ));
  }

  #[test]
  fn supports_calc_max_single_argument() {
    assert!(supports_declaration("border-left-width", "calc(max(0px))"));
    assert!(supports_declaration("left", "max(calc(0px))"));
  }

  #[test]
  fn supports_font_size_accepts_container_query_units() {
    // The `nbcnews.com` pageset fixture uses `@supports(font-size:1cqh)` to switch a font-size
    // custom property to a container-query driven clamp() expression. FastRender resolves
    // container-query units for `font-size` during the container pass, so the query should
    // evaluate true.
    assert!(supports_declaration("font-size", "1cqh"));
    assert!(supports_declaration("font-size", "2cqw"));

    // Supported font-size syntaxes should remain true.
    assert!(supports_declaration("font-size", "0"));
    assert!(supports_declaration("font-size", "16px"));
    assert!(supports_declaration("font-size", "1rem"));
    assert!(supports_declaration("font-size", "2vw"));
    assert!(supports_declaration("font-size", "smaller"));
  }

  #[test]
  fn supports_light_dark_color_function() {
    assert!(supports_declaration("color", "light-dark(red,red)"));
  }

  #[test]
  fn supports_transition_behavior_accepts_known_keywords_and_rejects_invalid() {
    assert!(supports_declaration("transition-behavior", "normal"));
    assert!(supports_declaration(
      "transition-behavior",
      "allow-discrete"
    ));
    assert!(supports_declaration(
      "transition-behavior",
      "normal, allow-discrete"
    ));

    assert!(!supports_declaration("transition-behavior", "bogus"));
    assert!(!supports_declaration(
      "transition-behavior",
      "normal allow-discrete"
    ));
    assert!(!supports_declaration("transition-behavior", "normal,"));
  }

  #[test]
  fn supports_timeline_scope_accepts_keywords_and_dashed_ident_list() {
    assert!(supports_declaration("timeline-scope", "none"));
    assert!(supports_declaration("timeline-scope", "all"));
    assert!(supports_declaration("timeline-scope", "--scroller"));
    assert!(supports_declaration("timeline-scope", "--a, --b"));

    assert!(!supports_declaration("timeline-scope", "bogus"));
    assert!(!supports_declaration("timeline-scope", "foo, --bar"));
    assert!(!supports_declaration("timeline-scope", "--foo bar"));
  }

  #[test]
  fn supports_timeline_scope_dashed_ident() {
    let css = r"
      div { display: block; }
      @supports (timeline-scope: --scroller) { div { display: inline; } }
    ";
    assert_eq!(render_div_display(css), "inline");
  }

  #[test]
  fn supports_timeline_scope_all_keyword() {
    let css = r"
      div { display: block; }
      @supports (timeline-scope: all) { div { display: inline; } }
    ";
    assert_eq!(render_div_display(css), "inline");
  }

  #[test]
  fn supports_timeline_scope_rejects_trailing_comma() {
    let css = r"
      div { display: block; }
      @supports (timeline-scope: --scroller,) { div { display: inline; } }
    ";
    assert_eq!(render_div_display(css), "block");
  }

  #[test]
  fn supports_timeline_scope_rejects_non_dashed_ident() {
    let css = r"
      div { display: block; }
      @supports (timeline-scope: scroller) { div { display: inline; } }
    ";
    assert_eq!(render_div_display(css), "block");
  }

  #[test]
  fn supports_declaration_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    assert!(
      supports_declaration("display", "block"),
      "baseline sanity check failed"
    );
    assert!(
      !supports_declaration("display", &format!("{nbsp}block")),
      "NBSP must not be treated as whitespace in @supports values"
    );
    assert!(
      !supports_declaration(&format!("{nbsp}display"), "block"),
      "NBSP must not be treated as whitespace in @supports property names"
    );
  }

  #[test]
  fn supports_declaration_strips_important_case_insensitive_and_whitespace() {
    assert!(supports_declaration("color", "red !IMPORTANT"));
    assert!(supports_declaration("color", "red ! important"));
    assert!(supports_declaration("color", "red!important"));

    // `!important` must be at the end of the value to be treated as the important flag.
    assert!(!supports_declaration("color", "red !important bogus"));
  }

  #[test]
  fn supports_declaration_strips_important_with_comments() {
    assert!(supports_declaration("color", "red!/**/important"));
    assert!(supports_declaration("color", "red!important/**/"));
    assert!(supports_declaration("color", "red !/**/important"));

    // `!important` must still be the final token (ignoring trailing whitespace/comments).
    assert!(!supports_declaration("color", "red!important/**/ bogus"));
  }

  #[test]
  fn supports_declaration_does_not_panic_on_non_ascii_values_when_stripping_important() {
    // `css_coverage` feeds arbitrary declaration values through `supports_declaration`. Ensure
    // stripping `!important` does not assume the value is ASCII.
    assert!(!supports_declaration("not-a-real-property", "\"“\" \"”\""));
    assert!(supports_declaration("font-family", "\"“\" !important"));
  }
}
