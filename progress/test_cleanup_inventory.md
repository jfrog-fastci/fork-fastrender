# Test Cleanup Inventory (Phase 1.4)

This file is the durable checklist for the test architecture migration described in
`instructions/test_cleanup.md`.

Update this inventory in every migration PR:
- Change **Status** from `TODO` → `DONE` once the tests have been moved and the old harness/shim is
  deleted (or renamed into its final form).
- If a test’s destination plan changes, update the **Destination** and add a short note explaining
  why.

## Planned end-state `tests/` crate roots

The target is **one** main integration-test binary, plus special binaries only when absolutely
required:

- `tests/integration.rs` — normal integration tests (`mod common; mod api; mod fixtures; mod wpt;`)
- `tests/allocation_failure.rs` — special: `#[global_allocator]` (must be its own binary)
- `tests/isolation.rs` — optional special binary for tests that require process-level isolation
  (env var snapshots / global counters that cannot be made deterministic inside a shared test
  binary). **Goal: keep total `tests/*.rs` ≤ 3.**

## Top-level `tests/*.rs` inventory

This repo is mid-migration; the set of top-level `tests/*.rs` crates changes frequently. Keep this
section in sync with `ls tests/*.rs`.

### Active top-level crates (HEAD)

| File | Type | Destination (new architecture) | Notes | Status |
|---|---|---|---|---|
| `tests/allocation_failure_tests.rs` | special | `tests/allocation_failure.rs` | Contains `#[global_allocator]` (via `tests/allocation_failure/mod.rs`); must remain separate. Rename from `*_tests.rs`. | TODO |
| `tests/animation_tests.rs` | unit | `src/animation/mod.rs` | Not just a harness (large file). Also uses `#[path]` to include `tests/animation/mod.rs`; migrate all to `src/animation/**`. | TODO |
| `tests/border_tests.rs` | unit | `src/style/` | Border parsing + cascade expectations (e.g. `src/style/types.rs`, `src/style/cascade.rs`). | TODO |
| `tests/cascade_tests.rs` | unit | `src/style/cascade.rs` | Harness for `tests/cascade/**`. | TODO |
| `tests/csp_img_data_url.rs` | integration | `tests/integration.rs::api::csp_img_data_url` | End-to-end CSP behavior (data: images). Fold into `tests/api/` + `tests/integration.rs`. | TODO |
| `tests/css_font_feature_values_test.rs` | unit | `src/style/font_feature_values.rs` | Parser-level tests for `@font-feature-values`. | TODO |
| `tests/css_integration_tests.rs` | unit | `src/css/loader.rs` | “Integration” in name only; tests internal CSS loading/URL rewrite/import logic. | TODO |
| `tests/determinism_tests.rs` | integration | `tests/integration.rs::fixtures::determinism` | Renders fixtures repeatedly + compares PNG output. Uses env vars (`FASTR_IN_PROCESS_DETERMINISM_*`) and Rayon scheduling. | TODO |
| `tests/dom_integration_tests.rs` | unit | `src/dom/` | DOM parsing/query/range tests under `tests/dom_integration/**` (split across `src/dom/**` + `src/dom2/**`). | TODO |
| `tests/fixtures_test.rs` | integration | `tests/integration.rs::fixtures::runner` | Reads `tests/fixtures/html/**` and compares against `tests/fixtures/golden/**`; mutates env (`FASTR_USE_BUNDLED_FONTS`, `UPDATE_GOLDEN`). | TODO |
| `tests/font_tests.rs` | unit | `src/text/` | Font loader/resolver/shaping + font-related style parsing (`src/text/**`, some `src/style/**`). | TODO |
| `tests/fuzz_corpus_smoke_test.rs` | integration | `tests/integration.rs::tooling::fuzz_corpus_smoke` | Drives the renderer against checked-in `tests/fuzz_corpus/**` inputs with tight timeouts. | TODO |
| `tests/iframe_tests.rs` | integration | `tests/integration.rs::fixtures::iframe` | Golden-image comparisons; sets `FASTR_USE_BUNDLED_FONTS`; uses file URLs/tempdirs. | TODO |
| `tests/image_integration_tests.rs` | integration | `tests/integration.rs::api::image_integration` | Image loading/output/CORS integration tests under `tests/image_integration/**`. | TODO |
| `tests/integration.rs` | integration | keep | Unified integration test binary. Should become the default home for remaining integration suites. | DONE |
| `tests/js_harness_tests.rs` | unit | `src/js/` | Large internal JS harness (event loop, timers, DOM integration). Likely lands as `#[cfg(test)]` submodules under `src/js/**`. | TODO |
| `tests/js_html_integration.rs` | integration | `tests/integration.rs::api::js_html_integration` | End-to-end HTML+JS execution behavior via `BrowserTab` and `EventLoop`. Fold into `tests/api/`. | TODO |
| `tests/layout_tests.rs` | unit | `src/layout/` | Harness for `tests/layout/**` (very large). | TODO |
| `tests/legacy_tests.rs` | unit | `src/paint/` | Legacy rendering regressions; many tests call paint internals (`paint_tree_with_resources_*`, `ImageCache`, etc.). | TODO |
| `tests/misc_tests.rs` | unit | `src/**` (split) | Mixed grab-bag; many tests mutate env/process-wide knobs. Repo-only guardrails may stay under `tests/integration.rs`. | TODO |
| `tests/paged_media.rs` | unit | `src/layout/pagination.rs` | Dedicated paged-media regression target; remove standalone binary after moving into `src/layout/**`. | TODO |
| `tests/paint_tests.rs` | unit | `src/paint/` | Paint/backdrop/ref-image tests under `tests/paint/**`, `tests/backdrop/**`. | TODO |
| `tests/progress_tests.rs` | integration | `tests/integration.rs::fixtures::progress` | Repo artifact guardrail tests for `progress/pages/*.json` (no library code). | TODO |
| `tests/quirks_body_percent_height_tests.rs` | integration | `tests/integration.rs::api::quirks` | End-to-end render regression; spawns large-stack thread. | TODO |
| `tests/ref_tests.rs` | integration | `tests/integration.rs::common::ref` | Reference image diff/test harness tests; serializes env var changes via mutex. | TODO |
| `tests/render_tests.rs` | integration | `tests/integration.rs::api::render` | End-to-end rendering APIs (`render_url`, diagnostics, timeouts, etc.). | TODO |
| `tests/scroll_tests.rs` | unit | `src/scroll.rs` | Scroll state/behavior tests under `tests/scroll/**`. | TODO |
| `tests/style_tests.rs` | unit | `src/style/` | Large style regression suite under `tests/style/**` (some use ref-image utilities). | TODO |
| `tests/text_tests.rs` | unit | `src/text/` | Text/shaping regressions under `tests/text/**`. | TODO |
| `tests/tree_tests.rs` | unit | `src/tree/` | Box/tree generation tests under `tests/tree/**`. | TODO |
| `tests/ui_tests.rs` | integration | `tests/integration.rs::api::ui` | Browser UI protocol tests under `tests/ui/**` (feature-gated in places). | TODO |
| `tests/user_agent_placeholder_pseudo_test.rs` | unit | `src/style/color.rs` | Placeholder pseudo default color (`GrayText`) regression; uses cascade helpers. | TODO |
| `tests/var_tests.rs` | unit | `src/style/var_resolution.rs` | CSS custom property (`var()`) resolution tests. | TODO |
| `tests/weibo_web_font_relative_url_test.rs` | unit | `src/text/font_loader.rs` | Uses fixtures; ensures `@font-face url(...)` in inline styles resolves against document base URL. | TODO |
| `tests/wpt_offline_invariants_test.rs` | integration | `tests/integration.rs::wpt::offline_invariants` | Repo invariant: WPT corpus should remain offline-only. Fold into `tests/wpt/` + `tests/integration.rs`. | TODO |

