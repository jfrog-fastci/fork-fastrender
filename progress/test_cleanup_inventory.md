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

## Top-level `tests/*.rs` inventory (60 files)

| File | Type | Destination (new architecture) | Notes | Status |
|---|---|---|---|---|
| `tests/accessibility_tests.rs` | unit | `src/accessibility.rs` | Harness for `tests/accessibility/**`; delete harness after move. | TODO |
| `tests/allocation_failure_tests.rs` | special | `tests/allocation_failure.rs` | Contains `#[global_allocator]` (via `tests/allocation_failure/mod.rs`); must remain separate. Rename from `*_tests.rs`. | TODO |
| `tests/animation_tests.rs` | unit | `src/animation/mod.rs` | Not just a harness (large file). Also uses `#[path]` to include `tests/animation/mod.rs`; migrate all to `src/animation/**`. | TODO |
| `tests/bin_tests.rs` | integration | `tests/integration.rs::api::cli` | CLI/binary tests under `tests/bin/**`; currently pulls in `tests/test_support`. | TODO |
| `tests/border_tests.rs` | unit | `src/style/` | Border parsing + cascade expectations (e.g. `src/style/types.rs`, `src/style/cascade.rs`). | TODO |
| `tests/browser_integration_tests.rs` | integration | `tests/integration.rs::browser_integration` | Consolidated into the shared `tests/integration.rs` binary (deleted the standalone harness). Browser integration no longer mutates process-wide env vars at init; tests needing serialization use `stage_listener_test_lock()` / `common::global_test_lock()`. | DONE |
| `tests/browser_tab_render_interleaving.rs` | integration | `tests/integration.rs::api::browser_tab_render_interleaving` | Uses public browser/tab APIs + JS event loop scheduling. | TODO |
| `tests/bundle_vary_manifest_key_test.rs` | unit | `src/resource/bundle.rs` | Bundle manifest synthetic `Vary` key behavior. | TODO |
| `tests/bundled_tests.rs` | unit | `src/text/font_loader.rs` | Bundled font fixture integrity/coverage (`FontContext::with_config(FontConfig::bundled_only())`). | TODO |
| `tests/calc_percent_height_is_not_collapsible_through.rs` | delete | delete | Pure `#[path]` shim for `tests/layout/calc_percent_height_is_not_collapsible_through.rs`. | TODO |
| `tests/cascade_tests.rs` | unit | `src/style/cascade.rs` | Harness for `tests/cascade/**`. | TODO |
| `tests/clip_tests.rs` | unit | `src/paint/clip_path.rs` | Clip/stacking-context regressions under `tests/clip/**`. | TODO |
| `tests/colr_tests.rs` | unit | `src/text/color_fonts/` | Color-font (COLR/CPAL) rendering; depends on fixture fonts + reference-image compare utilities. | TODO |
| `tests/container_scroll_state_queries_test.rs` | delete | delete | Pure `#[path]` shim for `tests/style/container_scroll_state_queries_test.rs`. | TODO |
| `tests/container_style_queries.rs` | delete | delete | Pure `#[path]` shim for `tests/style/container_style_queries.rs`. | TODO |
| `tests/content_visibility_tests.rs` | unit | `src/layout/contexts/block/mod.rs` | Internal layout test using `BlockFormattingContext`, `ComputedStyle`, etc. | TODO |
| `tests/css_font_feature_values_test.rs` | unit | `src/style/font_feature_values.rs` | Parser-level tests for `@font-feature-values`. | TODO |
| `tests/css_integration_tests.rs` | unit | `src/css/loader.rs` | “Integration” in name only; tests internal CSS loading/URL rewrite/import logic. | TODO |
| `tests/determinism_tests.rs` | integration | `tests/integration.rs::fixtures::determinism` | Renders fixtures repeatedly + compares PNG output. Uses env vars (`FASTR_IN_PROCESS_DETERMINISM_*`) and Rayon scheduling. | TODO |
| `tests/display_list_tests.rs` | unit | `src/paint/display_list.rs` | Display-list builder/renderer internals under `tests/display_list/**`. | TODO |
| `tests/dom_integration_tests.rs` | unit | `src/dom/` | DOM parsing/query/range tests under `tests/dom_integration/**` (split across `src/dom/**` + `src/dom2/**`). | TODO |
| `tests/fixtures_test.rs` | integration | `tests/integration.rs::fixtures::runner` | Reads `tests/fixtures/html/**` and compares against `tests/fixtures/golden/**`; mutates env (`FASTR_USE_BUNDLED_FONTS`, `UPDATE_GOLDEN`). | TODO |
| `tests/flex_nowrap_negative_margins_do_not_trigger_monotonic_fallback.rs` | delete | delete | Pure `#[path]` shim for `tests/layout/flex_nowrap_negative_margins_do_not_trigger_monotonic_fallback.rs`. | TODO |
| `tests/flex_wrap_order_does_not_trigger_manual_placement.rs` | delete | delete | Pure `#[path]` shim for `tests/layout/flex_wrap_order_does_not_trigger_manual_placement.rs`. | TODO |
| `tests/font_tests.rs` | unit | `src/text/` | Font loader/resolver/shaping + font-related style parsing (`src/text/**`, some `src/style/**`). | TODO |
| `tests/grid_tests.rs` | unit | `src/layout/contexts/grid.rs` | Grid layout algorithm tests under `tests/grid/**`. | TODO |
| `tests/headless_chrome_media_features_test.rs` | integration | `tests/integration.rs::api::media_features` | End-to-end render; uses `tests/paint/rayon_test_util.rs` and pins parallelism for determinism. | TODO |
| `tests/html_script_processing.rs` | unit | `src/js/html_classic_scripts.rs` | Tests `parse_and_run_classic_scripts` scheduling/fetch semantics. | TODO |
| `tests/iframe_tests.rs` | integration | `tests/integration.rs::fixtures::iframe` | Golden-image comparisons; sets `FASTR_USE_BUNDLED_FONTS`; uses file URLs/tempdirs. | TODO |
| `tests/image_integration_tests.rs` | integration | `tests/integration.rs::api::image_integration` | Image loading/output/CORS integration; uses `tests/test_support/net`. | TODO |
| `tests/interaction.rs` | unit | `src/interaction/` | Internal interaction engine tests. Current harness uses `#[path]` due to name collision (`tests/interaction.rs` vs `tests/interaction/mod.rs`). | TODO |
| `tests/js_harness_tests.rs` | unit | `src/js/` | Large internal JS harness (event loop, timers, DOM integration). Likely lands as `#[cfg(test)]` submodules under `src/js/**`. | TODO |
| `tests/js_webidl_union_record_enum.rs` | unit | `src/js/webidl/` | vm-js WebIDL union/record/enum conversion tests (`src/js/webidl/conversions.rs` + bindings). | TODO |
| `tests/layout_tests.rs` | unit | `src/layout/` | Harness for `tests/layout/**` (very large). | TODO |
| `tests/legacy_tests.rs` | unit | `src/paint/` | Legacy rendering regressions; many tests call paint internals (`paint_tree_with_resources_*`, `ImageCache`, etc.). | TODO |
| `tests/llvm_statepoint_stackmap_llvm18.rs` | integration | `tests/integration.rs::fixtures::llvm` | Requires external tools (`llvm-as-18`, `llc-18`, `llvm-readobj-18`, `llvm-objdump-18`); test skips when missing. | TODO |
| `tests/misc_tests.rs` | unit | `src/**` (split) | Mixed grab-bag; many tests mutate env/process-wide knobs. Repo-only guardrails may stay under `tests/integration.rs::fixtures::*` if they don’t belong to `src/`. | TODO |
| `tests/overflow_tests.rs` | unit | `src/paint/stacking.rs` | Overflow/clip regressions under `tests/overflow/**`. | TODO |
| `tests/paged_media.rs` | unit | `src/layout/pagination.rs` | Dedicated paged-media regression target; remove standalone binary after moving into `src/layout/**`. | TODO |
| `tests/paint_tests.rs` | unit | `src/paint/` | Paint/backdrop/ref-image tests under `tests/paint/**`, `tests/backdrop/**`. | TODO |
| `tests/pipeline_churn_guardrail.rs` | special | `tests/isolation.rs` | Depends on process-isolated global debug counters (layout churn); currently a `#[path]` shim into `tests/layout/pipeline_churn_guardrail.rs`. | TODO |
| `tests/progress_tests.rs` | integration | `tests/integration.rs::fixtures::progress` | Repo artifact guardrail tests for `progress/pages/*.json` (no library code). | TODO |
| `tests/quirks_body_percent_height_tests.rs` | integration | `tests/integration.rs::api::quirks` | End-to-end render regression; spawns large-stack thread. | TODO |
| `tests/ref_tests.rs` | integration | `tests/integration.rs::common::ref` | Reference image diff/test harness tests; serializes env var changes via mutex. | TODO |
| `tests/regression_tests.rs` | unit | `src/**` (split) | Cross-cutting regressions (layout/paint/resource/js). Some use fixture files under `tests/pages/**` and `tests/fixtures/**`. | TODO |
| `tests/render_control_test_render_delay_smoke.rs` | integration | `tests/integration.rs::api::render_control` | Ensures `render_control::set_test_render_delay_ms` is callable from integration crates (no `cfg(test)`). | TODO |
| `tests/render_tests.rs` | integration | `tests/integration.rs::api::render` | End-to-end rendering APIs (`render_url`, diagnostics, timeouts, etc.). | TODO |
| `tests/resource_tests.rs` | unit | `src/resource/` | Resource loading/caching/CORS/referrer policy tests under `tests/resource/**` (uses `tests/test_support/net`). | TODO |
| `tests/scroll_tests.rs` | unit | `src/scroll.rs` | Scroll state/behavior tests under `tests/scroll/**`. | TODO |
| `tests/shadow_tests.rs` | unit | `src/dom/` | Shadow DOM behavior tests under `tests/shadow/**`. | TODO |
| `tests/style_tests.rs` | unit | `src/style/` | Large style regression suite under `tests/style/**` (some use ref-image utilities). | TODO |
| `tests/svg_integration_tests.rs` | unit | `src/image_loader.rs` | SVG rasterization/filter regressions (`ImageCache::render_svg_pixmap_at_size`, SVG filters). | TODO |
| `tests/taffy_cache_tests.rs` | special | `tests/isolation.rs` | `src/layout/taffy_integration.rs` snapshots env-driven cache limits on first use; needs process isolation unless refactored to be resettable. | TODO |
| `tests/text_tests.rs` | unit | `src/text/` | Text/shaping regressions under `tests/text/**`. | TODO |
| `tests/tree_tests.rs` | unit | `src/tree/` | Box/tree generation tests under `tests/tree/**`. | TODO |
| `tests/ui_tests.rs` | integration | `tests/integration.rs::api::ui` | Browser UI protocol tests under `tests/ui/**` (feature-gated in places). | TODO |
| `tests/user_agent_placeholder_pseudo_test.rs` | unit | `src/style/color.rs` | Placeholder pseudo default color (`GrayText`) regression; uses cascade helpers. | TODO |
| `tests/var_tests.rs` | unit | `src/style/var_resolution.rs` | CSS custom property (`var()`) resolution tests. | TODO |
| `tests/weibo_web_font_relative_url_test.rs` | unit | `src/text/font_loader.rs` | Uses fixtures; ensures `@font-face url(...)` in inline styles resolves against document base URL. | TODO |
| `tests/wpt_test.rs` | integration | `tests/integration.rs::wpt` | WPT runner + harness tests. Mutates env (`FASTR_USE_BUNDLED_FONTS`, `RAYON_NUM_THREADS`, `UPDATE_WPT_EXPECTED`, etc.). | TODO |

