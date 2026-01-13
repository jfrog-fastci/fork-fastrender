pub mod about_pages;
pub(crate) mod about_pages_fetcher;
pub mod html_escape;
pub mod appearance;
pub mod compositor;
pub mod renderer_media_prefs;
pub mod chrome_assets;
pub mod bookmarks;
pub mod bookmarks_io_job;
pub mod chrome_frame;
pub mod browser_app;
pub mod browser_limits;
pub mod renderer_ipc;
pub mod protocol_limits;
pub mod browser_tab_controller;
pub mod chrome_action;
pub mod chrome_action_url;
pub mod chrome_dialog_action_url;
pub mod chrome_compositor_layout;
pub mod chrome_frame_document;
pub mod chrome_loading_progress;
pub mod process_assignment_config;
pub mod document_ticks;
pub mod clipboard_gate;
pub mod theme_parsing;
pub mod high_contrast;
// UI↔worker messaging lives in `messages.rs`.
//
// `render_worker` is the *single* production UI worker implementation. The `browser` binary and
// browser integration tests are expected to use it.
//
// Ownership contracts:
// - The worker owns per-tab navigation history. The UI drives it via `UiToWorker::{GoBack,GoForward,Reload}`.
// - Cancellation is cooperative: the UI should retain the per-tab `CancelGens` from `CreateTab` and
//   bump generations before sending new actions so in-flight work can be ignored/cancelled.
//
// `worker` contains small render-thread utilities (stage heartbeat forwarding, thread builder), but
// does **not** implement a separate UI worker loop.
pub mod cancel;
pub mod context_menu;
pub mod frame_upload;
pub mod worker_wake;
pub mod global_history;
pub mod history;
pub mod history_timestamp;
pub mod history_url_validation;
pub mod visited;
pub mod omnibox;
pub mod omnibox_nav;
pub mod panel_escape;
pub(crate) mod string_match;
pub mod tab_search;
pub mod load_progress;
pub mod search_suggest;
pub mod trusted_chrome_fetcher;
pub mod messages;
pub mod clipboard;
pub mod media_prefs;
pub mod window_title;
pub mod downloads;
pub mod downloads_panel_policy;
pub mod downloads_notifications;
pub mod open_in_new_tab;
pub mod titlebar_insets;
pub mod untrusted;
pub mod process_assignment;
pub mod render_worker;
pub mod renderer_backend;
pub mod renderer_process_id;
mod router_coalescer;
pub mod scrollbars;
pub mod shortcuts;
pub mod find_in_page;
pub mod repaint_scheduler;
pub mod address_bar;
pub mod url;
pub mod url_display;
pub mod security_indicator;
pub mod site_isolation;
pub mod worker;
pub mod zoom;
pub mod motion;
pub mod loading_overlay;
// Viewport-change throttling is used only by the windowed browser UI.
pub mod viewport_throttle;
pub mod notifications;
pub mod async_scroll;
pub mod chrome_dynamic_asset_fetcher;
pub mod renderer_chrome_html;
pub mod multiprocess;

#[cfg(any(test, feature = "browser_ui"))]
mod tab_accessible_label;

pub mod a11y_labels;

// OS clipboard access (arboard). Kept behind `browser_ui` so core renderer builds don't pull in
// windowing/clipboard system dependencies.
#[cfg(feature = "browser_ui")]
pub mod os_clipboard;

// egui widget accessibility helpers (AccessKit).
#[cfg(feature = "browser_ui")]
pub mod a11y;

// Minimal AccessKit integration for the compositor (non-egui) browser UI backend.
#[cfg(feature = "browser_ui_base")]
pub mod compositor_accessibility;

// AccessKit + winit adapter helpers for future renderer-based accessibility trees.
#[cfg(feature = "browser_ui")]
pub mod accesskit_bridge;

// Bridge between FastRender's accessibility tree and AccessKit (used by renderer-chrome UI work).
#[cfg(feature = "browser_ui")]
pub mod fastrender_accesskit;

// AccessKit node ID helpers for in-page accessibility nodes (distinct from egui's own nodes).
#[cfg(feature = "browser_ui")]
pub mod page_accesskit_ids;

// Test-only helpers for extracting egui/AccessKit accessibility output.
#[cfg(all(test, feature = "browser_ui"))]
pub mod a11y_test_util;

// Test-only helpers for snapshotting FastRender-generated AccessKit tree updates.
#[cfg(all(test, feature = "browser_ui"))]
pub mod accesskit_snapshot;

// Page accessibility (AccessKit subtree) conversion helpers.
#[cfg(feature = "browser_ui")]
pub mod page_accesskit_subtree;

