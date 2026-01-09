use std::borrow::Cow;

fn is_http_scheme_prefix(input: &str) -> Option<usize> {
  if input
    .get(..7)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
  {
    Some(7)
  } else if input
    .get(..8)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
  {
    Some(8)
  } else {
    None
  }
}

fn authority_end(input: &str, offset: usize) -> usize {
  input[offset..]
    .find(&['/', '?', '#'][..])
    .map(|pos| offset + pos)
    .unwrap_or_else(|| input.len())
}

fn tolerant_encode_path_query_fragment(input: &str, start: usize) -> Cow<'_, str> {
  if !input.as_bytes()[start..]
    .iter()
    .any(|b| matches!(*b, b'|' | b' '))
  {
    return Cow::Borrowed(input);
  }

  let mut out = String::with_capacity(input.len());
  out.push_str(&input[..start]);
  let mut last = start;

  for (rel_idx, ch) in input[start..].char_indices() {
    let abs_idx = start + rel_idx;
    let replacement = match ch {
      '|' => Some("%7C"),
      ' ' => Some("%20"),
      _ => None,
    };
    if let Some(replacement) = replacement {
      out.push_str(&input[last..abs_idx]);
      out.push_str(replacement);
      last = abs_idx + ch.len_utf8();
    }
  }

  out.push_str(&input[last..]);
  Cow::Owned(out)
}

pub(crate) fn normalize_http_url_for_resolution(input: &str) -> Cow<'_, str> {
  let Some(prefix_len) = is_http_scheme_prefix(input) else {
    return Cow::Borrowed(input);
  };
  let start = authority_end(input, prefix_len);
  tolerant_encode_path_query_fragment(input, start)
}

pub(crate) fn normalize_url_reference_for_resolution(reference: &str) -> Cow<'_, str> {
  let start = if reference.starts_with("//") {
    authority_end(reference, 2)
  } else if let Some(prefix_len) = is_http_scheme_prefix(reference) {
    authority_end(reference, prefix_len)
  } else {
    0
  };
  tolerant_encode_path_query_fragment(reference, start)
}