## `tests/` subdirectory summary (first pass)

This is a directory-level view to help catch “stray” test code and harnesses during parallel
migrations.

| Directory | Current contents | Target | Notes |
|---|---|---|---|
| `tests/accessibility/` | a11y tree/semantics tests | `src/accessibility.rs` | Unit tests. |
| `tests/allocation_failure/` | OOM + custom allocator harness | `tests/allocation_failure.rs` | Must stay separate due to `#[global_allocator]`. |
| `tests/animation/` | animation engine tests | `src/animation/` | Unit tests; `tests/animation_tests.rs` also contains top-level test code. |
| `tests/bin/` | CLI/binary tests | `tests/integration.rs::api::cli` | Keep as integration tests; share net/fs helpers via `tests/common/`. |
| `tests/browser_integration/` | browser/UI worker integration suite | `tests/integration.rs::browser_integration` | Runs in the shared integration binary; no process-init env mutation. Tests that touch global state should serialize via `stage_listener_test_lock()` / `common::global_test_lock()`. |
| `tests/css_integration/` | css loader/import/url rewrite tests | `src/css/loader.rs` (+ friends) | Despite name, these are mostly unit tests. |
| `tests/dom_integration/` | DOM parsing/query integration tests | `src/dom/**` + `src/dom2/**` | Unit tests. |
| `tests/fixtures/` | HTML + golden-image fixtures | `tests/integration.rs::fixtures` | Stays in `tests/` (data-driven integration). |
| `tests/layout/` | layout regressions, paging, flex/grid/table, etc | `src/layout/**` | Unit tests (bulk of migration). |
| `tests/paint/` + `tests/backdrop/` | paint/backdrop filter/render pipeline tests | `src/paint/**` | Unit tests; move shared Rayon init helper to `src/test_utils` or `tests/common`. |
| `tests/progress/` | guardrails for committed `progress/` artifacts | `tests/integration.rs::fixtures::progress` | Not library tests; keep in integration. |
| `tests/ref/` | image diff + ref-test harness utilities | `tests/common/` | Not a binary; used by fixtures/determinism/etc. |
| `tests/regression/` | cross-cutting regressions | `src/**` (split) | Many unit tests; some may remain fixture-driven integration tests. |
| `tests/resource/` | resource fetching/cache/CORS tests | `src/resource/**` | Unit tests; some use local HTTP server helpers. |
| `tests/style/` | cascade/values/layout-affecting style regressions | `src/style/**` | Unit tests. |
| `tests/svg_integration/` | SVG rasterization/filter tests | `src/image_loader.rs` + `src/paint/svg_filter/**` | Unit tests despite “integration” name. |
| `tests/taffy_cache/` | env-driven cache limit override tests | `tests/isolation.rs` (or refactor → `src/layout/taffy_integration.rs`) | Needs process isolation unless the env snapshot becomes resettable. |
| `tests/test_support/` | shared test helpers (net, etc) | `tests/common/` | Must not compile as its own binary. |
| `tests/text/` | shaping/text regressions | `src/text/**` | Unit tests. |
| `tests/tree/` | box/tree generation regressions | `src/tree/**` | Unit tests. |
| `tests/ui/` | browser UI protocol tests | `tests/integration.rs::api::ui` | Integration tests (feature-gated). |
| `tests/wpt/` + `tests/wpt_dom/` | WPT runners + fixtures | `tests/integration.rs::wpt` | Stays in `tests/` (fixture-driven integration). |

## End-state invariants to verify

- `ls tests/*.rs | wc -l` is **≤ 3**
- No `#[path = "..."]` in `tests/` (shims removed): `rg '#\\[path\\s*=' tests/` returns nothing
- No internal-module imports in `tests/` (integration tests use public API only)
