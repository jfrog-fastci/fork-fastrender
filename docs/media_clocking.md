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
* `src/media/audio_clock.rs` — `InterpolatedAudioClock` (smooth audio clock derived from callback frame counts)
* `src/media/audio/mod.rs` — `AudioBackend` + `AudioClock` + `AudioOutputInfo` (backend time + output-latency estimate)
* `src/media/audio/config.rs` — `AudioEngineConfig` (`FASTR_AUDIO_*` tuning + deterministic overrides for tests)
* `src/media/audio/engine.rs` — `AudioEngine` (backend selection, sink grouping/volume, exposes a device clock for A/V sync)
* `src/media/audio_engine.rs` — output-stream idle controller (`IdleEngine`): debounced start/stop of the OS/device output stream; used by the CPAL backend (not `crate::media::audio::AudioEngine`)
* `src/media/master_clock.rs` — `MasterClock` (chooses audio vs system master clock, keeps time continuous)
* `src/media/clock.rs` — `MediaClock` abstraction + `PlaybackClock` (play/pause/seek/rate timeline mapping)
  + `AudioStreamClock` (map a shared device clock into a per-element timeline)
* `src/media/audio/drift.rs` — audio drift correction (target-buffer controller + drift-aware resampling helper)
* `src/media/audio/null_backend.rs` — `NullAudioBackend` (silence / CI fallback)
* `src/media/audio/cpal_backend.rs` — CPAL output backend (feature = `audio_cpal`)
* `src/media/audio/wav_backend.rs` — WAV file output backend (feature = `audio_wav`)
* `src/media/av_sync.rs` — video scheduling + correction policy (drop/hold/delay)
* `src/js/clock.rs` — existing `Clock` + `VirtualClock` pattern used for deterministic time in tests
  (media clocking should mirror this pattern)

> Note: at any point in time, code may be mid-migration. If the implementation diverges from this
> model, fix the code or update this doc; don’t let the mismatch persist.

## Enabling real audio output (Cargo features)

Audio backends that require platform/system libraries are intentionally **opt-in** so CI/minimal
hosts don't need system audio development packages.

- When `audio_cpal` is not enabled (typical default/CI builds): `NullAudioBackend` (silence; always
  available).
