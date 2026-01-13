#![cfg(feature = "browser_ui")]

use crate::ui::about_pages;
use crate::ui::browser_app::BrowserAppState;
use crate::ui::chrome::ChromeAction;
use crate::ui::zoom;

#[cfg(target_os = "macos")]
const MOD_CMD_CTRL: &str = "Cmd";
#[cfg(not(target_os = "macos"))]
const MOD_CMD_CTRL: &str = "Ctrl";

#[cfg(target_os = "macos")]
const SHORTCUT_NEW_TAB: &str = "Cmd+T";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_NEW_TAB: &str = "Ctrl+T";

#[cfg(target_os = "macos")]
const SHORTCUT_NEW_WINDOW: &str = "Cmd+N";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_NEW_WINDOW: &str = "Ctrl+N";

#[cfg(target_os = "macos")]
const SHORTCUT_CLOSE_TAB: &str = "Cmd+W";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_CLOSE_TAB: &str = "Ctrl+W";

#[cfg(target_os = "macos")]
const SHORTCUT_SAVE_PAGE: &str = "Cmd+S";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_SAVE_PAGE: &str = "Ctrl+S";

#[cfg(target_os = "macos")]
const SHORTCUT_PRINT: &str = "Cmd+P";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_PRINT: &str = "Ctrl+P";

#[cfg(target_os = "macos")]
const SHORTCUT_REOPEN_TAB: &str = "Cmd+Shift+T";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_REOPEN_TAB: &str = "Ctrl+Shift+T";

#[cfg(target_os = "macos")]
const SHORTCUT_RELOAD: &str = "Cmd+R";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_RELOAD: &str = "Ctrl+R";

#[cfg(target_os = "macos")]
const SHORTCUT_ZOOM_IN: &str = "Cmd++";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_ZOOM_IN: &str = "Ctrl++";

#[cfg(target_os = "macos")]
const SHORTCUT_ZOOM_OUT: &str = "Cmd+-";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_ZOOM_OUT: &str = "Ctrl+-";

#[cfg(target_os = "macos")]
const SHORTCUT_ZOOM_RESET: &str = "Cmd+0";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_ZOOM_RESET: &str = "Ctrl+0";

#[cfg(target_os = "macos")]
const SHORTCUT_COPY: &str = "Cmd+C";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_COPY: &str = "Ctrl+C";

#[cfg(target_os = "macos")]
const SHORTCUT_CUT: &str = "Cmd+X";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_CUT: &str = "Ctrl+X";

#[cfg(target_os = "macos")]
const SHORTCUT_PASTE: &str = "Cmd+V";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_PASTE: &str = "Ctrl+V";

#[cfg(target_os = "macos")]
const SHORTCUT_BACK: &str = "Cmd+[";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_BACK: &str = "Alt+Left";

#[cfg(target_os = "macos")]
const SHORTCUT_FORWARD: &str = "Cmd+]";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_FORWARD: &str = "Alt+Right";

#[cfg(target_os = "macos")]
const SHORTCUT_BOOKMARK_MANAGER: &str = "Cmd+Shift+O";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_BOOKMARK_MANAGER: &str = "Ctrl+Shift+O";

#[cfg(target_os = "macos")]
const SHORTCUT_DOWNLOADS: &str = "Cmd+Shift+J";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_DOWNLOADS: &str = "Ctrl+J";

#[cfg(target_os = "macos")]
const SHORTCUT_TOGGLE_FULLSCREEN: &str = "Ctrl+Cmd+F";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_TOGGLE_FULLSCREEN: &str = "F11";

#[cfg(target_os = "macos")]
const SHORTCUT_QUIT: &str = "Cmd+Q";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_QUIT: &str = "Alt+F4";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuBarState {
  pub debug_log_open: bool,
  pub history_panel_open: bool,
  pub bookmarks_panel_open: bool,
  pub page_bookmarked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
  NewTab,
  NewWindow,
  CloseTab,
  Quit,
  Copy,
  Cut,
  Paste,
  Reload,
  ZoomIn,
  ZoomOut,
  ZoomReset,
  ToggleDebugLogPanel,
  ToggleHistoryPanel,
  ToggleBookmarksPanel,
  ToggleBookmarksManager,
  ToggleBookmarkThisPage,
  ToggleDownloadsPanel,
  ToggleFullScreen,
  Back,
  Forward,
  ReopenClosedTab,
  OpenHelp,
  OpenAbout,
}

