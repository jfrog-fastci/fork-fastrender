use std::fmt;
use std::ops::Range;
use std::sync::Arc;

/// Media sample payload bytes.
///
/// For container formats like MP4, demuxing can often avoid per-sample copies by returning a
/// `Shared` slice into an `Arc<[u8]>` holding the full file.
#[derive(Clone)]
pub enum MediaData {
  /// Packet owns its bytes (e.g. data read into a fresh `Vec<u8>`).
  Owned(Vec<u8>),
  /// Packet references a sub-range of a shared byte buffer.
  Shared {
    bytes: Arc<[u8]>,
    range: Range<usize>,
  },
}

impl MediaData {
  /// Returns the packet bytes as a slice.
  #[inline]
  pub fn as_slice(&self) -> &[u8] {
    match self {
      Self::Owned(data) => data.as_slice(),
      Self::Shared { bytes, range } => &bytes[range.start..range.end],
    }
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.as_slice().len()
  }

  #[inline]
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

impl From<Vec<u8>> for MediaData {
  fn from(value: Vec<u8>) -> Self {
    Self::Owned(value)
  }
}

impl fmt::Debug for MediaData {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Owned(data) => f
        .debug_struct("MediaData::Owned")
        .field("len", &data.len())
        .finish(),
      Self::Shared { bytes, range } => f
        .debug_struct("MediaData::Shared")
        .field("bytes_len", &bytes.len())
        .field("range", range)
        .field("len", &range.len())
        .finish(),
    }
  }
}

impl PartialEq for MediaData {
  fn eq(&self, other: &Self) -> bool {
    self.as_slice() == other.as_slice()
  }
}

impl Eq for MediaData {}

/// A demuxed elementary stream packet (audio sample or video access unit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPacket {
  /// Container-native track identifier (MP4 track id, Matroska track number, etc).
  pub track_id: u64,
  /// Decode timestamp (nanoseconds).
  ///
  /// For codecs with frame reordering (e.g. video streams with B-frames), packets must be emitted
  /// in **decode order** (sample index order). `dts_ns` is monotonic in that order.
  pub dts_ns: u64,
  /// Presentation timestamp (nanoseconds).
  ///
  /// Important: `pts_ns` is **not guaranteed to be monotonic** for video streams with B-frames.
  /// Demuxers must not reorder packets by PTS; presentation-order reordering (if needed) belongs
  /// downstream from demux.
  pub pts_ns: u64,
  /// Packet duration (nanoseconds), when known.
  pub duration_ns: u64,
  /// Encoded bytes for the packet.
  pub data: MediaData,
  /// Whether this packet is a random access point (keyframe / sync sample).
  pub is_keyframe: bool,
}

impl MediaPacket {
  #[inline]
  pub fn as_slice(&self) -> &[u8] {
    self.data.as_slice()
  }
}
