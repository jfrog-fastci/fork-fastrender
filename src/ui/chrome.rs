#![cfg(feature = "browser_ui")]

use crate::ui::browser_app::BrowserAppState;
use crate::render_control::StageHeartbeat;
use crate::ui::messages::TabId;

#[derive(Debug, Clone)]
pub enum ChromeAction {
  NewTab,
  CloseTab(TabId),
  ActivateTab(TabId),
  NavigateTo(String),
  Back,
  Forward,
  Reload,
  AddressBarFocusChanged(bool),
}

fn stage_label(stage: StageHeartbeat) -> &'static str {
  match stage {
    StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects | StageHeartbeat::DomParse => {
      "Fetch"
    }
    StageHeartbeat::CssInline | StageHeartbeat::CssParse | StageHeartbeat::Cascade => "CSS",
    StageHeartbeat::BoxTree | StageHeartbeat::Layout => "Layout",
    StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize => "Paint",
    StageHeartbeat::Script => "Script",
    StageHeartbeat::Done => "Done",
  }
}

pub fn chrome_ui(ctx: &egui::Context, app: &mut BrowserAppState) -> Vec<ChromeAction> {
  let mut actions = Vec::new();

  egui::TopBottomPanel::top("chrome").show(ctx, |ui| {
    // Tabs row.
    ui.horizontal_wrapped(|ui| {
      for tab in &app.tabs {
        let is_active = app.active_tab_id() == Some(tab.id);
        let title = tab.display_title();

        if ui.selectable_label(is_active, title).clicked() {
          actions.push(ChromeAction::ActivateTab(tab.id));
        }

        if ui.button("×").clicked() {
          actions.push(ChromeAction::CloseTab(tab.id));
        }

        ui.separator();
      }

      if ui.button("+").clicked() {
        actions.push(ChromeAction::NewTab);
      }
    });

    ui.separator();

    // Navigation + address bar row.
    ui.horizontal(|ui| {
      let (can_back, can_forward, loading, stage, error) = app
        .active_tab()
        .map(|t| (t.can_go_back, t.can_go_forward, t.loading, t.stage, t.error.clone()))
        .unwrap_or((false, false, false, None, None));

      if ui.add_enabled(can_back, egui::Button::new("←")).clicked() {
        actions.push(ChromeAction::Back);
      }
      if ui
        .add_enabled(can_forward, egui::Button::new("→"))
        .clicked()
      {
        actions.push(ChromeAction::Forward);
      }
      if ui.button("⟳").clicked() {
        actions.push(ChromeAction::Reload);
      }

      let address_bar_id = ui.make_persistent_id("address_bar");
      let response = ui.add(
        egui::TextEdit::singleline(&mut app.chrome.address_bar_text)
          .id(address_bar_id)
          .desired_width(f32::INFINITY)
          .hint_text("Enter URL…"),
      );

      if app.chrome.request_focus_address_bar {
        response.request_focus();
        app.chrome.request_focus_address_bar = false;
      }

      if app.chrome.request_select_all_address_bar {
        if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, address_bar_id) {
          let end = app.chrome.address_bar_text.chars().count();
          state.set_ccursor_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(end),
          )));
          state.store(ctx, address_bar_id);
        }
        app.chrome.request_select_all_address_bar = false;
      }

      let has_focus = response.has_focus();
      if has_focus != app.chrome.address_bar_has_focus {
        app.chrome.address_bar_has_focus = has_focus;
        app.chrome.address_bar_editing = has_focus;
        actions.push(ChromeAction::AddressBarFocusChanged(has_focus));
      }

      if has_focus && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.chrome.address_bar_editing = false;
        app.sync_address_bar_to_active();
        response.surrender_focus();
      }

      if has_focus && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        app.chrome.address_bar_editing = false;
        actions.push(ChromeAction::NavigateTo(app.chrome.address_bar_text.clone()));
        response.surrender_focus();
      }

      if loading {
        ui.add(egui::Spinner::new());
        ui.label("Loading…");
      }

      if loading {
        if let Some(stage) = stage.filter(|s| *s != StageHeartbeat::Done) {
          ui.label(format!("{}…", stage_label(stage)));
        }
      }

      if let Some(err) = error.as_deref().filter(|s| !s.trim().is_empty()) {
        ui.label(
          egui::RichText::new("Error")
            .color(egui::Color32::WHITE)
            .background_color(egui::Color32::from_rgb(160, 0, 0)),
        )
        .on_hover_text(err);
      }
    });
  });

  actions
}