pub fn menu_bar_ui(
  ctx: &egui::Context,
  app: &BrowserAppState,
  state: MenuBarState,
) -> Vec<MenuCommand> {
  let mut commands = Vec::new();
  let has_active_tab = app.active_tab_id().is_some();
  let can_close_tab = app.tabs.len() > 1 && has_active_tab;
  let (can_back, can_forward) = app
    .active_tab()
    .map(|tab| (tab.can_go_back, tab.can_go_forward))
    .unwrap_or((false, false));
  let can_reopen_closed = !app.closed_tabs.is_empty();
  let can_bookmark_this_page = app
    .active_tab()
    .and_then(|tab| tab.committed_url.as_deref().or(tab.current_url.as_deref()))
    .is_some();

  egui::TopBottomPanel::top("menu_bar")
    .resizable(false)
    .show(ctx, |ui| {
      egui::menu::bar(ui, |ui| {
        // -------------------------------------------------------------------
        // File
        // -------------------------------------------------------------------
        ui
          .menu_button("File", |ui| {
            let new_tab_resp =
              ui.add(egui::Button::new("New Tab").shortcut_text(SHORTCUT_NEW_TAB));
            new_tab_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Open new tab"));
            if new_tab_resp.clicked() {
              commands.push(MenuCommand::NewTab);
              ui.close_menu();
            }

            let save_page_resp = ui
              .add_enabled(
                false,
                egui::Button::new("Save Page…").shortcut_text(SHORTCUT_SAVE_PAGE),
              )
              .on_disabled_hover_text("Not implemented yet");
            save_page_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Save page (not implemented)")
            });

            let print_resp = ui
              .add_enabled(false, egui::Button::new("Print…").shortcut_text(SHORTCUT_PRINT))
              .on_disabled_hover_text("Not implemented yet");
            print_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Print page (not implemented)")
            });

            let close_tab_resp = ui.add_enabled(
              can_close_tab,
              egui::Button::new("Close Tab").shortcut_text(SHORTCUT_CLOSE_TAB),
            );
            close_tab_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close current tab")
            });
            if close_tab_resp.clicked() {
              commands.push(MenuCommand::CloseTab);
              ui.close_menu();
            }

            ui.separator();

            let quit_resp = ui.add(egui::Button::new("Quit").shortcut_text(SHORTCUT_QUIT));
            quit_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Quit browser"));
            if quit_resp.clicked() {
              commands.push(MenuCommand::Quit);
              ui.close_menu();
            }
          })
          .response
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "File menu"));

        // -------------------------------------------------------------------
        // Edit
        // -------------------------------------------------------------------
        ui
          .menu_button("Edit", |ui| {
            let undo_resp = ui
              .add_enabled(
                false,
                egui::Button::new("Undo").shortcut_text(format!("{MOD_CMD_CTRL}+Z")),
              )
              .on_disabled_hover_text("Not implemented yet");
            undo_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Undo"));

            let redo_resp = ui
              .add_enabled(
                false,
                egui::Button::new("Redo")
                  .shortcut_text(format!("{MOD_CMD_CTRL}+Shift+Z")),
              )
              .on_disabled_hover_text("Not implemented yet");
            redo_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Redo"));

            ui.separator();

            let cut_resp = ui.add(egui::Button::new("Cut").shortcut_text(SHORTCUT_CUT));
            cut_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Cut"));
            if cut_resp.clicked() {
              commands.push(MenuCommand::Cut);
              ui.close_menu();
            }

            let copy_resp = ui.add(egui::Button::new("Copy").shortcut_text(SHORTCUT_COPY));
            copy_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Copy"));
            if copy_resp.clicked() {
              commands.push(MenuCommand::Copy);
              ui.close_menu();
            }

            let paste_resp = ui.add(egui::Button::new("Paste").shortcut_text(SHORTCUT_PASTE));
            paste_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Paste"));
            if paste_resp.clicked() {
              commands.push(MenuCommand::Paste);
              ui.close_menu();
            }
          })
          .response
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Edit menu"));

        // -------------------------------------------------------------------
        // View
        // -------------------------------------------------------------------
        ui
          .menu_button("View", |ui| {
            let reload_resp = ui.add_enabled(
              has_active_tab,
              egui::Button::new("Reload").shortcut_text(SHORTCUT_RELOAD),
            );
            reload_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Reload page"));
            if reload_resp.clicked() {
              commands.push(MenuCommand::Reload);
              ui.close_menu();
            }

            ui.separator();

            let zoom_in_resp = ui.add_enabled(
              has_active_tab,
              egui::Button::new("Zoom In").shortcut_text(SHORTCUT_ZOOM_IN),
            );
            zoom_in_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Zoom in"));
            if zoom_in_resp.clicked() {
              commands.push(MenuCommand::ZoomIn);
              ui.close_menu();
            }

            let zoom_out_resp = ui.add_enabled(
              has_active_tab,
              egui::Button::new("Zoom Out").shortcut_text(SHORTCUT_ZOOM_OUT),
            );
            zoom_out_resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Zoom out"));
            if zoom_out_resp.clicked() {
              commands.push(MenuCommand::ZoomOut);
              ui.close_menu();
            }

            let zoom_reset_resp = ui.add_enabled(
              has_active_tab,
              egui::Button::new("Reset Zoom").shortcut_text(SHORTCUT_ZOOM_RESET),
            );
            zoom_reset_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Reset zoom")
            });
            if zoom_reset_resp.clicked() {
              commands.push(MenuCommand::ZoomReset);
              ui.close_menu();
            }

            ui.separator();

            let debug_log_a11y_label = if state.debug_log_open {
              "Hide debug log"
            } else {
              "Show debug log"
            };
            let mut show_debug_log = state.debug_log_open;
            let debug_log_resp = ui.checkbox(&mut show_debug_log, "Debug log");
            debug_log_resp.widget_info(move || {
              egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, debug_log_a11y_label)
            });
            if debug_log_resp.clicked() {
              commands.push(MenuCommand::ToggleDebugLogPanel);
              ui.close_menu();
            }

            let fullscreen_resp = ui.add(
              egui::Button::new("Toggle Full Screen").shortcut_text(SHORTCUT_TOGGLE_FULLSCREEN),
            );
            fullscreen_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Toggle full screen")
            });
            if fullscreen_resp.clicked() {
              commands.push(MenuCommand::ToggleFullScreen);
              ui.close_menu();
            }
          })
          .response
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "View menu"));

        // -------------------------------------------------------------------
        // History
        // -------------------------------------------------------------------
        ui
          .menu_button("History", |ui| {
            let back_resp =
              ui.add_enabled(can_back, egui::Button::new("Back").shortcut_text(SHORTCUT_BACK));
            back_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Go back")
            });
            if back_resp.clicked() {
              commands.push(MenuCommand::Back);
              ui.close_menu();
            }

            let forward_resp = ui.add_enabled(
              can_forward,
              egui::Button::new("Forward").shortcut_text(SHORTCUT_FORWARD),
            );
            forward_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Go forward")
            });
            if forward_resp.clicked() {
              commands.push(MenuCommand::Forward);
              ui.close_menu();
            }

            let reopen_resp = ui.add_enabled(
              can_reopen_closed,
              egui::Button::new("Reopen Closed Tab").shortcut_text(SHORTCUT_REOPEN_TAB),
            );
            reopen_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Reopen closed tab")
            });
            if reopen_resp.clicked() {
              commands.push(MenuCommand::ReopenClosedTab);
              ui.close_menu();
            }

            ui.separator();

            let history_panel_a11y_label = if state.history_panel_open {
              "Hide history panel"
            } else {
              "Show history panel"
            };
            let mut show_history_panel = state.history_panel_open;
            let history_panel_resp = ui
              .checkbox(&mut show_history_panel, "History panel")
              .on_hover_text("Show the global history side panel");
            history_panel_resp.widget_info(move || {
              egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, history_panel_a11y_label)
            });
            if history_panel_resp.clicked() {
              commands.push(MenuCommand::ToggleHistoryPanel);
              ui.close_menu();
            }
          })
          .response
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "History menu"));

        // -------------------------------------------------------------------
        // Bookmarks
        // -------------------------------------------------------------------
        ui
          .menu_button("Bookmarks", |ui| {
            let bookmark_label = if state.page_bookmarked {
              "Remove Bookmark"
            } else {
              "Bookmark This Page"
            };
            let bookmark_a11y_label = if state.page_bookmarked {
              "Remove bookmark"
            } else {
              "Bookmark this page"
            };
            let bookmark_resp = ui.add_enabled(
              can_bookmark_this_page,
              egui::Button::new(bookmark_label).shortcut_text(format!("{MOD_CMD_CTRL}+D")),
            );
            bookmark_resp.widget_info(move || {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, bookmark_a11y_label)
            });
            if bookmark_resp.clicked() {
              commands.push(MenuCommand::ToggleBookmarkThisPage);
              ui.close_menu();
            }

            ui.separator();

            let bookmarks_panel_a11y_label = if state.bookmarks_panel_open {
              "Hide bookmarks panel"
            } else {
              "Show bookmarks panel"
            };
            let mut show_bookmarks_panel = state.bookmarks_panel_open;
            let bookmarks_panel_resp = ui
              .checkbox(&mut show_bookmarks_panel, "Bookmarks panel")
              .on_hover_text("Show the bookmarks side panel");
            bookmarks_panel_resp.widget_info(move || {
              egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, bookmarks_panel_a11y_label)
            });
            if bookmarks_panel_resp.clicked() {
              commands.push(MenuCommand::ToggleBookmarksPanel);
              ui.close_menu();
            }

            let bookmarks_manager_resp = ui.add(
              egui::Button::new("Bookmark manager…").shortcut_text(SHORTCUT_BOOKMARK_MANAGER),
            );
            bookmarks_manager_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Open bookmark manager")
            });
            if bookmarks_manager_resp.clicked() {
              commands.push(MenuCommand::ToggleBookmarksManager);
              ui.close_menu();
            }
          })
          .response
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Bookmarks menu"));

        // -------------------------------------------------------------------
        // Window
        // -------------------------------------------------------------------
        ui
          .menu_button("Window", |ui| {
            let new_window_resp =
              ui.add(egui::Button::new("New Window").shortcut_text(SHORTCUT_NEW_WINDOW));
            new_window_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Open new window")
            });
            if new_window_resp.clicked() {
              commands.push(MenuCommand::NewWindow);
              ui.close_menu();
            }

            let downloads_resp = ui.add(
              egui::Button::new("Show Downloads…").shortcut_text(SHORTCUT_DOWNLOADS),
            );
            downloads_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Show downloads")
            });
            if downloads_resp.clicked() {
              commands.push(MenuCommand::ToggleDownloadsPanel);
              ui.close_menu();
            }
          })
          .response
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Window menu"));

        // -------------------------------------------------------------------
        // Help
        // -------------------------------------------------------------------
        ui
          .menu_button("Help", |ui| {
            let help_resp = ui.button("Help");
            help_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Open help")
            });
            if help_resp.clicked() {
              commands.push(MenuCommand::OpenHelp);
              ui.close_menu();
            }

            let about_resp = ui.button("About FastRender");
            about_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Open about FastRender")
            });
            if about_resp.clicked() {
              commands.push(MenuCommand::OpenAbout);
              ui.close_menu();
            }
          })
          .response
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Help menu"));

      });
    });

  commands
}

