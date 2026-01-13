# Media clocking & A/V sync model

This document describes the **intended** timing model for media playback (video + audio) in
FastRender.

The goal is to prevent “slow drift” and “mysterious desync” bugs by being explicit about:

* what *time* means at each layer (container timestamps, timeline time, device time),
* which clock is authoritative (master),
* what the UI tick does (and does *not* do),
* what tolerances are expected, and why.

Implementation map (keep these modules aligned with this doc):

* `src/media/timebase.rs` — container timebase/tick ↔ `Duration` conversions (PTS normalization)
* `src/media/clock.rs` — `MediaClock` abstraction + per-stream timeline mapping (`AudioStreamClock`)
* `src/media/audio/mod.rs` — `AudioBackend` trait + `AudioClock` (audio device time exposure)
* `src/media/audio/null_backend.rs` — `NullAudioBackend` (silence / CI fallback)
* `src/media/audio/cpal_backend.rs` — CPAL output backend (feature = `audio_cpal`)
* `src/media/av_sync.rs` — video scheduling + correction policy (drop/hold/delay) (**intended module
  path**)
* `src/js/clock.rs` — existing `Clock` + `VirtualClock` pattern used for deterministic time in tests
  (media clocking should mirror this pattern)

> Note: at any point in time, code may be mid-migration. If the implementation diverges from this
> model, fix the code or update this doc; don’t let the mismatch persist.

## Enabling real audio output (Cargo features)

Audio backends that require platform/system libraries are intentionally **opt-in** so CI/minimal
hosts don't need system audio development packages.