- `audio_cpal`: real-time audio output via [`cpal`](https://crates.io/crates/cpal).
  - Linux note: typically requires system packages (e.g. ALSA headers).
- `audio_wav`: pure-Rust WAV debug backend (writes PCM samples into a `.wav` file).

Runtime selection (CI-friendly):

- `FASTR_AUDIO_BACKEND=null|cpal|auto` – select which backend to use at runtime when constructing the
  default `AudioBackend` (default: `auto` → prefer CPAL when compiled, else null/silence).
- `FASTR_AUDIO_DEVICE=<substring>` – best-effort output device selection for CPAL (case-insensitive
  substring match on the device name; unset uses the host default device).

CI note: the `ci` feature umbrella intentionally avoids enabling `audio_cpal` by default (to keep
Linux CI/agent builds free of system audio development dependencies). Developers can opt in locally
with `--features audio_cpal`.

Example (desktop browser UI with real audio output):

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui,audio_cpal --bin browser
```

Compile-only check for developers (does not open an audio device; runs a tiny unit test):

```bash
bash scripts/cargo_agent.sh test --features audio_cpal --lib audio_cpal_feature_compiles -- --exact
```

## Audio engine configuration (`FASTR_AUDIO_*`)

FastRender’s audio output pipeline is configured by `AudioEngineConfig` (`src/media/audio/config.rs`).
Values are read once from `FASTR_AUDIO_*` environment variables and clamped to hard limits (see
`src/media/audio/limits.rs`). Unit tests can override the process-global config via
`crate::media::audio::set_audio_engine_config(...)`.

Important: this config is read directly from the process environment (not via `RuntimeToggles`).

Key knobs:

- `FASTR_AUDIO_STREAM_MAX_BUFFER_MS` — per-sink buffered audio limit (default 2000ms; hard cap 5000ms).
- `FASTR_AUDIO_MAX_STREAMS` — global cap on concurrently active audio sinks (default 32).
- `FASTR_AUDIO_BUFFER_BUDGET` — global buffered-audio budget across all sinks (default 32 MiB;
  accepts `kb`/`mb`/`gb`/`kib`/`mib`/`gib` suffixes and `_` separators).
- `FASTR_AUDIO_IDLE_TIMEOUT_MS` — when all streams are idle for this duration, the output backend may
  stop/pause its device stream (power saving; used by the CPAL backend; default 3000ms).
- `FASTR_AUDIO_DEFAULT_SAMPLE_RATE_HZ` / `FASTR_AUDIO_DEFAULT_CHANNELS` — default output format for
  non-device backends (null/WAV; defaults 48kHz stereo).
- `FASTR_AUDIO_PREROLL_MS` / `FASTR_AUDIO_LOW_BUFFER_MS` / `FASTR_AUDIO_LOW_BUFFER_DEBOUNCE_MS` —
  buffering thresholds intended for playback state/backpressure. These are currently experimental
  and may not be fully wired yet.

When the effective sink limit is reached, `AudioEngine::create_sink*` returns a no-op sink (push
accepts 0 samples). The effective limit is:

```text
min(FASTR_AUDIO_MAX_STREAMS, FASTR_AUDIO_BUFFER_BUDGET / bytes_per_sink)
```

…where `bytes_per_sink` is derived from the output format (sample rate/channels) and
`FASTR_AUDIO_STREAM_MAX_BUFFER_MS`.

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

In code today, audio time is surfaced in a few layers:

1. [`InterpolatedAudioClock`](../src/media/audio_clock.rs) — a smooth clock derived from callback
   frame counts (and optional backend timestamps). This avoids “stair-stepping” when callers query
   between audio callbacks.

2. [`AudioClock`](../src/media/audio/mod.rs) — raw backend time.

    * `AudioClock::OutputFrames { .. }` is typically derived from “frames written” in the output
      callback (via `InterpolatedAudioClock`).
    * `AudioClock::Instant { .. }` is a wall-clock fallback used when an output playhead counter is
      unavailable (e.g. if the `audio_cpal` backend falls back to silence after a device error).

   **Important:** `AudioClock::time()` is not guaranteed to mean “time heard”. For
   output-frame-derived clocks it often means “time of frames written/committed to the backend”, which
   can be ahead of the speakers by a roughly constant buffer duration. When you need “time heard”,
   subtract [`AudioOutputInfo::estimated_output_latency`](../src/media/audio/mod.rs).

3. [`MasterClock`](../src/media/master_clock.rs) — selects audio vs system clock domains and keeps
   time continuous across source changes (e.g. during startup/buffering audio may not be started yet).

4. [`PlaybackClock`](../src/media/clock.rs) — maps the chosen master clock onto the media timeline and
   implements play/pause/seek/rate without accumulating drift.

---

## Audio backend clock contract (what `src/media/audio/*` must provide)

Even with “audio is master” as a principle, the media pipeline still needs a precise contract for
what time the audio backend reports.

In code, the contract surface is:

* `AudioBackend` + `AudioSink` in `src/media/audio/mod.rs`
* `AudioClock` in `src/media/audio/mod.rs`

The audio backend should provide:

* A **sample rate** (`Hz`) and channel configuration for the output stream.
* A monotonically increasing **playhead counter**: “how many output frames have been written/committed
  to the output backend since stream start”.
  * Depending on the backend this may mean “frames produced in the output callback”, which can be
    ahead of “time heard” by roughly the output latency.
* An estimate of **output latency** (“how long after we hand frames to the backend they become
  audible”).
* A way to query **audio device time in timeline units**, i.e.:
  * “What media timeline time is *currently audible* (or will become audible after the modeled
    latency)?”

### Preferred: backend timestamps

If the backend/API provides real device timestamps (or callback timestamps tied to the device clock),
use them. This is the best way to make `audio_device_time` stable and low-jitter.

### Fallback: sample-counter clock

If no timestamps are available, derive time from the number of output frames written/committed (a
sample-counter clock):

```text
written_time = frames_written / sample_rate
time_heard ≈ written_time - output_latency_constant
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

* `UiToWorker::Tick { tab_id, delta }` in `src/ui/messages.rs`

Importantly, this message is a **wake-up mechanism**, not a master time source:

* It carries a best-effort `delta` (elapsed time since the previous tick for that tab) so the worker
  can advance deterministic time-based effects like CSS animations and JS timers.
* It does **not** carry an authoritative “media time” timestamp. Audio/video playback must still
  query its own clocks (audio device time when available) rather than inferring time from tick
  cadence/jitter.

Where it comes from: the windowed `browser` app schedules ticks using winit timers
(`ControlFlow::WaitUntil`) based on per-tab schedule hints from the worker:

* **General time-based effects** (CSS animations/transitions, JS timers/rAF): driven by
  `RenderedFrame.next_tick` and `WorkerToUi::TickHint` (see `App::drive_animation_tick` in
  `src/bin/browser.rs`).
* **Media playback deadlines** (video frame presentation / A‑V sync): driven by
  `WorkerToUi::RequestWakeAfter { reason: WakeReason::Media }`, which requests a **one-shot** wakeup
  even when the worker did not produce a new frame (e.g. holding the previous frame until the next
  PTS). The UI typically responds by waking and delivering a `Tick` with `delta=Duration::ZERO`.
  `after=Duration::MAX` is treated as "cancel pending media wake".

Tick delivery is best-effort: it can jitter, coalesce, or pause/reset entirely, so it must never be
treated as the master media timeline clock.

What the tick does:

* Gives the media pipeline CPU time to:
  * pump decoders,
  * submit audio,
  * choose the correct video frame for the current timeline time,
  * request a redraw if the displayed frame should change.
* Provides a place to schedule the **next wake-up** (e.g. “wake me around when the next frame is
  due”).

What the tick must **not** do:

* **Do not** treat tick `delta` (or “time since last tick”) as progress on the media timeline.
  * Ticks can jitter, coalesce, or pause/reset entirely (window moved, system under load,
    backgrounded).
  * Accumulating `delta` from ticks is a classic way to create drift.
* **Do not** advance media time by a fixed amount per tick (e.g. “+16ms each tick”).
  * This is a tempting pattern because ticks carry a `delta` and can be driven deterministically in
    tests, and the worker does use the tick delta for **CSS animation sampling**.
  * But that approach is not suitable for audio/video: the audio device continues advancing in real
    time regardless of UI tick delivery, and the UI may also deliver `delta=0` “wake-up” ticks for
    media scheduling.

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

This mapping lives in `src/media/clock.rs` today as `PlaybackClock`, so *every* subsystem (audio
submission, video presentation, JS APIs) can consume the same “timeline now” without accumulating
tick-derived drift.

---

## A/V sync policy (video scheduled to audio)

When audio is master, video presentation is a function of:

* `t = timeline_now()` from `src/media/clock.rs`
* the set of decoded video frames with `frame.pts`

The renderer should display a frame whose PTS is “close enough” to the current time, with bounded
tolerance. Small early/late deltas are treated as noise (present the frame), while larger deltas
trigger corrective actions (hold when too early; drop when too late).

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

### Tuning tolerances via environment variables

The A/V sync thresholds can be overridden at runtime via environment variables (values are integer
milliseconds; underscores are allowed):

Preferred env vars:

* `FASTRENDER_AVSYNC_IN_SYNC_MS` — in-sync window (`|video_pts - now| <= tolerance` ⇒ present)
* `FASTRENDER_AVSYNC_DROP_LATE_MS` — drop threshold (`now - video_pts > max_late` ⇒ drop)
* `FASTRENDER_AVSYNC_DELAY_EARLY_MS` — hold threshold (`video_pts - now > max_early` ⇒ hold + wake later)

Legacy env vars (still supported):

* `FASTR_AV_SYNC_TOLERANCE_MS` — in-sync window (`|video_pts - now| <= tolerance` ⇒ present)
* `FASTR_AV_SYNC_MAX_LATE_MS` — drop threshold (`now - video_pts > max_late` ⇒ drop)
* `FASTR_AV_SYNC_MAX_EARLY_MS` — hold threshold (`video_pts - now > max_early` ⇒ hold + wake later)

Note: the legacy `FASTR_AV_SYNC_*` vars flow through the `RuntimeToggles` mechanism (which only
captures `FASTR_*` keys), but the `FASTRENDER_AVSYNC_*` vars are read directly from the process
environment in `src/media/av_sync.rs`.

Defaults are 20ms / 80ms / 40ms respectively (see `src/media/av_sync.rs`).

---

## Deterministic tests: `VirtualClock` + injectable clocks

Media timing code is notoriously hard to test if it depends on real audio hardware and real wall
time.

The intended test strategy is:

* A `VirtualClock` that only advances when the test tells it to.
  * There is already a pattern for this in `src/js/clock.rs` (`VirtualClock` implementing a `Clock`
    trait).
  * Media clocking is designed to support this via `MediaClock` in `src/media/clock.rs`: production
    code can use `RealAudioDeviceClock` (wall time), while tests can inject a fake/virtual device
    clock (see the `FakeDeviceClock` used in `src/media/clock.rs` unit tests).
* A `NullAudioBackend` (`src/media/audio/null_backend.rs`) is used as a silence/CI fallback when real
  audio output is unavailable.
  * It is driven by an injected monotonic `Clock` (`RealClock` by default; `VirtualClock` in tests).
  * Its `AudioBackend::clock()` implementation advances a simulated output playhead even when no
    samples are queued (silence), which makes it suitable as a stable master clock in deterministic
    A/V sync tests.

Current implementation note: for deterministic tests, construct `NullAudioBackend` with a
`VirtualClock` (see `NullAudioBackend::new_deterministic()` / `NullAudioBackend::new_with_clock(...)`)
and then:

* advance the `VirtualClock` by the desired amount, and
* call `AudioBackend::clock()` (or `NullAudioBackend::pump()`) to advance the simulated playhead.

Alternatively, tests that want to bypass the backend can use `AudioClock::OutputFrames` backed by an
[`InterpolatedAudioClock`](../src/media/audio_clock.rs) and advance it deterministically by calling
`InterpolatedAudioClock::on_callback_end_at(...)` with a captured `Instant` + deterministic offsets,
so the reported time is derived from the known callback frame counts.

With these pieces, a test can:

1. start playback at t=0,
2. advance the `VirtualClock` by exactly 33ms,
3. run one `media.tick()`,
4. assert which frame is selected and what `currentTime` reports,

…with **zero flakiness** and without requiring audio devices in CI.

---

## Known limitations (documented so drift bugs are diagnosable)

### Sample-rate mismatch between decoded stream and audio device

Even when all timestamps are “correct”, real-world playback often encounters a tiny long-term mismatch
between:

* the stream’s nominal sample clock (as implied by container PTS cadence / decoder output), and
* the audio device’s actual consumption clock.

This shows up as **latency creep** (buffer steadily grows) or **underruns** (buffer steadily shrinks)
over long runs. The recommended mitigation is **adaptive resampling**: keep the buffered duration
near a target (e.g. 100ms) by slightly adjusting the effective playback rate (typically within ±1%).

FastRender includes building blocks for this in `src/media/audio/drift.rs`:

* `DriftController` — bounded, slew-limited PI controller that outputs a `playback_rate` multiplier
  based on observed buffered duration.
* `DriftResampler` — a simple drift-aware linear resampler that consumes from `PcmF32QueueConsumer`
  using `base_ratio * playback_rate * user_playback_rate` (where `user_playback_rate` corresponds to
  `HTMLMediaElement.playbackRate`).

### Constant output latency model

Many audio APIs do not provide a precise “samples hit the speaker at time X” timestamp. A common
fallback is to assume a **constant output latency** and treat the backend clock as “frames
written/committed”:

```text
time_heard ≈ (frames_written / sample_rate) - output_latency_constant
```

This has two implications:

* It can introduce a **constant A/V offset** (video consistently early/late by a fixed amount).
* It should **not** create unbounded drift by itself, as long as the audio clock is still derived
  from the device/sample counter, not from UI ticks.

When debugging, distinguish:

* **Offset:** constant error (fix by calibrating latency).
* **Drift:** error grows over time (fix by ensuring a single master clock is used everywhere).

Current implementation note: backends expose a best-effort output-latency estimate via
`AudioOutputInfo::estimated_output_latency`, but `AudioClock::time()` does not apply it
automatically. Treating `AudioClock::time()` as “time heard” is therefore equivalent to assuming
`output_latency_constant = 0`, which can show up as a constant A/V offset. Subtract the estimated
latency (or model preroll via `AudioStreamClock`) when you need a “time heard” estimate.

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
