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
const SHORTCUT_CLOSE_TAB: &str = "Cmd+W";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_CLOSE_TAB: &str = "Ctrl+W";

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
  ToggleBookmarkThisPage,
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
        ui.menu_button("File", |ui| {
          if ui
            .add(egui::Button::new("New Tab").shortcut_text(SHORTCUT_NEW_TAB))
            .clicked()
          {
            commands.push(MenuCommand::NewTab);
            ui.close_menu();
          }

          if ui
            .add_enabled(
              can_close_tab,
              egui::Button::new("Close Tab").shortcut_text(SHORTCUT_CLOSE_TAB),
            )
            .clicked()
          {
            commands.push(MenuCommand::CloseTab);
            ui.close_menu();
          }

          ui.separator();

          if ui
            .add(egui::Button::new("Quit").shortcut_text(SHORTCUT_QUIT))
            .clicked()
          {
            commands.push(MenuCommand::Quit);
            ui.close_menu();
          }
        });

        // -------------------------------------------------------------------
        // Edit
        // -------------------------------------------------------------------
        ui.menu_button("Edit", |ui| {
          ui
            .add_enabled(
              false,
              egui::Button::new("Undo").shortcut_text(format!("{MOD_CMD_CTRL}+Z")),
            )
            .on_disabled_hover_text("Not implemented yet");

          ui
            .add_enabled(
              false,
              egui::Button::new("Redo")
                .shortcut_text(format!("{MOD_CMD_CTRL}+Shift+Z")),
            )
            .on_disabled_hover_text("Not implemented yet");

          ui.separator();

          if ui
            .add(egui::Button::new("Cut").shortcut_text(SHORTCUT_CUT))
            .clicked()
          {
            commands.push(MenuCommand::Cut);
            ui.close_menu();
          }
          if ui
            .add(egui::Button::new("Copy").shortcut_text(SHORTCUT_COPY))
            .clicked()
          {
            commands.push(MenuCommand::Copy);
            ui.close_menu();
          }
          if ui
            .add(egui::Button::new("Paste").shortcut_text(SHORTCUT_PASTE))
            .clicked()
          {
            commands.push(MenuCommand::Paste);
            ui.close_menu();
          }
        });

        // -------------------------------------------------------------------
        // View
        // -------------------------------------------------------------------
        ui.menu_button("View", |ui| {
          if ui
            .add_enabled(
              has_active_tab,
              egui::Button::new("Reload").shortcut_text(SHORTCUT_RELOAD),
            )
            .clicked()
          {
            commands.push(MenuCommand::Reload);
            ui.close_menu();
          }

          ui.separator();

          if ui
            .add_enabled(
              has_active_tab,
              egui::Button::new("Zoom In").shortcut_text(SHORTCUT_ZOOM_IN),
            )
            .clicked()
          {
            commands.push(MenuCommand::ZoomIn);
            ui.close_menu();
          }
          if ui
            .add_enabled(
              has_active_tab,
              egui::Button::new("Zoom Out").shortcut_text(SHORTCUT_ZOOM_OUT),
            )
            .clicked()
          {
            commands.push(MenuCommand::ZoomOut);
            ui.close_menu();
          }
          if ui
            .add_enabled(
              has_active_tab,
              egui::Button::new("Reset Zoom").shortcut_text(SHORTCUT_ZOOM_RESET),
            )
            .clicked()
          {
            commands.push(MenuCommand::ZoomReset);
            ui.close_menu();
          }

          ui.separator();

          let mut show_debug_log = state.debug_log_open;
          if ui.checkbox(&mut show_debug_log, "Debug log").clicked() {
            commands.push(MenuCommand::ToggleDebugLogPanel);
            ui.close_menu();
          }

          // Placeholder (full screen).
          ui
            .add_enabled(false, egui::Button::new("Toggle Full Screen"))
            .on_disabled_hover_text("Not implemented yet");
        });

        // -------------------------------------------------------------------
        // History
        // -------------------------------------------------------------------
        ui.menu_button("History", |ui| {
          if ui
            .add_enabled(can_back, egui::Button::new("Back").shortcut_text(SHORTCUT_BACK))
            .clicked()
          {
            commands.push(MenuCommand::Back);
            ui.close_menu();
          }
          if ui
            .add_enabled(
              can_forward,
              egui::Button::new("Forward").shortcut_text(SHORTCUT_FORWARD),
            )
            .clicked()
          {
            commands.push(MenuCommand::Forward);
            ui.close_menu();
          }
          if ui
            .add_enabled(
              can_reopen_closed,
              egui::Button::new("Reopen Closed Tab").shortcut_text(SHORTCUT_REOPEN_TAB),
            )
            .clicked()
          {
            commands.push(MenuCommand::ReopenClosedTab);
            ui.close_menu();
          }

          ui.separator();

          let mut show_history_panel = state.history_panel_open;
          if ui
            .checkbox(&mut show_history_panel, "History panel")
            .on_hover_text("Show the global history side panel")
            .clicked()
          {
            commands.push(MenuCommand::ToggleHistoryPanel);
            ui.close_menu();
          }
        });

        // -------------------------------------------------------------------
        // Bookmarks
        // -------------------------------------------------------------------
        ui.menu_button("Bookmarks", |ui| {
          let bookmark_label = if state.page_bookmarked {
            "★ Bookmark This Page"
          } else {
            "☆ Bookmark This Page"
          };
          if ui
            .add_enabled(
              can_bookmark_this_page,
              egui::Button::new(bookmark_label).shortcut_text(format!("{MOD_CMD_CTRL}+D")),
            )
            .clicked()
          {
            commands.push(MenuCommand::ToggleBookmarkThisPage);
            ui.close_menu();
          }

          ui.separator();

          let mut show_bookmarks_panel = state.bookmarks_panel_open;
          if ui
            .checkbox(&mut show_bookmarks_panel, "Bookmarks panel")
            .on_hover_text("Show the bookmarks side panel")
            .clicked()
          {
            commands.push(MenuCommand::ToggleBookmarksPanel);
            ui.close_menu();
          }

          ui
            .add_enabled(false, egui::Button::new("Bookmark manager…"))
            .on_disabled_hover_text("Not implemented yet");
        });

        // -------------------------------------------------------------------
        // Window
        // -------------------------------------------------------------------
        ui.menu_button("Window", |ui| {
          ui
            .add_enabled(false, egui::Button::new("New Window"))
            .on_disabled_hover_text("Not implemented yet");
          ui
            .add_enabled(false, egui::Button::new("Show Downloads…"))
            .on_disabled_hover_text("Not implemented yet");
        });

        // -------------------------------------------------------------------
        // Help
        // -------------------------------------------------------------------
        ui.menu_button("Help", |ui| {
          if ui.button("Help").clicked() {
            commands.push(MenuCommand::OpenHelp);
            ui.close_menu();
          }
          if ui.button("About FastRender").clicked() {
            commands.push(MenuCommand::OpenAbout);
            ui.close_menu();
          }
        });
      });
    });

  commands
}

pub fn dispatch_menu_command(command: MenuCommand, app: &mut BrowserAppState) -> Vec<ChromeAction> {
  match command {
    MenuCommand::NewTab => vec![ChromeAction::NewTab],
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

  #[test]
  fn dispatch_new_tab_emits_chrome_action() {
    let mut app = BrowserAppState::new();
    let actions = dispatch_menu_command(MenuCommand::NewTab, &mut app);
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], crate::ui::ChromeAction::NewTab));
  }

  #[test]
  fn file_new_tab_menu_item_emits_command() {
    let ctx = egui::Context::default();
    let app = BrowserAppState::new();

    // Frame 1: open the File menu.
    begin_frame(&ctx, left_click_at(egui::pos2(10.0, 10.0)));
    let cmds = menu_bar_ui(&ctx, &app, MenuBarState::default());
    let _ = ctx.end_frame();
    assert!(
      cmds.is_empty(),
      "expected opening the File menu not to emit a command, got {cmds:?}"
    );

    // Frame 2: click the first menu item ("New Tab").
    begin_frame(&ctx, left_click_at(egui::pos2(20.0, 32.0)));
    let cmds = menu_bar_ui(&ctx, &app, MenuBarState::default());
    let _ = ctx.end_frame();

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
}
