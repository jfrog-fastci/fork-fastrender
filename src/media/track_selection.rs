#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackSelectionPolicy {
  pub prefer_enabled: bool,
  pub prefer_default: bool,
  pub prefer_non_commentary: bool,
  pub prefer_non_hearing_impaired: bool,
  pub prefer_highest_resolution: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackFilterMode {
  /// Only emit packets from the selected primary audio/video tracks.
  #[default]
  PrimaryOnly,
  /// Emit packets from all supported tracks.
  AllTracks,
}

impl Default for TrackSelectionPolicy {
  fn default() -> Self {
    Self {
      prefer_enabled: true,
      prefer_default: true,
      prefer_non_commentary: true,
      prefer_non_hearing_impaired: true,
      prefer_highest_resolution: true,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackCandidate<Id> {
  pub id: Id,
  pub enabled: bool,
  pub default: bool,
  pub commentary: bool,
  pub hearing_impaired: bool,
  /// Pixel count (width*height) for video tracks; should be 0 for audio tracks.
  pub pixel_count: u64,
}

pub fn select_primary_video_track_id<Id: Copy>(
  candidates: &[TrackCandidate<Id>],
  policy: TrackSelectionPolicy,
) -> Option<Id> {
  select_track_id(candidates, policy, true)
}

pub fn select_primary_audio_track_id<Id: Copy>(
  candidates: &[TrackCandidate<Id>],
  policy: TrackSelectionPolicy,
) -> Option<Id> {
  // Audio selection is intentionally conservative: after applying enabled/default/commentary/etc
  // preferences, pick the first matching track for determinism.
  select_track_id(candidates, policy, false)
}

fn select_track_id<Id: Copy>(
  candidates: &[TrackCandidate<Id>],
  policy: TrackSelectionPolicy,
  consider_resolution: bool,
) -> Option<Id> {
  if candidates.is_empty() {
    return None;
  }

  let has_enabled = policy.prefer_enabled && candidates.iter().any(|t| t.enabled);
  let has_default = policy.prefer_default && candidates.iter().any(|t| t.default);
  let has_non_commentary = policy.prefer_non_commentary && candidates.iter().any(|t| !t.commentary);
  let has_non_hearing_impaired =
    policy.prefer_non_hearing_impaired && candidates.iter().any(|t| !t.hearing_impaired);

  let mut best_id: Option<Id> = None;
  let mut best_key: (u8, u8, u8, u8, u64) = (0, 0, 0, 0, 0);

  for candidate in candidates {
    let enabled_key = if has_enabled { u8::from(candidate.enabled) } else { 1 };
    let default_key = if has_default { u8::from(candidate.default) } else { 1 };
    let non_commentary_key = if has_non_commentary {
      u8::from(!candidate.commentary)
    } else {
      1
    };
    let non_hearing_impaired_key = if has_non_hearing_impaired {
      u8::from(!candidate.hearing_impaired)
    } else {
      1
    };
    let pixel_count_key = if consider_resolution && policy.prefer_highest_resolution {
      candidate.pixel_count
    } else {
      0
    };

    let key = (
      enabled_key,
      default_key,
      non_commentary_key,
      non_hearing_impaired_key,
      pixel_count_key,
    );

    // Strictly-greater only, so ties are resolved by first-in-list for determinism.
    if best_id.is_none() || key > best_key {
      best_id = Some(candidate.id);
      best_key = key;
    }
  }

  best_id
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tie_breaks_to_first_track_for_audio() {
    let policy = TrackSelectionPolicy::default();
    let tracks = [
      TrackCandidate {
        id: 1u32,
        enabled: true,
        default: true,
        commentary: false,
        hearing_impaired: false,
        pixel_count: 0,
      },
      TrackCandidate {
        id: 2u32,
        enabled: true,
        default: true,
        commentary: false,
        hearing_impaired: false,
        pixel_count: 0,
      },
    ];

    assert_eq!(select_primary_audio_track_id(&tracks, policy), Some(1));
  }
}