### Completed (top-level crate removed)

| File | Type | Destination (new architecture) | Notes | Status |
|---|---|---|---|---|
| `tests/accessibility_tests.rs` | integration | `tests/integration.rs::accessibility` | Top-level harness removed; suite now lives under `tests/accessibility/**` and uses `tests/common/accessibility`. | DONE |
| `tests/bin_tests.rs` | integration | `tests/integration.rs::bin` | Top-level harness removed; suite now lives under `tests/bin/**`. | DONE |
| `tests/browser_integration_tests.rs` | integration | `tests/integration.rs::browser_integration` | Top-level harness removed and consolidated into `tests/integration.rs`. Browser integration no longer mutates process-wide env vars at init; tests needing serialization use `stage_listener_test_lock()` / `common::global_test_lock()`. | DONE |
| `tests/browser_tab_render_interleaving.rs` | integration | `tests/integration.rs::browser_integration::browser_tab_render_interleaving` | Moved into `tests/browser_integration/browser_tab_render_interleaving.rs`. | DONE |
| `tests/bundle_vary_manifest_key_test.rs` | unit | `src/resource/bundle.rs` | Moved into bundle module unit tests. | DONE |
| `tests/bundled_tests.rs` | integration | `tests/integration.rs::bundled` | Top-level harness removed; suite now lives under `tests/bundled/**`. | DONE |
| `tests/calc_percent_height_is_not_collapsible_through.rs` | delete | delete | Pure `#[path]` shim removed; test remains under `tests/layout/**`. | DONE |
| `tests/colr_tests.rs` | unit | `src/text/color_fonts/golden_tests.rs` | Migrated to unit tests to directly exercise COLR rasterization. | DONE |
| `tests/container_scroll_state_queries_test.rs` | delete | delete | Pure `#[path]` shim removed; test remains under `tests/style/**`. | DONE |
| `tests/container_style_queries.rs` | delete | delete | Pure `#[path]` shim removed; test remains under `tests/style/**`. | DONE |
| `tests/content_visibility_tests.rs` | unit | `src/layout/contexts/*` | Migrated into layout context unit tests (block/flex/grid). | DONE |
| `tests/clip_tests.rs` | unit | `src/paint/display_list_builder.rs` | Migrated clip-rect regression coverage into `src/paint/display_list_builder.rs` unit tests and removed `tests/clip_tests.rs`/`tests/clip/**`. | DONE |
| `tests/display_list_tests.rs` | unit | `src/paint/display_list_renderer/tests/display_list/mod.rs` | Migrated the display-list backend regression suite into unit tests under `src/paint/display_list_renderer/tests/display_list/**` and removed the standalone test binary. | DONE |
| `tests/flex_nowrap_negative_margins_do_not_trigger_monotonic_fallback.rs` | delete | delete | Pure `#[path]` shim removed; test remains under `tests/layout/**`. | DONE |
| `tests/flex_wrap_order_does_not_trigger_manual_placement.rs` | delete | delete | Pure `#[path]` shim removed; test remains under `tests/layout/**`. | DONE |
| `tests/grid_tests.rs` | unit | `src/layout/contexts/grid.rs` | Migrated to grid context unit tests (and `tests/grid/**` directory removed). | DONE |
| `tests/headless_chrome_media_features_test.rs` | integration | `tests/integration.rs::browser_integration::headless_chrome_media_features` | Moved into `tests/browser_integration/headless_chrome_media_features.rs`. | DONE |
| `tests/html_script_processing.rs` | unit | `src/js/html_classic_scripts.rs` | Migrated to unit tests for `parse_and_run_classic_scripts`. | DONE |
| `tests/interaction.rs` | delete | delete | Standalone interaction harness removed; suite now lives under `tests/interaction/**` and is pulled into `tests/integration.rs::interaction`. Long-term goal: migrate to unit tests under `src/interaction/**`. | DONE |
| `tests/js_webidl_union_record_enum.rs` | unit | `src/js/webidl/bindings/webidl_union_record_tests.rs` | Migrated to unit tests alongside WebIDL bindings. | DONE |
| `tests/llvm_statepoint_stackmap_llvm18.rs` | integration | `tests/integration.rs::tooling::llvm_stackmaps` | Moved into `tests/tooling/llvm_stackmaps.rs` (requires LLVM 18 tools; skips when missing). | DONE |
| `tests/overflow_tests.rs` | unit | `src/paint/stacking.rs` | Migrated into `src/paint/stacking/tests/**`. | DONE |
| `tests/pipeline_churn_guardrail.rs` | unit | `src/layout/tests/pipeline_churn_guardrail.rs` | Migrated into unit tests under `src/layout/tests/**`; uses `crate::testing::global_test_lock()` to keep counter-reset assertions deterministic, so a dedicated binary is no longer required. | DONE |
| `tests/regression_tests.rs` | integration | `tests/integration.rs::regression` | Top-level harness removed; suite now lives under `tests/regression/**`. | DONE |
| `tests/render_control_test_render_delay_smoke.rs` | integration | `tests/integration.rs::api::render_control` | Moved into `tests/api/render_control.rs`. | DONE |
| `tests/resource_tests.rs` | integration | `tests/integration.rs::resource` | Top-level harness removed; suite now lives under `tests/resource/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/shadow_tests.rs` | unit | `src/dom2/shadow_dom.rs` | Migrated to unit tests for declarative shadow DOM + slotting. | DONE |
| `tests/svg_integration_tests.rs` | unit | `src/image_loader/tests.rs` + `src/paint/svg_filter/tests/**` | Migrated SVG rasterization + filter tests into unit tests and removed the standalone test binary. | DONE |
| `tests/taffy_cache_tests.rs` | unit | `src/layout/taffy_integration.rs` | Migrated to unit tests; old `tests/taffy_cache/**` directory removed. | DONE |
| `tests/wpt_test.rs` | integration | `tests/wpt/tests.rs` | Top-level harness removed; runner tests now live under `tests/wpt/**` (must be wired into `tests/integration.rs`). | DONE |

