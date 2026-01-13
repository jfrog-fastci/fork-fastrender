use crate::ui::bookmarks::BookmarkId;
use crate::ui::messages::TabId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeAction {
  /// Focus the address bar and select all contents.
  ///
  /// This is emitted by chrome-level keyboard shortcuts (e.g. Ctrl/Cmd+L).
  ///
  /// Front-ends are expected to translate this into UI state changes (e.g. setting
  /// `ChromeState::request_focus_address_bar` / `request_select_all_address_bar`).
  FocusAddressBar,
  /// Open a new top-level browser window.
  NewWindow,
  /// Toggle native window fullscreen mode.
  ToggleFullScreen,
  OpenFindInPage,
  /// Save the current page (Ctrl/Cmd+S).
  ///
  /// Front-ends may implement this via a native save dialog. When unimplemented, it should surface a
  /// clear user-facing notification.
  SavePage,
  /// Print the current page (Ctrl/Cmd+P).
  ///
  /// Front-ends may implement this via a native print dialog/preview. When unimplemented, it should
  /// surface a clear user-facing notification.
  PrintPage,
  /// Begin/update an active "find in page" query for a tab.
  FindQuery {
    tab_id: TabId,
    query: String,
    case_sensitive: bool,
  },
  /// Jump to the next match for the active find query.
  FindNext(TabId),
  /// Jump to the previous match for the active find query.
  FindPrev(TabId),
  /// Close the find bar for a tab and clear highlights/results.
  CloseFindInPage(TabId),
  NewTab,
  CloseTab(TabId),
  /// Detach a tab into a new top-level browser window.
  DetachTab(TabId),
  ReloadTab(TabId),
  DuplicateTab(TabId),
  CloseOtherTabs(TabId),
  CloseTabsToRight(TabId),
  ReopenClosedTab,
  ActivateTab(TabId),
  TogglePinTab(TabId),
  NavigateTo(String),
  /// Open an omnibox/address-bar input in a new foreground tab, leaving the current tab unchanged.
  ///
  /// The windowed browser is expected to resolve/normalize/validate the provided string using the
  /// same pipeline as a typed navigation (see `BrowserTabState::navigate_typed`).
  OpenUrlInNewTab(String),
  Back,
  Forward,
  Reload,
  StopLoading,
  Home,
  /// Open the tab search / quick switcher overlay (Ctrl/Cmd+Shift+A).
  OpenTabSearch,
  /// Close the tab search / quick switcher overlay (Escape, selection, click-away).
  CloseTabSearch,
  /// Toggle visibility of the bookmarks bar.
  ToggleBookmarksBar,
  /// Show/hide the in-window top menu bar.
  ///
  /// This does not affect keyboard shortcuts (which are routed independently).
  SetShowMenuBar(bool),
  AddressBarFocusChanged(bool),
  /// Toggle a bookmark for the currently active tab.
  ToggleBookmarkForActiveTab,
  /// Reorder the bookmarks bar (root node list) to the exact provided order.
  ReorderBookmarksBar(Vec<BookmarkId>),
  /// Toggle visibility of the global history panel.
  ToggleHistoryPanel,
  /// Toggle visibility of the bookmarks manager UI.
  ToggleBookmarksManager,
  /// Open the clear browsing data dialog.
  OpenClearBrowsingDataDialog,
  /// Open the "Set home page URL" dialog.
  OpenHomeUrlDialog,
  /// Toggle visibility of the downloads panel.
  ToggleDownloadsPanel,
}
