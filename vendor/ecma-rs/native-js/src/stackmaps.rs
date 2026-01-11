/// Very small parser for LLVM's `.llvm_stackmaps` section.
///
/// We only need enough structure to count records in tests. The on-disk format
/// starts with four little-endian `u32` values:
///
/// ```text
/// u32 Version
/// u32 NumFunctions
/// u32 NumConstants
/// u32 NumRecords
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackMapsHeader {
  pub version: u32,
  pub num_functions: u32,
  pub num_constants: u32,
  pub num_records: u32,
}

impl StackMapsHeader {
  pub fn parse(data: &[u8]) -> Option<Self> {
    if data.len() < 16 {
      return None;
    }
    let u32_at = |off: usize| -> u32 {
      u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
    };
    Some(Self {
      version: u32_at(0),
      num_functions: u32_at(4),
      num_constants: u32_at(8),
      num_records: u32_at(12),
    })
  }
}