## `tests/` subdirectory summary (first pass)

This is a directory-level view to help catch “stray” test code and harnesses during parallel
migrations.

| Directory | Current contents | Target | Notes |
|---|---|---|---|
| `tests/api/` | public API integration tests | `tests/integration.rs::api` | Must only use public API. |
| `tests/accessibility/` | accessibility/accname fixtures + assertions | `tests/integration.rs::accessibility` | Public API + fixture-driven; stays in integration. |
| `tests/allocation_failure/` | OOM + custom allocator harness | `tests/allocation_failure.rs` | Must stay separate due to `#[global_allocator]`. |
| `tests/animation/` | animation engine tests | `src/animation/` | Unit tests; `tests/animation_tests.rs` also contains top-level test code. |
| `tests/bin/` | CLI/binary tests | `tests/integration.rs::bin` | Keep as integration tests; share net/fs helpers via `tests/common/`. |
| `tests/browser_integration/` | browser/UI worker integration suite | `tests/integration.rs::browser_integration` | Runs in the shared integration binary; avoid process-init env mutation. Tests that touch global state should serialize via `stage_listener_test_lock()` / `common::global_test_lock()`. |
| `tests/bundled/` | bundled font fixture tests | `tests/integration.rs::bundled` | Integration-style fixture assertions. |
| `tests/common/` | shared helpers for integration tests | keep (not a binary) | Replaces the old `tests/test_support/**` helpers. |
| `tests/css_integration/` | css loader/import/url rewrite tests | `src/css/loader.rs` (+ friends) | Despite name, these are mostly unit tests. |
| `tests/fuzz_corpus/` | checked-in corpus inputs for smoke testing | `tests/integration.rs::tooling::fuzz_corpus_smoke` | Exercised by `tests/fuzz_corpus_smoke_test.rs`. |
| `tests/dom_integration/` | DOM parsing/query integration tests | `src/dom/**` + `src/dom2/**` | Unit tests. |
| `tests/fixtures/` | HTML + golden-image fixtures | `tests/integration.rs::fixtures` | Stays in `tests/` (data-driven integration). |
| `tests/guards/` | repo invariants / consolidation guards | `tests/integration.rs::guards` | Integration-style checks for repo structure. |
| `tests/image_integration/` | image loading/output integration tests | `tests/integration.rs::api::image_integration` | Network/CORS/streaming output; stays integration. |
| `tests/interaction/` | interaction engine tests | `src/interaction/**` | Harness removed; suite is pulled into `tests/integration.rs::interaction` for now, but should eventually migrate to unit tests in `src/interaction/**`. |
| `tests/js/` | JS subsystem integration tests | `tests/integration.rs::js` | Consolidated into the shared integration binary. |
| `tests/layout/` | layout regressions, paging, flex/grid/table, etc | `src/layout/**` | Unit tests (bulk of migration). |
| `tests/paint/` + `tests/backdrop/` | paint/backdrop filter/render pipeline tests | `src/paint/**` | Unit tests; move shared Rayon init helper to `src/test_utils` or `tests/common`. |
| `tests/progress/` | guardrails for committed `progress/` artifacts | `tests/integration.rs::fixtures::progress` | Not library tests; keep in integration. |
| `tests/ref/` | image diff + ref-test harness utilities | `tests/common/` | Not a binary; used by fixtures/determinism/etc. (may be renamed/moved). |
| `tests/regression/` | cross-cutting regressions | `src/**` (split) | Many unit tests; some may remain fixture-driven integration tests. |
| `tests/resource/` | resource fetching/cache/CORS tests | `tests/integration.rs::resource` | Uses net harness; stays integration for now. |
| `tests/style/` | cascade/values/layout-affecting style regressions | `src/style/**` | Unit tests. |
| `tests/text/` | shaping/text regressions | `src/text/**` | Unit tests. |
| `tests/tooling/` | external tool integration (e.g. LLVM stackmaps) | `tests/integration.rs::tooling` | Tests that shell out to toolchains; should skip when tools absent. |
| `tests/tree/` | box/tree generation regressions | `src/tree/**` | Unit tests. |
| `tests/ui/` | browser UI protocol tests | `tests/integration.rs::api::ui` | Integration tests (feature-gated). |
| `tests/wpt/` + `tests/wpt_dom/` | WPT runners + fixtures | `tests/integration.rs::wpt` | Stays in `tests/` (fixture-driven integration). |

## End-state invariants to verify

- `ls tests/*.rs | wc -l` is **≤ 3**
- No `#[path = "..."]` in `tests/` (shims removed): `rg '#\\[path\\s*=' tests/` returns nothing
- No internal-module imports in `tests/` (integration tests use public API only)
