pub mod about_pages;
pub mod appearance;
pub mod renderer_media_prefs;
pub mod bookmarks;
pub mod browser_app;
pub mod browser_limits;
pub mod browser_tab_controller;
pub mod chrome_loading_progress;
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
pub mod global_history;
pub mod history;
pub mod visited;
pub mod omnibox;
pub mod load_progress;
pub mod search_suggest;
pub mod messages;
pub mod downloads;
pub mod render_worker;
pub mod scrollbars;
pub mod shortcuts;
pub mod find_in_page;
pub mod repaint_scheduler;
pub mod address_bar;
pub mod url;
pub mod url_display;
pub mod security_indicator;
pub mod worker;
pub mod zoom;
pub mod motion;
pub mod loading_overlay;
// Viewport-change throttling is used only by the windowed browser UI.
pub mod viewport_throttle;
pub mod notifications;

// egui widget accessibility helpers (AccessKit).
#[cfg(feature = "browser_ui")]
pub mod a11y;

// `chrome` depends on egui, so keep it behind the `browser_ui` feature gate.
#[cfg(feature = "browser_ui")]
pub mod chrome;

#[cfg(feature = "browser_ui")]
pub mod bookmarks_manager;

// `menu_bar` depends on egui and is only used by the windowed browser UI.
#[cfg(feature = "browser_ui")]
pub mod menu_bar;

// SVG→egui icon rasterization + caching for browser chrome widgets.
#[cfg(feature = "browser_ui")]
pub mod icons;

#[cfg(feature = "browser_ui")]
pub mod theme;

// Profile persistence helpers + debounced autosave worker.
pub mod profile_persistence;
pub mod profile_autosave;

#[cfg(any(test, feature = "browser_ui"))]
pub mod session;

#[cfg(feature = "browser_ui")]
pub mod session_autosave;

// CLI parsing and wgpu-adapter selection knobs for the `browser` binary.
#[cfg(feature = "browser_ui")]
pub mod browser_cli;

pub use messages::{
  CursorKind, DownloadId, DownloadOutcome, NavigationReason, PointerButton, PointerModifiers,
  RenderedFrame, RepaintReason, TabId, UiToWorker, WorkerToUi,
};

// `input_mapping` depends on the optional egui/winit stack, so keep it behind the
// `browser_ui` feature gate.
#[cfg(feature = "browser_ui")]
pub mod input_mapping;

// Input routing helpers used by the windowed browser UI.
#[cfg(feature = "browser_ui")]
pub mod input_routing;

#[cfg(feature = "browser_ui")]
pub use input_mapping::{InputMapping, WheelDelta, CSS_PX_PER_WHEEL_LINE};

pub use browser_tab_controller::BrowserTabController;
#[cfg(any(test, feature = "browser_ui"))]
pub use render_worker::spawn_browser_worker_for_test;
pub use render_worker::{
  spawn_browser_ui_worker, spawn_browser_worker, spawn_browser_worker_with_factory,
  spawn_test_browser_worker, spawn_ui_worker, spawn_ui_worker_for_test,
  spawn_ui_worker_with_factory, BrowserWorkerHandle, UiThreadWorkerHandle,
};
pub use worker::RenderWorker;

#[cfg(feature = "browser_ui")]
pub mod wgpu_pixmap_texture;

pub use url::{
  normalize_user_url, omnibox_input_looks_like_url, resolve_link_url, resolve_omnibox_input,
  validate_user_navigation_url_scheme, OmniboxInputResolution, DEFAULT_SEARCH_ENGINE_TEMPLATE,
};
#[cfg(feature = "browser_ui")]
pub use wgpu_pixmap_texture::WgpuPixmapTexture;

pub use omnibox::{
  build_omnibox_suggestions, build_omnibox_suggestions_default_limit, AboutPagesProvider,
  BookmarksProvider, ClosedTabsProvider, DEFAULT_OMNIBOX_LIMIT, OmniboxAction, OmniboxContext,
  OmniboxProvider, OmniboxSearchSource, OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource,
  OpenTabsProvider, PrimaryActionProvider, RemoteSearchSuggestProvider, VisitedProvider,
};
pub use search_suggest::{SearchSuggestConfig, SearchSuggestService, SearchSuggestUpdate};
pub use browser_app::{
  AppUpdate, BrowserAppState, BrowserTabState, ChromeState, ClosedTabState, DownloadEntry,
  DownloadProgressSummary, DownloadStatus, DownloadsState, FrameReadyUpdate, FindInPageState,
  LatestFrameMeta, OpenSelectDropdownUpdate, RemoteSearchSuggestCache, TabGroupColor, TabGroupId,
  TabGroupState,
};
pub use global_history::{
  ClearBrowsingDataRange, GlobalHistoryEntry, GlobalHistoryStore, DEFAULT_GLOBAL_HISTORY_CAPACITY,
};
pub use history::{HistoryEntry, TabHistory};
pub use visited::{should_record_visit_in_history, VisitedUrlRecord, VisitedUrlStore};
pub use zoom::{
  clamp_zoom, viewport_css_and_dpr_for_zoom, zoom_in, zoom_out, zoom_percent, zoom_reset,
  DEFAULT_ZOOM, MAX_ZOOM, MIN_ZOOM, ZOOM_STEP,
};

pub use bookmarks::{
  BookmarkError, BookmarkFolder, BookmarkId, BookmarkNode, BookmarkStore, BookmarkStoreMigration,
  BOOKMARK_STORE_VERSION,
};
pub use notifications::{
  classify_warning_toast, WarningToast, WarningToastIcon, WarningToastPresentation, WarningToastState,
  WARNING_TOAST_DEFAULT_TTL,
};

pub use frame_upload::FrameUploadCoalescer;
pub use viewport_throttle::{ViewportThrottle, ViewportThrottleConfig, ViewportUpdate};

pub use crate::select_dropdown;
pub use crate::select_dropdown::{SelectDropdown, SelectDropdownChoice};
#[cfg(feature = "browser_ui")]
pub use chrome::{chrome_ui, chrome_ui_with_bookmarks, ChromeAction};
#[cfg(feature = "browser_ui")]
pub use menu_bar::{dispatch_menu_command, menu_bar_ui, MenuBarState, MenuCommand};
#[cfg(feature = "browser_ui")]
pub use session::{BrowserSession, BrowserSessionTab, BrowserSessionWindow, BrowserWindowState};
#[cfg(feature = "browser_ui")]
pub use session_autosave::SessionAutosave;
#[cfg(feature = "browser_ui")]
pub use icons::{icon, icon_button, icon_tinted, paint_icon_in_rect, spinner, BrowserIcon};
pub use profile_autosave::{AutosaveMsg, ProfileAutosaveHandle};
pub use profile_persistence::{
  bookmarks_path, history_path, load_bookmarks, load_history, parse_bookmarks_json,
  parse_history_json, save_bookmarks_atomic, save_history_atomic, LoadOutcome, LoadSource,
  PersistedGlobalHistoryStore,
};