- Default: `NullAudioBackend` (silence; always available).
- `audio_cpal`: real-time audio output via [`cpal`](https://crates.io/crates/cpal).
  - Linux note: typically requires system packages (e.g. ALSA headers).
- `audio_wav`: pure-Rust WAV debug backend (writes PCM samples into a `.wav` file).

Example (desktop browser UI with real audio output):

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui,audio_cpal --bin browser
```

Compile-only check for developers (does not open an audio device; runs a tiny unit test):

```bash
bash scripts/cargo_agent.sh test --features audio_cpal --lib audio_cpal_feature_compiles -- --exact
```

---

## Key definitions (terminology)

### System monotonic time

*What it is:* a monotonic wall-clock suitable for scheduling and measuring durations.

*In Rust:* typically `std::time::Instant` (or an injectable wrapper around it).

*What it is used for:*

* “Wake me up around X” deadlines (UI/event-loop waits, timers).
* Measuring decode time, render budget usage, etc.

*What it is **not** used for (when audio is present):*

* Advancing the media timeline. The system monotonic clock can drift relative to the audio device
  clock, which will show up as A/V drift.

---

### Media timeline time

*What it is:* the current playback position on the conceptual media timeline, in seconds.

This is the time surfaced to:

* `HTMLMediaElement.currentTime`
* `timeupdate` events
* seeking logic (“go to t=37.5s”)

Properties:

* Timeline time **pauses** when playback is paused.
* Timeline time **jumps** when seeking.
* Timeline time is affected by `playbackRate` (1.0 = normal speed).

Think of “timeline time” as a value produced by mapping the chosen *master clock* through a stateful
function that accounts for pause/seek/rate.

---

### PTS (presentation timestamp)

*What it is:* the per-sample / per-frame timestamp from the container/codec that says *when this
frame should be presented* on the media timeline.

Important properties:

* PTS is usually expressed in a stream timebase (e.g. ticks), which must be converted to seconds
  (see `src/media/timebase.rs`).
* PTS is defined in *presentation order*; decode order may differ (e.g. with B-frames).
* PTS can be missing/invalid in malformed content; the demuxer/decoder layer must normalize it into
  a monotonic (or at least usable) timeline for the renderer.

The **A/V sync layer** should reason in terms of:

* `video_frame.pts` (timeline seconds)
* `audio_sample.pts` (timeline seconds)
* “current master timeline time” (also seconds)

---

### Timestamp representation (avoid floats)

Most drift bugs are caused by choosing the wrong **clock**, but a surprising number come from
choosing the wrong **number type**.

Guidelines:

* Prefer `std::time::Duration` (or integer tick counters) for internal media timestamps.
* Avoid accumulating `f32` / `f64` deltas over long playback: floating point rounding error can
  accumulate into noticeable drift, especially with high-resolution container timebases (e.g. 90kHz
  PTS) and long-running playback.
* Use `src/media/timebase.rs` for tick ↔ `Duration` conversions (it rounds and saturates instead of
  overflowing).
* Only convert to `f64` seconds at API boundaries like `HTMLMediaElement.currentTime` / UI displays.

---

### Audio device time

*What it is:* the audio hardware’s notion of time, expressed as “when samples will be heard”.

In practice, audio APIs often give you one of:

* an explicit timestamp from the device (best), or
* a callback-driven stream with an implicit clock (you infer time from sample counts).

FastRender should expose an **audio clock** to the rest of the media pipeline that answers:

> “What media timeline time is currently reaching the speakers (or will reach them after a known
> constant latency)?”

This clock is the **master** when audio is present (see below).

In code today, audio time is surfaced in two layers:

1. [`AudioClock`](../src/media/audio/mod.rs) — raw “device time” as exposed by the audio backend.

* `AudioClock::OutputFrames { .. }` — derived from a backend playhead counter (preferred when output
  is active, e.g. CPAL).
* `AudioClock::Instant { .. }` — derived from wall-clock time (used by `NullAudioBackend`).

2. [`MediaClock`](../src/media/clock.rs) + [`AudioStreamClock`](../src/media/clock.rs) — the
   clocking abstraction intended for A/V sync + `HTMLMediaElement.currentTime`.
   * `MediaClock` is the “master clock” interface (audio device time when audio is present).
   * `AudioStreamClock` maps a shared device clock into a per-element media timeline and applies
     pause/seek/rate changes without accumulating drift (see `AudioStreamClock::seek` /
     `AudioStreamClock::set_rate`).

At the time of writing, these layers are not yet fully wired together for real playback; when they
are, prefer driving `MediaClock` from the audio backend’s `AudioClock::OutputFrames` (plus an output
latency estimate) rather than from `Instant`.

---

## Audio backend clock contract (what `src/media/audio/*` must provide)

Even with “audio is master” as a principle, the media pipeline still needs a precise contract for
what time the audio backend reports.

In code, the contract surface is:

* `AudioBackend` + `AudioSink` in `src/media/audio/mod.rs`
* `AudioClock` in `src/media/audio/mod.rs`

The audio backend should provide:

* A **sample rate** (`Hz`) and channel configuration for the output stream.
* A monotonically increasing **playhead counter**: “how many output frames have been presented to the
  device since stream start”.
* An estimate of **output latency** (“how long after we hand frames to the backend they become
  audible”).
* A way to query **audio device time in timeline units**, i.e.:
  * “What media timeline time is *currently audible* (or will become audible after the modeled
    latency)?”

### Preferred: backend timestamps

If the backend/API provides real device timestamps (or callback timestamps tied to the device clock),
use them. This is the best way to make `audio_device_time` stable and low-jitter.

### Fallback: sample-counter clock

If no timestamps are available, derive time from the number of played frames:

```text
played_time = played_frames / sample_rate
audio_device_time ≈ played_time + output_latency_constant
```

Notes:

* The playhead must be derived from the backend’s real consumption (or a counter that tracks the
  callback’s delivered frames), not from UI ticks.
* A constant latency model is acceptable initially (it creates offset, not drift). See “Known
  limitations”.

---

## Master clock selection (why audio is master)

When an audio track is present and audio output is enabled, **audio is the master clock**.

Reasons:

1. **The user’s perception is anchored to audio.** If audio stutters or is time-warped, it is
   extremely noticeable. Video can be dropped/held occasionally with much less perceived harm.
2. **Audio devices run on their own clock.** The hardware clock can drift relative to
   `Instant::now()` by tens of ms over time. If we advance “currentTime” using system monotonic time
   while audio plays using the device clock, we will get A/V drift even if both pipelines are
   “perfect”.
3. **Audio requires real-time scheduling.** Audio output is constrained by the backend’s callback /
   buffer deadlines. Video can adapt to whatever the audio clock is doing.

### Fallback clocks (no audio / no device)

If there is no audio track, or audio output is disabled/unavailable:

* Use a **monotonic system clock** as the master (e.g. `Instant` via an injectable `Clock`).
* The clock origin must be “play start” plus a stored offset so pause/seek remain correct.

Muting (`HTMLMediaElement.muted = true` or `volume = 0`) is **not** a fallback condition: playback
continues silently, so audio queues must still drain and the audio device clock remains a valid master
clock.

This fallback is fine because there is no external hardware clock to drift against.

---

## UI tick: wake-up mechanism, not a time source

The UI / event loop “tick” (e.g. a per-frame update, a timer firing, a winit `RedrawRequested`) is
**only a wake-up mechanism**.

In FastRender’s windowed browser, “tick” is concretely a UI→worker protocol message:

* `UiToWorker::Tick { tab_id }` in `src/ui/messages.rs`

Importantly, this message does **not** carry a timestamp. That’s intentional: the worker is expected
to query whatever clock is appropriate (for JS timers, for media playback, etc.) rather than trusting
the UI thread’s scheduling jitter.

Where it comes from: the windowed `browser` app schedules these ticks using winit timers
(`ControlFlow::WaitUntil`) around `ANIMATION_TICK_INTERVAL` in `src/bin/browser.rs`. That interval is
best-effort (it can jitter, coalesce, or pause), so it must never be treated as a timeline clock.

What the tick does:

* Gives the media pipeline CPU time to:
  * pump decoders,
  * submit audio,
  * choose the correct video frame for the current timeline time,
  * request a redraw if the displayed frame should change.
* Provides a place to schedule the **next wake-up** (e.g. “wake me around when the next frame is
  due”).

What the tick must **not** do:

* **Do not** treat “time since last tick” as progress on the media timeline.
  * Ticks can jitter, coalesce, or pause entirely (window moved, system under load, backgrounded).
  * Accumulating `dt` from ticks is a classic way to create drift.
* **Do not** advance media time by a fixed amount per tick (e.g. “+16ms each tick”).
  * This is a tempting pattern because it is deterministic, and FastRender currently uses a fixed
    tick step for **CSS animation sampling** (see `TICK_ANIMATION_STEP_MS` in
    `src/ui/render_worker.rs`), but that approach is not suitable for audio/video because the audio
    device continues advancing in real time regardless of UI tick delivery.

Correct model:

1. Tick wakes up.
2. Media pipeline reads the **master clock** (audio device time when present).
3. Media pipeline computes “timeline now”.
4. Media pipeline selects/presents frames based on that timeline time.
5. Media pipeline returns/schedules the next wake-up deadline (in **system monotonic time**) purely
   to reduce latency and CPU usage.

---

## Timeline mapping (pause/seek/rate)

The media timeline is derived from a master clock via state:

* `base_timeline_time` — timeline time at the moment we last changed state (play/pause/seek).
* `base_master_time` — master clock time at that same moment.
* `playback_rate` — multiplier (1.0 default).
* `playing` — whether timeline advances.

Conceptually:

```text
if playing:
  timeline_now = base_timeline_time + (master_now - base_master_time) * playback_rate
else:
  timeline_now = base_timeline_time
```

Seeking updates `base_timeline_time` (and usually resets `base_master_time`).

This mapping lives in `src/media/clock.rs` today as `AudioStreamClock`, so *every* subsystem (audio
submission, video presentation, JS APIs) can consume the same “timeline now”.

---

## A/V sync policy (video scheduled to audio)

When audio is master, video presentation is a function of:

* `t = timeline_now()` from `src/media/clock.rs`
* the set of decoded video frames with `frame.pts`

The renderer should display a frame whose PTS is “closest to but not ahead of” the current time, with
bounded tolerance.

Typical actions:

* **Video ahead** (frame PTS too far *after* `t`): delay presenting; keep the previous frame and
  schedule an earlier wake-up.
* **Video behind** (frame PTS too far *before* `t`): drop frames until within tolerance.

This logic belongs in `src/media/av_sync.rs`.

### Recommended default tolerances

These numbers are intentionally conservative defaults; tune per platform later, but keep the meaning
stable:

* **In-sync window:** `|video_pts - t| <= 20ms`
  * Differences smaller than this are treated as “noise” from scheduling jitter and timestamp
    quantization.
* **Drop threshold (video late):** `t - video_pts > 80ms`
  * If video is more than ~2–5 frames late depending on FPS, drop until within the in-sync window.
* **Delay threshold (video early):** `video_pts - t > 40ms`
  * If video is significantly early, hold the last frame and wake up closer to the target PTS.

Why asymmetric? Being a bit early is usually less harmful than being noticeably late (which looks
like audio leading the picture). Also, dropping is a harsher action than delaying.

If/when we implement frame-time-aware tolerances (e.g. based on measured FPS), keep the above as a
cap/floor so behavior stays predictable.

---

## Deterministic tests: `NullAudioBackend` + `VirtualClock`

Media timing code is notoriously hard to test if it depends on real audio hardware and real wall
time.

The intended test strategy is:

* A `VirtualClock` that only advances when the test tells it to.
  * There is already a pattern for this in `src/js/clock.rs` (`VirtualClock` implementing a `Clock`
    trait).
  * Media clocking is designed to support this via `MediaClock` in `src/media/clock.rs`: production
    code can use `RealAudioDeviceClock` (wall time), while tests can inject a fake/virtual device
    clock (see the `FakeDeviceClock` used in `src/media/clock.rs` unit tests).
* A `NullAudioBackend` (`src/media/audio/*`) that:
  * accepts audio samples into a queue,
  * advances “device playback position” based on the injected `VirtualClock`,
  * exposes an audio master clock derived from “played frames / sample_rate”.

Current implementation note: `src/media/audio/null_backend.rs` currently uses `Instant` for its
clock. That is fine for “best effort” headless runs, but tests that need strict determinism should
avoid wall time. Two ways to achieve that:

* Add a `Virtual`/injected-clock variant to `AudioClock` (mirroring `src/js/clock.rs`) and teach
  `NullAudioBackend` to use it in tests, or
* Use `AudioClock::OutputFrames` with a test-controlled `frames_played` counter derived from the
  `VirtualClock` (e.g. `frames = now * sample_rate`).

With these pieces, a test can:

1. start playback at t=0,
2. advance the `VirtualClock` by exactly 33ms,
3. run one `media.tick()`,
4. assert which frame is selected and what `currentTime` reports,

…with **zero flakiness** and without requiring audio devices in CI.

---

## Known limitations (documented so drift bugs are diagnosable)

### Constant output latency model

Many audio APIs do not provide a precise “samples hit the speaker at time X” timestamp. A common
fallback is to assume a **constant output latency**:

```text
audio_device_time ≈ (played_frames / sample_rate) + output_latency_constant
```

This has two implications:

* It can introduce a **constant A/V offset** (video consistently early/late by a fixed amount).
* It should **not** create unbounded drift by itself, as long as the audio clock is still derived
  from the device/sample counter, not from UI ticks.

When debugging, distinguish:

* **Offset:** constant error (fix by calibrating latency).
* **Drift:** error grows over time (fix by ensuring a single master clock is used everywhere).

Current implementation note: FastRender’s existing clocks (`AudioClock` in `src/media/audio/mod.rs`,
`RealAudioDeviceClock` in `src/media/clock.rs`) do not yet incorporate an explicit output-latency
estimate, which is equivalent to assuming `output_latency_constant = 0`. Expect a possible constant
offset until we plumb real latency/timestamps through the backend.

### Backend timestamp quality varies

Different OS backends provide different levels of timing fidelity. When a backend can provide a real
device timestamp, prefer it; when it cannot, use the best available sample-counter model, but keep
the audio clock as the master regardless.

---

## Quick drift-bug checklist

If you observe A/V drift or “currentTime slowly diverges from what you hear”:

1. Verify that **only one clock** is used to advance the media timeline:
   * audio device time if audio is playing,
   * system monotonic time only when audio is absent.
2. Verify the UI tick is not accumulating `dt` to advance timeline time.
3. Confirm `src/media/av_sync.rs` compares **video PTS to timeline now**, not to UI tick timestamps.
4. If the error is constant, check the output-latency constant rather than changing tolerances.