pub fn dispatch_menu_command(command: MenuCommand, app: &mut BrowserAppState) -> Vec<ChromeAction> {
  match command {
    MenuCommand::NewTab => vec![ChromeAction::NewTab],
    MenuCommand::NewWindow => vec![ChromeAction::NewWindow],
    MenuCommand::CloseTab => {
      if app.tabs.len() > 1 {
        app
          .active_tab_id()
          .map(|tab_id| vec![ChromeAction::CloseTab(tab_id)])
          .unwrap_or_default()
      } else {
        Vec::new()
      }
    }
    MenuCommand::Reload => vec![ChromeAction::Reload],
    MenuCommand::Back => vec![ChromeAction::Back],
    MenuCommand::Forward => vec![ChromeAction::Forward],
    MenuCommand::ReopenClosedTab => vec![ChromeAction::ReopenClosedTab],
    MenuCommand::ToggleBookmarksManager => vec![ChromeAction::ToggleBookmarksManager],
    MenuCommand::ToggleDownloadsPanel => vec![ChromeAction::ToggleDownloadsPanel],
    MenuCommand::ToggleFullScreen => vec![ChromeAction::ToggleFullScreen],
    MenuCommand::ZoomIn => {
      if let Some(tab) = app.active_tab_mut() {
        tab.zoom = zoom::zoom_in(tab.zoom);
      }
      Vec::new()
    }
    MenuCommand::ZoomOut => {
      if let Some(tab) = app.active_tab_mut() {
        tab.zoom = zoom::zoom_out(tab.zoom);
      }
      Vec::new()
    }
    MenuCommand::ZoomReset => {
      if let Some(tab) = app.active_tab_mut() {
        tab.zoom = zoom::zoom_reset();
      }
      Vec::new()
    }
    MenuCommand::OpenHelp => vec![
      ChromeAction::NewTab,
      ChromeAction::NavigateTo(about_pages::ABOUT_HELP.to_string()),
    ],
    MenuCommand::OpenAbout => vec![
      ChromeAction::NewTab,
      ChromeAction::NavigateTo(about_pages::ABOUT_VERSION.to_string()),
    ],
    MenuCommand::Copy
    | MenuCommand::Cut
    | MenuCommand::Paste
    | MenuCommand::ToggleDebugLogPanel
    | MenuCommand::ToggleHistoryPanel
    | MenuCommand::ToggleBookmarksPanel
    | MenuCommand::ToggleBookmarkThisPage
    | MenuCommand::Quit => Vec::new(),
  }
}