// `chrome` depends on egui, so keep it behind the `browser_ui` feature gate.
#[cfg(feature = "browser_ui")]
pub mod chrome;

// Experimental: render browser chrome using FastRender itself (HTML/CSS), rather than egui.
#[cfg(feature = "browser_ui")]
pub mod renderer_chrome;

#[cfg(feature = "browser_ui")]
pub mod bookmarks_manager;

#[cfg(feature = "browser_ui")]
pub mod history_panel;

#[cfg(feature = "browser_ui")]
pub mod downloads_panel;

#[cfg(feature = "browser_ui")]
pub mod clear_browsing_data_dialog;

#[cfg(feature = "browser_ui")]
pub mod panels;

// `menu_bar` depends on egui and is only used by the windowed browser UI.
#[cfg(feature = "browser_ui")]
pub mod menu_bar;

// SVG→egui icon rasterization + caching for browser chrome widgets.
#[cfg(feature = "browser_ui")]
pub mod icons;

// WCAG-style contrast helpers used by chrome theme tests.
#[cfg(feature = "browser_ui")]
pub mod contrast;
// Shared egui widgets used by multiple side panels / dialogs (history/downloads/bookmarks/etc).
#[cfg(feature = "browser_ui")]
pub mod panel_widgets;

#[cfg(feature = "browser_ui")]
pub mod theme;

// Profile persistence helpers + debounced autosave worker.
pub mod profile_persistence;
pub mod profile_autosave;

// UI-thread throttle for sending large profile snapshots (history/bookmarks) to the autosave worker.
#[cfg(any(test, feature = "browser_ui"))]
pub mod autosave_send_scheduler;
#[cfg(any(test, feature = "browser_ui"))]
pub use autosave_send_scheduler::AutosaveSendScheduler;

#[cfg(any(test, feature = "browser_ui"))]
pub mod session;

#[cfg(any(test, feature = "browser_ui"))]
pub mod session_save_scheduler;

#[cfg(feature = "browser_ui")]
pub mod session_autosave;

// CLI parsing and wgpu-adapter selection knobs for the `browser` binary.
#[cfg(feature = "browser_ui")]
pub mod browser_cli;

pub use messages::{
  BrowserMediaPreferences, CursorKind, DatalistSuggestion, DownloadId, DownloadOutcome,
  NavigationReason, PointerButton, PointerModifiers, RenderedFrame, RepaintReason, TabId, UiToWorker,
  WakeReason, WorkerToUi,
};
pub use cancel::CancelGens;
pub use renderer_process_id::RendererProcessId;

pub use process_assignment::{ProcessAssignmentEvent, ProcessAssignmentState, ProcessModel};

// `input_mapping` depends on the optional egui/winit stack, so keep it behind the
// `browser_ui` feature gate.
#[cfg(feature = "browser_ui")]
pub mod input_mapping;

// Input routing helpers used by the windowed browser UI.
#[cfg(feature = "browser_ui")]
pub mod input_routing;

// AccessKit action routing for FastRender-rendered documents (screen reader "press", "set value",
// etc).
#[cfg(feature = "browser_ui")]
pub mod fast_accesskit_actions;

#[cfg(feature = "browser_ui")]
pub use input_mapping::{InputMapping, WheelDelta, CSS_PX_PER_WHEEL_LINE};

pub use browser_tab_controller::BrowserTabController;
#[cfg(any(test, feature = "browser_ui"))]
pub use render_worker::spawn_browser_worker_for_test;
pub use render_worker::{
  spawn_browser_ui_worker, spawn_browser_worker, spawn_browser_worker_with_factory,
  spawn_browser_worker_with_name,
  spawn_test_browser_worker, spawn_ui_worker, spawn_ui_worker_for_test,
  spawn_ui_worker_with_factory, BrowserWorkerHandle, UiThreadWorkerHandle,
};
pub use renderer_backend::{RendererBackend, RendererBackendHandle, ThreadRendererBackend};
pub use worker::RenderWorker;

#[cfg(feature = "browser_ui")]
pub mod wgpu_pixmap_texture;

pub use url::{
  normalize_user_url, omnibox_input_looks_like_url, resolve_link_url, resolve_omnibox_input,
  resolve_omnibox_search_query, validate_trusted_chrome_navigation_url_scheme,
  validate_user_navigation_url_scheme, OmniboxInputResolution, DEFAULT_SEARCH_ENGINE_TEMPLATE,
};
pub use site_isolation::{is_cross_site_navigation, SiteKey};
#[cfg(feature = "browser_ui")]
pub use wgpu_pixmap_texture::WgpuPixmapTexture;

