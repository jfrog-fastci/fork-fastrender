/// Decision logic for the real-time audio mixer callback.
///
/// This module is intentionally CPAL-independent so it can be unit-tested without enabling the
/// `audio_cpal` feature (and without requiring native audio system dependencies in CI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MixerCallbackAction {
  /// Mix samples from sinks into the output buffer.
  Mix,
  /// Output silence, but still drain sinks to maintain their notion of "playback progress".
  SilenceAndDrain,
  /// Output silence without draining sinks.
  Silence,
}

/// Decide what the mixer callback should do for the current cycle.
///
/// - If there are no sinks, we always output silence.
/// - If any sink is potentially audible, we must mix.
/// - Otherwise, we output silence and optionally drain sinks (to implement "mute" semantics rather
///   than "pause" semantics).
#[inline]
pub(crate) fn decide_mixer_callback_action(
  has_sinks: bool,
  any_sink_maybe_audible: bool,
  drain_silent_sinks: bool,
) -> MixerCallbackAction {
  if !has_sinks {
    MixerCallbackAction::Silence
  } else if any_sink_maybe_audible {
    MixerCallbackAction::Mix
  } else if drain_silent_sinks {
    MixerCallbackAction::SilenceAndDrain
  } else {
    MixerCallbackAction::Silence
  }
}

#[cfg(test)]
mod tests {
  use super::{decide_mixer_callback_action, MixerCallbackAction};

  #[test]
  fn audio_mixer_opt_decision_no_sinks() {
    assert_eq!(
      decide_mixer_callback_action(false, false, true),
      MixerCallbackAction::Silence
    );
    assert_eq!(
      decide_mixer_callback_action(false, true, true),
      MixerCallbackAction::Silence
    );
  }

  #[test]
  fn audio_mixer_opt_decision_mix_if_any_audible() {
    assert_eq!(
      decide_mixer_callback_action(true, true, true),
      MixerCallbackAction::Mix
    );
    assert_eq!(
      decide_mixer_callback_action(true, true, false),
      MixerCallbackAction::Mix
    );
  }

  #[test]
  fn audio_mixer_opt_decision_silence_and_drain_if_configured() {
    assert_eq!(
      decide_mixer_callback_action(true, false, true),
      MixerCallbackAction::SilenceAndDrain
    );
    assert_eq!(
      decide_mixer_callback_action(true, false, false),
      MixerCallbackAction::Silence
    );
  }
}

