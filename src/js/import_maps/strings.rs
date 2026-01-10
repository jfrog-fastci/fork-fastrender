use std::cmp::Ordering;

pub(crate) fn cmp_code_units(a: &str, b: &str) -> Ordering {
  let mut a_it = a.encode_utf16();
  let mut b_it = b.encode_utf16();
  loop {
    match (a_it.next(), b_it.next()) {
      (None, None) => return Ordering::Equal,
      (None, Some(_)) => return Ordering::Less,
      (Some(_), None) => return Ordering::Greater,
      (Some(x), Some(y)) => match x.cmp(&y) {
        Ordering::Equal => {}
        other => return other,
      },
    }
  }
}
