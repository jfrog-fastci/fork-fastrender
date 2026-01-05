pub fn entry_anchor_id(name: &str) -> String {
  let mut hash: u64 = 14695981039346656037;
  for byte in name.as_bytes() {
    hash ^= u64::from(*byte);
    hash = hash.wrapping_mul(1099511628211);
  }
  format!("entry-{hash:016x}")
}
