use super::properties::{
  is_global_keyword_str, is_known_style_property, parse_property_value,
  supports_parsed_declaration_is_valid, vendor_prefixed_property_alias,
};
use crate::style::var_resolution::contains_var;
use std::borrow::Cow;

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

/// Validates a (property, value) pair for use in @supports queries.
///
/// Returns true when the property is recognized and either the value is a CSS-wide keyword,
/// contains a var() reference, or parses according to the engine's supported grammar.
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
  let value_without_important = trim_ascii_whitespace(raw_value.trim_end_matches("!important"));

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
  } else if normalized_property.starts_with("-webkit-") {
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

  if contains_var(value_without_important) {
    return true;
  }

  let parsed = match parse_property_value(canonical_property, value_without_important) {
    Some(v) => v,
    None => return false,
  };

  supports_parsed_declaration_is_valid(canonical_property, value_without_important, &parsed)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn supports_legacy_webkit_box_display_values() {
    assert!(supports_declaration("display", "-webkit-box"));
    assert!(supports_declaration("display", "-webkit-inline-box"));
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
  fn supports_word_break_auto_phrase_is_false() {
    // GitLab ships `@supports (word-break:auto-phrase)` gates, but FastRender does not implement
    // the `auto-phrase` value yet. Returning `true` here would incorrectly enable styles that
    // expect native phrase-based line breaking behavior.
    assert!(!supports_declaration("word-break", "auto-phrase"));
    assert!(supports_declaration("word-break", "break-word"));
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

    // Unsupported/unknown units should make the query false.
    assert!(!supports_declaration("height", "100dvb"));
    assert!(!supports_declaration("height", "100bogusunit"));

    // `none` is only valid for max-* properties.
    assert!(supports_declaration("max-width", "none"));
    assert!(!supports_declaration("width", "none"));
  }

  #[test]
  fn supports_container_type_keywords_only() {
    assert!(supports_declaration("container-type", "inline-size"));
    assert!(!supports_declaration("container-type", "scroll-state"));
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
    assert!(supports_declaration("transition-behavior", "allow-discrete"));
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
}