pub use omnibox::{
  build_omnibox_suggestions, build_omnibox_suggestions_default_limit, AboutPagesProvider,
  BookmarksProvider, ClosedTabsProvider, DEFAULT_OMNIBOX_LIMIT, OmniboxAction, OmniboxContext,
  OmniboxProvider, OmniboxSearchSource, OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource,
  OpenTabsProvider, PrimaryActionProvider, RemoteSearchSuggestProvider, VisitedProvider,
};
pub use window_title::WindowTitleCache;
pub use search_suggest::{SearchSuggestConfig, SearchSuggestService, SearchSuggestUpdate};
pub use browser_app::{
  AppUpdate, BrowserAppState, BrowserTabState, ChromeState, ClosedTabState, DownloadEntry,
  DownloadProgressSummary, DownloadStatus, DownloadsState, FrameReadyUpdate, FindInPageState,
  LatestFrameMeta, OpenDatalistUpdate, OpenSelectDropdownUpdate, PageAccessibilitySnapshot,
  RemoteSearchSuggestCache,
  TabGroupColor, TabGroupId, TabGroupState,
};
pub use renderer_ipc::{FrameReadyLimits, FrameReadyViolation};
pub use global_history::{
  ClearBrowsingDataRange, GlobalHistoryEntry, GlobalHistorySearcher, GlobalHistoryStore,
  HistoryVisitDelta, DEFAULT_GLOBAL_HISTORY_CAPACITY,
};
pub use history::{HistoryEntry, TabHistory};
pub use visited::{should_record_visit_in_history, VisitedUrlRecord, VisitedUrlStore};
pub use zoom::{
  clamp_zoom, viewport_css_and_dpr_for_zoom, zoom_in, zoom_out, zoom_percent, zoom_reset,
  DEFAULT_ZOOM, MAX_ZOOM, MIN_ZOOM, ZOOM_STEP,
};

pub use bookmarks::{
  BookmarkDelta, BookmarkError, BookmarkFolder, BookmarkId, BookmarkNode, BookmarkStore,
  BookmarkStoreMigration, BOOKMARK_STORE_VERSION,
};
pub use notifications::{
  classify_warning_toast, Toast, ToastKind, ToastState, WarningToast, WarningToastIcon,
  WarningToastPresentation, WarningToastState, TOAST_DEFAULT_TTL, WARNING_TOAST_DEFAULT_TTL,
};

pub use frame_upload::{FrameUploadCoalescer, FrameUploadCoalescerStats};
pub use worker_wake::WorkerWakeCoalescer;
pub use viewport_throttle::{ViewportThrottle, ViewportThrottleConfig, ViewportUpdate};
pub use chrome_action::ChromeAction;
pub use chrome_action_url::ChromeActionUrl;
pub use chrome_dynamic_asset_fetcher::{ChromeDynamicAssetFetcher, ChromeDynamicAssetLimits};

pub use crate::select_dropdown;
pub use crate::select_dropdown::{SelectDropdown, SelectDropdownChoice};
pub use chrome_frame::chrome_frame_html_from_state;
#[cfg(feature = "browser_ui")]
pub use chrome::{chrome_ui, chrome_ui_with_bookmarks};
#[cfg(feature = "browser_ui")]
pub use menu_bar::{dispatch_menu_command, menu_bar_ui, MenuBarState, MenuCommand};
#[cfg(feature = "browser_ui")]
pub use session::{
  BrowserSession, BrowserSessionTab, BrowserSessionTabGroup, BrowserSessionWindow, BrowserWindowState,
};
#[cfg(feature = "browser_ui")]
pub use session_autosave::SessionAutosave;
#[cfg(feature = "browser_ui")]
pub use icons::{icon, icon_button, icon_button_with_id, icon_tinted, paint_icon_in_rect, spinner, BrowserIcon};
#[cfg(feature = "browser_ui")]
pub use panel_widgets::{
  danger_button, panel_empty_state, panel_header, panel_header_with_actions, panel_list_row,
  panel_search_field,
  PanelEmptyStateOutput, PanelHeaderOutput, PanelListRowResponse, SearchFieldOutput,
};
pub use profile_autosave::{AutosaveMsg, ProfileAutosaveHandle};
pub use profile_persistence::{
  bookmarks_path, history_path, load_bookmarks, load_history, parse_bookmarks_json,
  parse_history_json, save_bookmarks_atomic, save_history_atomic, LoadOutcome, LoadSource,
  PersistedGlobalHistoryStore,
};
