use crate::webidl::ExtendedAttribute;
use anyhow::{bail, Context, Result};
use webidl_ir::DictionaryMemberSchema;

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDictionaryMember {
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub schema: DictionaryMemberSchema,
}

pub fn parse_dictionary_member(input: &str) -> Result<ParsedDictionaryMember> {
  let mut s = input.trim();
  s = s.strip_suffix(';').unwrap_or(s).trim();

  let (ext_attrs, rest) = super::parse_leading_ext_attrs(s);
  let mut rest = super::strip_leading_ws_and_comments(rest).trim_start();

  let mut required = false;
  if let Some(after) = super::consume_keyword(rest, "required") {
    required = true;
    rest = super::strip_leading_ws_and_comments(after).trim_start();
  }

  let (mut ty, rest_after_type) = webidl_ir::parse_idl_type(rest)
    .map_err(|e| anyhow::Error::new(e))
    .context("parse dictionary member type")?;

  ty = super::type_resolution::merge_extra_annotations(ty, &ext_attrs);

  let (name, rest_after_name) = super::parse_identifier_prefix(rest_after_type)
    .ok_or_else(|| anyhow::anyhow!("expected dictionary member name after type"))?;
  let mut rest = super::strip_leading_ws_and_comments(rest_after_name).trim_start();

  let default: Option<webidl_ir::DefaultValue> = if rest.starts_with('=') {
    rest = &rest[1..];
    rest = super::strip_leading_ws_and_comments(rest).trim_start();
    let default_src = rest.trim();
    if default_src.is_empty() {
      bail!("expected default value after `=`");
    }
    Some(
      webidl_ir::parse_default_value(default_src)
        .map_err(|e| anyhow::Error::new(e))
        .with_context(|| format!("parse dictionary member default value `{default_src}`"))?,
    )
  } else if rest.trim().is_empty() {
    None
  } else {
    bail!("unexpected trailing input in dictionary member: `{}`", rest.trim());
  };

  Ok(ParsedDictionaryMember {
    ext_attrs,
    schema: DictionaryMemberSchema {
      name: name.to_string(),
      required,
      ty,
      default,
    },
  })
}
