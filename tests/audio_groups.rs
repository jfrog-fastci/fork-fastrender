use std::sync::Arc;

use fastrender::media::audio::{
  audio_engine_config, AudioEngine, AudioGroupId, AudioSink, NullAudioBackend,
};

fn all_samples_eq(samples: &[f32], expected: f32) -> bool {
  samples
    .iter()
    .all(|sample| (*sample - expected).abs() < 1e-6)
}

#[test]
fn group_mute_affects_all_streams_in_group() {
  let backend = Arc::new(NullAudioBackend::new_deterministic_with_defaults(48_000, 1));
  let engine = AudioEngine::new_with_backend(audio_engine_config(), backend.clone());

  let group_a = engine.create_group();
  let group_b = engine.create_group();

  let a1 = engine.create_sink_in_group(group_a);
  let a2 = engine.create_sink_in_group(group_a);
  let b1 = engine.create_sink_in_group(group_b);

  assert_eq!(a1.push_interleaved_f32(&[1.0]), 1);
  assert_eq!(a2.push_interleaved_f32(&[1.0]), 1);
  assert_eq!(b1.push_interleaved_f32(&[1.0]), 1);

  engine.set_group_muted(group_a, true);

  let out = backend.render(1);
  assert!(
    all_samples_eq(&out, 1.0),
    "only group_b should contribute when group_a is muted"
  );
}

#[test]
fn master_mute_affects_all_streams() {
  let backend = Arc::new(NullAudioBackend::new_deterministic_with_defaults(48_000, 1));
  let engine = AudioEngine::new_with_backend(audio_engine_config(), backend.clone());

  let a1 = engine.create_sink();
  let a2 = engine.create_sink();

  assert_eq!(a1.push_interleaved_f32(&[1.0]), 1);
  assert_eq!(a2.push_interleaved_f32(&[1.0]), 1);

  engine.set_master_muted(true);

  let out = backend.render(1);
  assert!(
    all_samples_eq(&out, 0.0),
    "no stream should contribute when master is muted"
  );
}

#[test]
fn sink_group_can_be_changed_later() {
  let backend = Arc::new(NullAudioBackend::new_deterministic_with_defaults(48_000, 1));
  let engine = AudioEngine::new_with_backend(audio_engine_config(), backend.clone());

  let group_a: AudioGroupId = engine.create_group();
  let group_b: AudioGroupId = engine.create_group();

  let sink = engine.create_sink_in_group(group_a);

  // Muting group A silences the sink...
  engine.set_group_muted(group_a, true);

  // ...but switching the sink to group B should make it audible again (group B is not muted).
  sink.set_group_id(group_b);

  assert_eq!(sink.push_interleaved_f32(&[1.0]), 1);
  let out0 = backend.render(1);
  assert!(all_samples_eq(&out0, 1.0));

  // Now muting group B should silence it.
  engine.set_group_muted(group_b, true);
  assert_eq!(sink.push_interleaved_f32(&[1.0]), 1);
  let out1 = backend.render(1);
  assert!(all_samples_eq(&out1, 0.0));
}
