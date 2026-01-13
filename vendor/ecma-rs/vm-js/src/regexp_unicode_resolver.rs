use crate::regexp::RegExpSyntaxError;
use crate::regexp_unicode_property_strings::UnicodeStringProperty;
use crate::regexp_unicode_tables::{
  resolve_property_name, resolve_property_value, NonBinaryProp, NonBinaryValue,
  ResolvedCodePointProperty, StringProp, UnicodePropertyName,
};

/// Resolved `\p{...}` / `\P{...}` Unicode property query.
///
/// This is the glue between:
/// - The raw `UnicodePropertyValueExpression` source text (no normalization), and
/// - The internal property tables used by the RegExp engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedUnicodeProperty {
  CodePoint(ResolvedCodePointProperty),
  String(UnicodeStringProperty),
}

#[inline]
fn syntax_error() -> RegExpSyntaxError {
  RegExpSyntaxError {
    message: "Invalid regular expression",
  }
}

/// Resolve the raw `UnicodePropertyValueExpression` source text (the text inside `\p{...}` /
/// `\P{...}`) into an internal property query.
///
/// This implements the ECMA-262 `UnicodePropertyValueExpression` handling, including the
/// `General_Category` lone-value precedence rule (`\p{Lu}` => `General_Category=Lu`).
///
/// `expr` is treated as a raw string: it is not trimmed, case-folded, or normalized.
pub(crate) fn resolve_unicode_property_value_expression(
  expr: &str,
  unicode_sets: bool,
) -> Result<ResolvedUnicodeProperty, RegExpSyntaxError> {
  // `UnicodePropertyValueExpression` is either:
  //   UnicodePropertyName = UnicodePropertyValue
  // or a lone name/value.
  if let Some(eq_pos) = expr.find('=') {
    // The parser should ensure there is at most one `=`, but be strict anyway.
    if expr[eq_pos.saturating_add(1)..].contains('=') {
      return Err(syntax_error());
    }

    let (name, value_with_eq) = expr.split_at(eq_pos);
    let value = &value_with_eq[1..]; // skip '='

    if name.is_empty() || value.is_empty() {
      return Err(syntax_error());
    }

    // `name` must resolve to one of the non-binary properties (`gc|sc|scx`).
    let Some(prop_name) = resolve_property_name(name, unicode_sets) else {
      return Err(syntax_error());
    };
    let UnicodePropertyName::NonBinary(prop) = prop_name else {
      // Binary properties (and string properties) are invalid in `name=value` form.
      return Err(syntax_error());
    };

    let Some(value) = resolve_property_value(prop, value) else {
      return Err(syntax_error());
    };

    let resolved = match (prop, value) {
      (NonBinaryProp::General_Category, NonBinaryValue::GeneralCategory(gc)) => {
        ResolvedCodePointProperty::GeneralCategory(gc)
      }
      (NonBinaryProp::Script, NonBinaryValue::Script(sc)) => ResolvedCodePointProperty::Script(sc),
      (NonBinaryProp::Script_Extensions, NonBinaryValue::Script(sc)) => {
        ResolvedCodePointProperty::ScriptExtensions(sc)
      }
      _ => return Err(syntax_error()),
    };

    Ok(ResolvedUnicodeProperty::CodePoint(resolved))
  } else {
    // Lone values have `General_Category` value precedence.
    if let Some(NonBinaryValue::GeneralCategory(gc)) =
      resolve_property_value(NonBinaryProp::General_Category, expr)
    {
      return Ok(ResolvedUnicodeProperty::CodePoint(
        ResolvedCodePointProperty::GeneralCategory(gc),
      ));
    }

    let Some(name) = resolve_property_name(expr, unicode_sets) else {
      return Err(syntax_error());
    };

    match name {
      UnicodePropertyName::Binary(bin) => Ok(ResolvedUnicodeProperty::CodePoint(
        ResolvedCodePointProperty::Binary(bin),
      )),
      // Non-binary property names require an explicit value (`Script=Latin` etc).
      UnicodePropertyName::NonBinary(_) => Err(syntax_error()),
      UnicodePropertyName::String(prop) => Ok(ResolvedUnicodeProperty::String(map_string_prop(prop))),
    }
  }
}

#[inline]
fn map_string_prop(prop: StringProp) -> UnicodeStringProperty {
  match prop {
    StringProp::Basic_Emoji => UnicodeStringProperty::BasicEmoji,
    StringProp::Emoji_Keycap_Sequence => UnicodeStringProperty::EmojiKeycapSequence,
    StringProp::RGI_Emoji_Flag_Sequence => UnicodeStringProperty::RgiEmojiFlagSequence,
    StringProp::RGI_Emoji_Modifier_Sequence => UnicodeStringProperty::RgiEmojiModifierSequence,
    StringProp::RGI_Emoji_Tag_Sequence => UnicodeStringProperty::RgiEmojiTagSequence,
    StringProp::RGI_Emoji_ZWJ_Sequence => UnicodeStringProperty::RgiEmojiZwjSequence,
    StringProp::RGI_Emoji => UnicodeStringProperty::RgiEmoji,
  }
}
