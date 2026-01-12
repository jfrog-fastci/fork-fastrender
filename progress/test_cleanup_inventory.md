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

- `tests/integration.rs` — one integration test binary (`mod common; mod api; mod fixtures; mod wpt;`)
- `tests/allocation_failure.rs` — special: `#[global_allocator]` (must be its own binary)

**Goal: keep total `tests/*.rs` at 2.** Any additional test binaries must be justified and treated
as temporary exceptions.

## Top-level `tests/*.rs` inventory

This repo is mid-migration; the set of top-level `tests/*.rs` crates changes frequently. Keep this
section in sync with `ls tests/*.rs`.

### Active top-level crates (HEAD)

| File | Type | Destination (new architecture) | Notes | Status |
|---|---|---|---|---|
| `tests/allocation_failure.rs` | special | keep | Contains `#[global_allocator]` (via `tests/allocation_failure/mod.rs`); must remain separate. | DONE |
| `tests/browser_integration_tests.rs` | integration (shim) | delete | Temporary compatibility shim for `--test browser_integration_tests` (extra binary). Real suite: `tests/integration.rs::browser_integration` (`--features browser_ui`). Remove once automation/docs use `cargo test --features browser_ui --test integration` to satisfy the [end-state invariant](#end-state-invariants-to-verify) <code>ls tests/*.rs &#124; wc -l == 2</code>. | TODO |
| `tests/integration.rs` | integration | keep | Unified integration test binary. Should become the default home for remaining integration suites. | DONE |

### Completed (top-level crate removed)

| File | Type | Destination (new architecture) | Notes | Status |
|---|---|---|---|---|
| `tests/accessibility_tests.rs` | integration | `tests/integration.rs::accessibility` | Top-level harness removed; suite now lives under `tests/accessibility/**` and uses `tests/common/accessibility`. | DONE |
| `tests/bin_tests.rs` | integration | `tests/integration.rs::bin` | Top-level harness removed; suite now lives under `tests/bin/**`. | DONE |
| `tests/browser_tab_render_interleaving.rs` | integration | `tests/integration.rs::browser_integration::browser_tab_render_interleaving` | Moved into `tests/browser_integration/browser_tab_render_interleaving.rs`. | DONE |
| `tests/` `image_integration` `_tests.rs` | delete | delete | Backwards-compat harness for `tests/` `image_integration/**`; redundant with `tests/integration.rs::image_integration`. | DONE |
| `tests/session_autosave.rs` | integration | `tests/integration.rs::ui::session_autosave` | Moved into `tests/ui/session_autosave.rs` (feature-gated: `browser_ui`). | DONE |
| `tests/svg_integration.rs` | delete | delete | No-op placeholder integration-test crate; removed to avoid an extra test binary. | DONE |
| `tests/bundle_vary_manifest_key_test.rs` | unit | `src/resource/bundle.rs` | Moved into bundle module unit tests. | DONE |
| `tests/bundled_tests.rs` | integration | `tests/integration.rs::bundled` | Top-level harness removed; suite now lives under `tests/bundled/**`. | DONE |
| `tests/calc_percent_height_is_not_collapsible_through.rs` | delete | delete | Pure `#[path]` shim removed; test remains under `tests/layout/**`. | DONE |
| `tests/colr_tests.rs` | unit | `src/text/color_fonts/golden_tests.rs` | Migrated to unit tests to directly exercise COLR rasterization. | DONE |
| `tests/container_scroll_state_queries_test.rs` | delete | delete | Pure `#[path]` shim removed; corresponding tests now live under `src/style/tests/style/**`. | DONE |
| `tests/container_style_queries.rs` | delete | delete | Pure `#[path]` shim removed; corresponding tests now live under `src/style/tests/style/**`. | DONE |
| `tests/content_visibility_tests.rs` | unit | `src/layout/contexts/*` | Migrated into layout context unit tests (block/flex/grid). | DONE |
| `tests/clip_tests.rs` | unit | `src/paint/display_list_builder.rs` | Migrated clip-rect regression coverage into `src/paint/display_list_builder.rs` unit tests and removed `tests/clip_tests.rs`/`tests/clip/**`. | DONE |
| `tests/display_list_tests.rs` | unit | `src/paint/display_list_renderer/tests/display_list/mod.rs` | Migrated the display-list backend regression suite into unit tests under `src/paint/display_list_renderer/tests/display_list/**` and removed the standalone test binary. | DONE |
| `tests/border_tests.rs` | delete | `src/style/tests/border/` | Top-level harness removed; suite moved out of `tests/` into lib unit tests. | DONE |
| `tests/cascade_tests.rs` | delete | `src/style/tests/cascade/` | Top-level harness removed; suite moved out of `tests/` into lib unit tests. | DONE |
| `tests/css_integration_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/css_integration/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/determinism_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/determinism/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/dom_integration_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/dom_integration/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/font_tests.rs` | delete | `src/text/tests/font/` | Top-level harness removed; suite moved out of `tests/` into `src/text/tests/font/**` unit tests. | DONE |
| `tests/js_harness_tests.rs` | unit | `src/js/vmjs/window_timers.rs` + `src/js/vmjs/window.rs` | Removed `tests/js_harness_tests.rs` and the old `tests/js_harness/` harness directory; coverage migrated into VM/Window unit tests (timers + window APIs). | DONE |
| `tests/layout_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/layout/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/legacy_tests.rs` | unit | `src/paint/tests/legacy/**` | Migrated the legacy paint backend regression suite into unit tests and removed the standalone test binary + legacy harness modules. | DONE |
| `tests/misc_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/misc/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/paint_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/paint/**` (and friends) and is pulled into `tests/integration.rs`. | DONE |
| `tests/progress_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/progress/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/ref_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/ref/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/render_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/render/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/scroll_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/scroll/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/style_tests.rs` | delete | `src/style/tests/style/` | Top-level harness removed; suite moved out of `tests/` into lib unit tests. | DONE |
| `tests/property_parser_no_panic_regression_test.rs` | delete | `src/style/tests/style/property_parser_no_panic_regression_test.rs` | Pure `#[path]` shim removed; tests now run as lib unit tests under `src/style/tests/style/`. | DONE |
| `tests/css_font_feature_values_test.rs` | unit | `src/style/tests/style/css_font_feature_values_test.rs` | Top-level crate removed; test now runs as a lib unit test under `src/style/tests/style/`. | DONE |
| `tests/user_agent_placeholder_pseudo_test.rs` | unit | `src/style/tests/style/user_agent_placeholder_pseudo_test.rs` | Top-level crate removed; test now runs as a lib unit test under `src/style/tests/style/`. | DONE |
| `tests/paged_media.rs` | unit | `src/layout/tests/paged_media.rs` | Top-level crate removed; tests migrated into layout unit tests (`src/layout/tests/`). | DONE |
| `tests/js_html_integration.rs` | integration | `tests/integration.rs::js::js_html_integration` | Moved into `tests/js/js_html_integration.rs` and included from `tests/js/mod.rs`. | DONE |
| `tests/text_tests.rs` | delete | `src/text/tests/` | Top-level harness removed; suite moved out of `tests/` into `src/text/tests/**` unit tests. | DONE |
| `tests/tree_tests.rs` | delete | delete | Top-level harness removed; tree/box generation tests migrated to unit tests under `src/tree/**`. | DONE |
| `tests/ui_tests.rs` | delete | delete | Top-level harness removed; suite now lives under `tests/ui/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/var_tests.rs` | delete | `src/style/tests/var/` | Top-level harness removed; suite moved out of `tests/` into lib unit tests. | DONE |
| `tests/weibo_web_font_relative_url_test.rs` | unit | `src/text/tests/weibo_web_font_relative_url_test.rs` | Migrated to unit tests under `src/text/tests/**`. | DONE |
| `tests/flex_nowrap_negative_margins_do_not_trigger_monotonic_fallback.rs` | delete | delete | Pure `#[path]` shim removed; test remains under `tests/layout/**`. | DONE |
| `tests/flex_wrap_order_does_not_trigger_manual_placement.rs` | delete | delete | Pure `#[path]` shim removed; test remains under `tests/layout/**`. | DONE |
| `tests/fuzz_corpus_smoke_test.rs` | integration | `tests/integration.rs::tooling::fuzz_corpus_smoke` | Moved into `tests/tooling/fuzz_corpus_smoke.rs` and included from `tests/tooling/mod.rs`. | DONE |
| `tests/grid_tests.rs` | unit | `src/layout/contexts/grid.rs` | Migrated to grid context unit tests (and `tests/grid/**` directory removed). | DONE |
| `tests/headless_chrome_media_features_test.rs` | integration | `tests/integration.rs::browser_integration::headless_chrome_media_features` | Moved into `tests/browser_integration/headless_chrome_media_features.rs`. | DONE |
| `tests/html_script_processing.rs` | unit | `src/js/html_classic_scripts.rs` | Migrated to unit tests for `parse_and_run_classic_scripts`. | DONE |
| `tests/interaction.rs` | delete | delete | Standalone interaction harness removed; suite now lives under `tests/interaction/**` and is pulled into `tests/integration.rs::interaction`. Long-term goal: migrate to unit tests under `src/interaction/**`. | DONE |
| `tests/js_webidl_union_record_enum.rs` | unit | `src/js/webidl/bindings/webidl_union_record_tests.rs` | Migrated to unit tests alongside WebIDL bindings. | DONE |
| `tests/llvm_statepoint_stackmap_llvm18.rs` | integration | `tests/integration.rs::tooling::llvm_stackmaps` | Moved into `tests/tooling/llvm_stackmaps.rs` (requires LLVM 18 tools; skips when missing). | DONE |
| `tests/overflow_tests.rs` | unit | `src/paint/stacking.rs` | Migrated into `src/paint/stacking/tests/**`. | DONE |
| `tests/pipeline_churn_guardrail.rs` | unit | `src/layout/tests/pipeline_churn_guardrail.rs` | Migrated into unit tests under `src/layout/tests/**`; uses `crate::testing::global_test_lock()` to keep counter-reset assertions deterministic, so a dedicated binary is no longer required. | DONE |
| `tests/regression_tests.rs` | integration | `tests/integration.rs::regression` | Top-level harness removed; suite now lives under `tests/regression/**`. | DONE |
| `tests/csp_img_data_url.rs` | integration | `tests/api/csp_img_data_url.rs` | Runs via `tests/integration.rs::api`. | DONE |
| `tests/quirks_body_percent_height_tests.rs` | integration | `tests/integration.rs::api::quirks` | Moved into `tests/api/quirks_body_percent_height.rs` (uses the shared large-stack helper). | DONE |
| `tests/render_control_test_render_delay_smoke.rs` | integration | `tests/integration.rs::api::render_control` | Moved into `tests/api/render_control.rs`. | DONE |
| `tests/resource_tests.rs` | integration | `tests/integration.rs::resource` | Top-level harness removed; suite now lives under `tests/resource/**` and is pulled into `tests/integration.rs`. | DONE |
| `tests/shadow_tests.rs` | unit | `src/dom2/shadow_dom.rs` | Migrated to unit tests for declarative shadow DOM + slotting. | DONE |
| `tests/svg_integration_tests.rs` | unit | `src/image_loader/tests.rs` + `src/paint/svg_filter/tests/**` | Migrated SVG rasterization + filter tests into unit tests and removed the standalone test binary. | DONE |
| `tests/taffy_cache_tests.rs` | unit | `src/layout/taffy_integration.rs` | Migrated to unit tests; old `tests/taffy_cache/**` directory removed. | DONE |
| `tests/wpt_test.rs` | integration | `tests/wpt/tests.rs` | Top-level harness removed; runner tests now live under `tests/wpt/**` (must be wired into `tests/integration.rs`). | DONE |
| `tests/wpt_offline_invariants_test.rs` | delete | delete | Top-level harness removed; offline invariants now live under `tests/wpt/offline_invariants.rs` and run via `tests/integration.rs::wpt`. | DONE |

## `tests/` subdirectory summary (first pass)

This is a directory-level view to help catch “stray” test code and harnesses during parallel
migrations.

| Directory | Current contents | Target | Notes |
|---|---|---|---|
| `tests/api/` | public API integration tests | `tests/integration.rs::api` | Must only use public API. |
| `tests/accessibility/` | accessibility/accname fixtures + assertions | `tests/integration.rs::accessibility` | Public API + fixture-driven; stays in integration. |
| `tests/allocation_failure/` | OOM + custom allocator harness | `tests/allocation_failure.rs` | Must stay separate due to `#[global_allocator]`. |
| `tests/animation/` | animation engine tests | `src/animation/` | Currently not wired into `tests/integration.rs`; ensure these tests are either migrated into `src/animation/**` unit tests or included from the integration harness. |
| `tests/bin/` | CLI/binary tests | `tests/integration.rs::bin` | Keep as integration tests; share net/fs helpers via `tests/common/`. |
| `tests/browser_integration/` | browser/UI worker integration suite | `tests/integration.rs::browser_integration` | Runs in the shared integration binary; avoid process-init env mutation. Tests that touch global state should serialize via `stage_listener_test_lock()` / `common::global_test_lock()`. |
| `tests/bundled/` | bundled font fixture tests | `tests/integration.rs::bundled` | Integration-style fixture assertions. |
| `tests/common/` | shared helpers for integration tests | keep (not a binary) | Replaces the old `tests/test_support/**` helpers. |
| `tests/css_integration/` | css loader/import/url rewrite tests | `src/css/loader.rs` (+ friends) | Despite name, these are mostly unit tests. |
| `tests/fuzz_corpus/` | checked-in corpus inputs for smoke testing | `tests/integration.rs::tooling::fuzz_corpus_smoke` | Exercised by `tests/tooling/fuzz_corpus_smoke.rs` (via `tests/integration.rs`). |
| `tests/dom_integration/` | DOM parsing/query integration tests | `src/dom/**` + `src/dom2/**` | Unit tests. |
| `tests/fixtures/` | HTML + golden-image fixtures | `tests/integration.rs::fixtures` | Stays in `tests/` (data-driven integration). |
| `tests/guards/` | repo invariants / consolidation guards | `tests/integration.rs::guards` | Integration-style checks for repo structure. |
| `tests/` `image_integration/` | image loading/output integration tests | `tests/integration.rs::image_integration` | Network/CORS/streaming output; stays integration. |
| `tests/interaction/` | interaction engine tests | `src/interaction/**` | Harness removed; suite is pulled into `tests/integration.rs::interaction` for now, but should eventually migrate to unit tests in `src/interaction/**`. |
| `tests/js/` | JS subsystem integration tests | `tests/integration.rs::js` | Consolidated into the shared integration binary. |
| `tests/layout/` | layout regressions, paging, flex/grid/table, etc | `src/layout/**` | Unit tests (bulk of migration). |
| `tests/misc/` | grab-bag integration tests (legacy bucket) | `tests/integration.rs::misc` + migrate unit tests into `src/**` | Some internal/unit tests have been migrated out already (e.g. composed DOM snapshotting + exportparts algorithm tests are now unit tests in `src/dom.rs`; old files are stubs). |
| `tests/paint/` + `tests/backdrop/` | paint/backdrop filter/render pipeline tests | `src/paint/**` | Unit tests; move shared Rayon init helper to `src/test_utils` or `tests/common`. |
| `tests/progress/` | guardrails for committed `progress/` artifacts | `tests/integration.rs::progress` | Not library tests; keep in integration. |
| `tests/ref/` | image diff + ref-test harness utilities | `tests/common/` | Not a binary; used by fixtures/determinism/etc. (may be renamed/moved). |
| `tests/regression/` | cross-cutting regressions | `src/**` (split) | Many unit tests; some may remain fixture-driven integration tests. |
| `tests/resource/` | resource fetching/cache/CORS tests | `tests/integration.rs::resource` | Uses net harness; stays integration for now. |
| `tests/style/` | migrated (directory removed) | `src/style/**` | Suite moved into `src/style/tests/**` (unit tests). |
| `tests/text/` | migrated (directory removed) | `src/text/**` | Suite moved into `src/text/tests/**` (unit tests). |
| `tests/tooling/` | external tool integration (e.g. LLVM stackmaps) | `tests/integration.rs::tooling` | Tests that shell out to toolchains; should skip when tools absent. |
| `tests/tree/` | migrated (stub module remains) | `src/tree/**` | Suite migrated to unit tests; `tests/tree/mod.rs` is currently an empty module referenced by `tests/integration.rs`. |
| `tests/ui/` | browser UI protocol tests | `tests/integration.rs::ui` | Integration tests (feature-gated). |
| `tests/wpt/` + `tests/wpt_dom/` | WPT runners + fixtures | `tests/integration.rs::wpt` | Stays in `tests/` (fixture-driven integration). |

## End-state invariants to verify

- `ls tests/*.rs | wc -l` is **2**
- No `#[path = "..."]` in `tests/` (shims removed): `rg '#\\[path\\s*=' tests/` returns nothing
- No internal-module imports in `tests/` (integration tests use public API only)
