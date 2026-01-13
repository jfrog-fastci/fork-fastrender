#![no_main]

use arbitrary::Arbitrary;
use fastrender::media::{
  duration_to_ticks, ticks_to_duration, ticks_to_timestamp, timestamp_to_ticks, MediaTimestamp,
  Timebase,
};
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

#[derive(Arbitrary, Debug)]
struct MediaTimebaseInput {
  num: u32,
  den: u32,
  ticks: i64,
}

fuzz_target!(|input: MediaTimebaseInput| {
  let tb = Timebase::new(input.num, input.den);

  let d = ticks_to_duration(input.ticks, tb);
  let ticks2 = duration_to_ticks(d, tb);
  let ts: MediaTimestamp = ticks_to_timestamp(input.ticks, tb);
  let ticks3 = timestamp_to_ticks(ts, tb);

  // Round-trips should generally drift by at most one tick, but the conversion goes through
  // nanoseconds (`Duration`), so extremely high-resolution timebases (<1ns/tick) can legitimately
  // lose more than one tick. Only assert the stronger invariant when a tick is at least 1ns and the
  // conversion did not saturate.
  let tick_nanos_num = (tb.num as u128).saturating_mul(1_000_000_000u128);
  let tick_is_at_least_1ns = tick_nanos_num >= tb.den as u128;

  if input.ticks > 0
    && tb.num != 0
    && tb.den != 0
    && d != Duration::MAX
    && ticks2 != i64::MAX
    && tick_is_at_least_1ns
  {
    let diff = (i128::from(ticks2) - i128::from(input.ticks)).abs();
    assert!(diff <= 1);
  }

  // Exercise signed timestamp conversions too. Only assert a tight round-trip bound when the
  // timestamp did not saturate.
  if tb.num != 0
    && tb.den != 0
    && tick_is_at_least_1ns
    && ts.as_nanos() != i64::MAX
    && ts.as_nanos() != i64::MIN
    && ticks3 != i64::MAX
    && ticks3 != i64::MIN
  {
    let diff = (i128::from(ticks3) - i128::from(input.ticks)).abs();
    assert!(diff <= 1);
  }
});