#[cfg(test)]
mod tests {
  use super::{dispatch_menu_command, menu_bar_ui, MenuBarState, MenuCommand};
  use crate::ui::browser_app::{BrowserAppState, BrowserTabState};
  use crate::ui::TabId;

  fn begin_frame(ctx: &egui::Context, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.events = events;
    ctx.begin_frame(raw);
  }

  fn left_click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    vec![
      egui::Event::PointerMoved(pos),
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
      },
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
      },
    ]
  }

  fn menu_bar_frame(
    ctx: &egui::Context,
    app: &BrowserAppState,
    events: Vec<egui::Event>,
  ) -> (Vec<MenuCommand>, egui::FullOutput) {
    begin_frame(ctx, events);
    let cmds = menu_bar_ui(ctx, app, MenuBarState::default());
    let output = ctx.end_frame();
    (cmds, output)
  }

  fn find_text_center(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Option<egui::Pos2> {
    fn in_shape(shape: &egui::epaint::Shape, needle: &str) -> Option<egui::Pos2> {
      match shape {
        egui::epaint::Shape::Text(text) => {
          if text.galley.text().contains(needle) {
            Some(text.pos + text.galley.size() / 2.0)
          } else {
            None
          }
        }
        egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|s| in_shape(s, needle)),
        _ => None,
      }
    }

    shapes.iter().find_map(|clipped| in_shape(&clipped.shape, needle))
  }

  fn click_menu_item(ctx: &egui::Context, app: &BrowserAppState, menu: &str, item: &str) -> Vec<MenuCommand> {
    let (_cmds, output) = menu_bar_frame(ctx, app, Vec::new());
    let menu_pos = find_text_center(&output.shapes, menu)
      .unwrap_or_else(|| panic!("failed to find menu bar label {menu:?}"));

    let (cmds, output) = menu_bar_frame(ctx, app, left_click_at(menu_pos));
    assert!(
      cmds.is_empty(),
      "expected opening the {menu} menu not to emit a command, got {cmds:?}"
    );

    // Egui's `menu_button` checks whether the menu is open before processing the click event that
    // opens it, so the popup contents are generally not painted until the *next* frame.
    let (_cmds, output) = menu_bar_frame(ctx, app, Vec::new());
    let item_pos = find_text_center(&output.shapes, item)
      .unwrap_or_else(|| panic!("failed to find menu item {menu} → {item}"));

    let (cmds, _output) = menu_bar_frame(ctx, app, left_click_at(item_pos));
    cmds
  }

  #[test]
  fn dispatch_new_tab_emits_chrome_action() {
    let mut app = BrowserAppState::new();
    let actions = dispatch_menu_command(MenuCommand::NewTab, &mut app);
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], crate::ui::ChromeAction::NewTab));
  }

  #[test]
  fn dispatch_new_window_emits_chrome_action() {
    let mut app = BrowserAppState::new();
    let actions = dispatch_menu_command(MenuCommand::NewWindow, &mut app);
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], crate::ui::ChromeAction::NewWindow));
  }

  #[test]
  fn dispatch_downloads_emits_chrome_action() {
    let mut app = BrowserAppState::new();
    let actions = dispatch_menu_command(MenuCommand::ToggleDownloadsPanel, &mut app);
    assert_eq!(actions.len(), 1);
    assert!(matches!(
      actions[0],
      crate::ui::ChromeAction::ToggleDownloadsPanel
    ));
  }

  #[test]
  fn dispatch_bookmarks_manager_emits_chrome_action() {
    let mut app = BrowserAppState::new();
    let actions = dispatch_menu_command(MenuCommand::ToggleBookmarksManager, &mut app);
    assert_eq!(actions.len(), 1);
    assert!(matches!(
      actions[0],
      crate::ui::ChromeAction::ToggleBookmarksManager
    ));
  }

  #[test]
  fn dispatch_toggle_full_screen_emits_chrome_action() {
    let mut app = BrowserAppState::new();
    let actions = dispatch_menu_command(MenuCommand::ToggleFullScreen, &mut app);
    assert_eq!(actions.len(), 1);
    assert!(matches!(
      actions[0],
      crate::ui::ChromeAction::ToggleFullScreen
    ));
  }

  #[test]
  fn file_new_tab_menu_item_emits_command() {
    let ctx = egui::Context::default();
    let app = BrowserAppState::new();
    let cmds = click_menu_item(&ctx, &app, "File", "New Tab");

    assert!(
      cmds.iter().any(|c| matches!(c, MenuCommand::NewTab)),
      "expected File → New Tab to emit MenuCommand::NewTab, got {cmds:?}"
    );
  }

  #[test]
  fn view_zoom_in_menu_mutates_active_tab_zoom() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);
    let before = app.active_tab().unwrap().zoom;
    let actions = dispatch_menu_command(MenuCommand::ZoomIn, &mut app);
    assert!(actions.is_empty());
    assert!(app.active_tab().unwrap().zoom > before);
  }

  #[test]
  fn window_new_window_menu_item_emits_command() {
    let ctx = egui::Context::default();
    let app = BrowserAppState::new();
    let cmds = click_menu_item(&ctx, &app, "Window", "New Window");

    assert!(
      cmds.iter().any(|c| matches!(c, MenuCommand::NewWindow)),
      "expected Window → New Window to emit MenuCommand::NewWindow, got {cmds:?}"
    );
  }

  #[test]
  fn window_show_downloads_menu_item_emits_command() {
    let ctx = egui::Context::default();
    let app = BrowserAppState::new();
    let cmds = click_menu_item(&ctx, &app, "Window", "Show Downloads…");

    assert!(
      cmds
        .iter()
        .any(|c| matches!(c, MenuCommand::ToggleDownloadsPanel)),
      "expected Window → Show Downloads… to emit MenuCommand::ToggleDownloadsPanel, got {cmds:?}"
    );
  }

  #[test]
  fn bookmarks_manager_menu_item_emits_command() {
    let ctx = egui::Context::default();
    let app = BrowserAppState::new();
    let cmds = click_menu_item(&ctx, &app, "Bookmarks", "Bookmark manager…");

    assert!(
      cmds
        .iter()
        .any(|c| matches!(c, MenuCommand::ToggleBookmarksManager)),
      "expected Bookmarks → Bookmark manager… to emit MenuCommand::ToggleBookmarksManager, got {cmds:?}"
    );
  }

  #[test]
  fn view_toggle_full_screen_menu_item_emits_command() {
    let ctx = egui::Context::default();
    let app = BrowserAppState::new();
    let cmds = click_menu_item(&ctx, &app, "View", "Toggle Full Screen");

    assert!(
      cmds.iter().any(|c| matches!(c, MenuCommand::ToggleFullScreen)),
      "expected View → Toggle Full Screen to emit MenuCommand::ToggleFullScreen, got {cmds:?}"
    );
  }
}
