#![cfg(feature = "browser_ui")]

//! Bookmarks manager UI for the windowed browser frontend.
//!
//! This is implemented as an `egui::SidePanel` so it does not overlap the rendered page image
//! (which keeps page hit-testing/pointer forwarding simple).

use std::collections::HashMap;
use std::borrow::Cow;
use std::time::Duration;

use crate::ui::bookmarks_io_job::{BookmarksIoJob, BookmarksIoJobUpdate};
use crate::ui::motion::UiMotion;

use super::{
  a11y, icon_button, icon_tinted, panel_empty_state, panel_header, panel_search_field, BookmarkId,
  BookmarkDelta, BookmarkNode, BookmarkStore, BrowserIcon,
};

use super::string_match::contains_ascii_case_insensitive;

#[derive(Debug, Clone)]
pub enum BookmarksManagerAction {
  Open(String),
  OpenInNewTab(String),
}

#[derive(Debug, Default)]
pub struct BookmarksManagerState {
  pub search: String,
  pub import_json: String,
  pub import_path: String,
  pub export_path: String,
  pub export_json: Option<String>,
  pub message: Option<String>,
  pub error: Option<String>,

  io_job: BookmarksIoJob,
  request_focus_search: bool,
  creating_folder: Option<CreateFolderState>,
  editing_bookmark: Option<EditBookmarkState>,

  /// Cached flattened list of bookmark rows for virtualized rendering.
  list_cache: Option<BookmarksListCache>,
  /// Incremented when folder open/closed state changes so the flattened view can be rebuilt.
  folder_open_revision: u64,
  /// Cached folder dropdown options + parent-label map (rebuilt on folder revision changes).
  folder_cache: Option<BookmarksFolderCache>,
}

impl BookmarksManagerState {
  pub fn request_focus_search(&mut self) {
    self.request_focus_search = true;
  }

  pub fn clear_transient(&mut self) {
    self.creating_folder = None;
    self.editing_bookmark = None;
  }

  fn poll_io_job(&mut self, store: &mut BookmarkStore, out: &mut BookmarksManagerOutput) {
    let Some(update) = self.io_job.poll() else {
      return;
    };

    match update {
      BookmarksIoJobUpdate::ExportFinished { path, result } => match result {
        Ok(()) => {
          self.error = None;
          self.message = Some(format!("Exported bookmarks to {}.", path));
        }
        Err(err) => {
          self.error = Some(err);
        }
      },
      BookmarksIoJobUpdate::ImportFinished { result, .. } => match result {
        Ok((imported, migration)) => {
          let delta = BookmarkDelta::ReplaceAll(imported);
          match store.apply_delta(&delta) {
            Ok(()) => {
              out.bookmark_deltas.push(delta);
              out.changed = true;
              out.request_flush = true;
              self.error = None;
              self.message = Some(format!("Imported bookmarks from file ({migration:?})."));
              self.import_json.clear();
              self.clear_transient();
            }
            Err(err) => {
              self.error = Some(format!("Failed to import bookmarks: {err:?}"));
            }
          }
        }
        Err(err) => {
          self.error = Some(err);
        }
      },
    }
  }
}

#[derive(Debug, Clone)]
struct BookmarksListCache {
  /// Revision key used to determine when the flattened list needs rebuilding.
  ///
  /// - For normal tree mode (empty search), this is `BookmarkStore::structure_revision()` so
  ///   bookmark content-only edits don't force a full rebuild.
  /// - For search mode, this is `BookmarkStore::revision()` since title/URL updates affect matches.
  cache_revision: u64,
  search_query: String,
  folder_open_revision: u64,
  rows: Vec<BookmarkRow>,
}

#[derive(Debug, Clone)]
struct BookmarksFolderCache {
  folder_revision: u64,
  folder_options: Vec<(Option<BookmarkId>, String)>,
  folder_labels: HashMap<Option<BookmarkId>, String>,
}

impl BookmarksFolderCache {
  fn new(store: &BookmarkStore) -> Self {
    let folder_options = folder_options(store);
    let folder_labels = folder_options.iter().cloned().collect::<HashMap<_, _>>();
    Self {
      folder_revision: store.folder_revision(),
      folder_options,
      folder_labels,
    }
  }

  fn ensure_up_to_date(&mut self, store: &BookmarkStore) {
    let revision = store.folder_revision();
    if self.folder_revision == revision {
      return;
    }
    self.folder_revision = revision;
    self.folder_options = folder_options(store);
    self.folder_labels = self
      .folder_options
      .iter()
      .cloned()
      .collect::<HashMap<_, _>>();
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BookmarkRowKind {
  Folder,
  Bookmark,
}

#[derive(Debug, Clone, Copy)]
struct BookmarkRow {
  kind: BookmarkRowKind,
  id: BookmarkId,
  depth: usize,
}

#[derive(Debug, Clone)]
struct CreateFolderState {
  title: String,
  parent: Option<BookmarkId>,
  error: Option<String>,
  request_focus_title: bool,
}

#[derive(Debug, Clone)]
struct EditBookmarkState {
  id: BookmarkId,
  title: String,
  url: String,
  parent: Option<BookmarkId>,
  error: Option<String>,
  request_focus_title: bool,
}

#[derive(Debug, Default)]
pub struct BookmarksManagerOutput {
  pub actions: Vec<BookmarksManagerAction>,
  pub changed: bool,
  pub bookmark_deltas: Vec<BookmarkDelta>,
  /// Whether this change is destructive enough to justify a best-effort immediate flush.
  pub request_flush: bool,
  pub close_requested: bool,
  /// Whether the panel contained a focused text input, and thus the page should not request egui
  /// focus this frame.
  pub unfocus_page: bool,
}

pub fn bookmarks_manager_side_panel(
  ctx: &egui::Context,
  state: &mut BookmarksManagerState,
  store: &mut BookmarkStore,
) -> BookmarksManagerOutput {
  let mut out = BookmarksManagerOutput::default();
  let mut folder_cache = state
    .folder_cache
    .take()
    .unwrap_or_else(|| BookmarksFolderCache::new(store));
  folder_cache.ensure_up_to_date(store);

  // Poll import/export background jobs before building UI.
  state.poll_io_job(store, &mut out);

  // While an IO job is active, keep the UI repainting so the "Working…" indicator animates and we
  // can pick up completion without requiring additional user input.
  if state.io_job.is_busy() {
    ctx.request_repaint_after(Duration::from_millis(16));
  }

  egui::SidePanel::left("fastr_bookmarks_manager")
    .resizable(true)
    .default_width(360.0)
    .show(ctx, |ui| {
      // -----------------------------------------------------------------------
      // Header
      // -----------------------------------------------------------------------
      panel_header(ui, BrowserIcon::BookmarkFilled, "Bookmarks", || {
        out.close_requested = true;
      });

      folder_cache.ensure_up_to_date(store);

      ui.add_space(ui.spacing().item_spacing.y.max(6.0));

      // -----------------------------------------------------------------------
      // Messages / errors (callouts)
      // -----------------------------------------------------------------------
      if let Some(msg) = state.message.as_deref().filter(|s| !s.trim().is_empty()) {
        if callout_dismissible(ui, CalloutKind::Message, msg, "Dismiss message") {
          state.message = None;
        }
        ui.add_space(ui.spacing().item_spacing.y.max(6.0));
      }
      if let Some(err) = state.error.as_deref().filter(|s| !s.trim().is_empty()) {
        if callout_dismissible(ui, CalloutKind::Error, err, "Dismiss error") {
          state.error = None;
        }
        ui.add_space(ui.spacing().item_spacing.y.max(6.0));
      }

      // -----------------------------------------------------------------------
      // Search + toolbar
      // -----------------------------------------------------------------------
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let new_folder = icon_button(ui, BrowserIcon::Folder, "New folder", true);
        new_folder
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Create new folder"));
        if new_folder.clicked() {
          state.creating_folder = Some(CreateFolderState {
            title: String::new(),
            parent: None,
            error: None,
            request_focus_title: true,
          });
        }

        ui.add_space(6.0);
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
          search_bar(ui, state, &mut out);
        });
      });
      ui.add_space(ui.spacing().item_spacing.y.max(8.0));

      // -----------------------------------------------------------------------
      // Create folder / edit bookmark flows (cards)
      // -----------------------------------------------------------------------
      if state.creating_folder.is_some() {
        {
          let folder_options = folder_cache.folder_options.as_slice();
          create_folder_card(ui, state, store, folder_options, &mut out);
        }
        folder_cache.ensure_up_to_date(store);
        ui.add_space(ui.spacing().item_spacing.y.max(8.0));
      }

      if state.editing_bookmark.is_some() {
        {
          let folder_options = folder_cache.folder_options.as_slice();
          edit_bookmark_card(ui, state, store, folder_options, &mut out);
        }
        folder_cache.ensure_up_to_date(store);
        ui.add_space(ui.spacing().item_spacing.y.max(8.0));
      }

      // -----------------------------------------------------------------------
      // Import / export
      // -----------------------------------------------------------------------
      ui.collapsing("Import / Export", |ui| {
        let profile_path = crate::ui::bookmarks_path();
        ui.label(
          egui::RichText::new(format!("Profile file: {}", profile_path.display()))
            .small()
            .color(ui.visuals().weak_text_color()),
        );

        ui.add_space(6.0);

        section_card(ui, "Export", |ui| {
          ui.horizontal_wrapped(|ui| {
            if ui.button("Copy JSON to clipboard").clicked() {
              match serde_json::to_string_pretty(store) {
                Ok(json) => {
                  state.export_json = Some(json.clone());
                  ctx.output_mut(|o| o.copied_text = json);
                  state.error = None;
                  state.message = Some("Exported bookmarks JSON copied to clipboard.".to_string());
                }
                Err(err) => {
                  state.error = Some(format!("Failed to export bookmarks: {err}"));
                }
              }
            }
            if let Some(json) = state.export_json.as_ref() {
              if ui.button("Copy last export").clicked() {
                ctx.output_mut(|o| o.copied_text = json.clone());
              }
            }
          });

          ui.add_space(6.0);
          ui.label(
            egui::RichText::new("Export path (optional)")
              .small()
              .color(ui.visuals().weak_text_color()),
          );

          let resp = ui.add(
            egui::TextEdit::singleline(&mut state.export_path)
              .hint_text(profile_path.display().to_string())
              .desired_width(f32::INFINITY),
          );
          resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Export path"));
          if resp.has_focus() || resp.clicked() {
            out.unfocus_page = true;
          }

          ui.horizontal_wrapped(|ui| {
            if ui.button("Use profile path").clicked() {
              state.export_path = profile_path.display().to_string();
            }

            let export_btn =
              ui.add_enabled(!state.io_job.is_busy(), egui::Button::new("Export file"));
            if export_btn.clicked() {
              let raw = state.export_path.trim();
              if raw.is_empty() {
                state.error = Some("Export path is empty.".to_string());
              } else {
                if let Err(err) = state.io_job.start_export(raw.to_string(), store.clone()) {
                  state.error = Some(err);
                }
              }
            }

            if state.io_job.is_exporting() {
              ui.label(
                egui::RichText::new("Working…")
                  .small()
                  .color(ui.visuals().weak_text_color()),
              );
            }
          });

          if let Some(json) = state.export_json.as_mut() {
            ui.add_space(6.0);
            ui.collapsing("Show exported JSON", |ui| {
              let resp = ui.add(
                egui::TextEdit::multiline(json)
                  .code_editor()
                  .desired_rows(6)
                  .lock_focus(true)
                  .desired_width(f32::INFINITY),
              );
              resp.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Exported bookmarks JSON")
              });
              if resp.has_focus() || resp.clicked() {
                out.unfocus_page = true;
              }
            });
          }
        });

        ui.add_space(8.0);

        section_card(ui, "Import", |ui| {
          ui.label(
            egui::RichText::new("Import replaces all bookmarks.")
              .small()
              .color(ui.visuals().warn_fg_color),
          );

          ui.add_space(6.0);
          ui.label(
            egui::RichText::new("Import path (optional)")
              .small()
              .color(ui.visuals().weak_text_color()),
          );

          let resp = ui.add(
            egui::TextEdit::singleline(&mut state.import_path)
              .hint_text(profile_path.display().to_string())
              .desired_width(f32::INFINITY),
          );
          resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Import path"));
          if resp.has_focus() || resp.clicked() {
            out.unfocus_page = true;
          }

          ui.horizontal_wrapped(|ui| {
            if ui.button("Use profile path").clicked() {
              state.import_path = profile_path.display().to_string();
            }

            let import_btn =
              ui.add_enabled(!state.io_job.is_busy(), egui::Button::new("Import file"));
            if import_btn.clicked() {
              let raw = state.import_path.trim();
              if raw.is_empty() {
                state.error = Some("Import path is empty.".to_string());
              } else {
                if let Err(err) = state.io_job.start_import(raw.to_string()) {
                  state.error = Some(err);
                }
              }
            }

            if state.io_job.is_importing() {
              ui.label(
                egui::RichText::new("Working…")
                  .small()
                  .color(ui.visuals().weak_text_color()),
              );
            }
          });

          ui.add_space(6.0);

          let resp = ui.add(
            egui::TextEdit::multiline(&mut state.import_json)
              .code_editor()
              .desired_rows(6)
              .hint_text("Paste bookmarks JSON here…")
              .desired_width(f32::INFINITY),
          );
          resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Import bookmarks JSON")
          });
          if resp.has_focus() || resp.clicked() {
            out.unfocus_page = true;
          }

          ui.horizontal_wrapped(|ui| {
            if ui.button("Import").clicked() {
              match BookmarkStore::from_json_str_migrating(&state.import_json) {
                Ok((imported, migration)) => {
                  let delta = BookmarkDelta::ReplaceAll(imported);
                  match store.apply_delta(&delta) {
                    Ok(()) => {
                      out.bookmark_deltas.push(delta);
                      out.changed = true;
                      out.request_flush = true;
                      state.error = None;
                      state.message = Some(format!("Imported bookmarks ({migration:?})."));
                      state.import_json.clear();
                      state.clear_transient();
                    }
                    Err(err) => {
                      state.error = Some(format!("Failed to import bookmarks: {err:?}"));
                    }
                  }
                }
                Err(err) => {
                  state.error = Some(format!("Failed to import bookmarks: {err:?}"));
                }
              }
            }
            if ui.button("Clear").clicked() {
              state.import_json.clear();
            }
          });
        });
      });

      ui.add_space(ui.spacing().item_spacing.y.max(6.0));
      ui.separator();

      // -----------------------------------------------------------------------
      // Bookmarks list
      // -----------------------------------------------------------------------
      folder_cache.ensure_up_to_date(store);
      {
        let folder_labels = &folder_cache.folder_labels;
        bookmarks_list(ui, state, store, folder_labels, &mut out);
      }
    });

  state.folder_cache = Some(folder_cache);

  // Clear edit state if the underlying node disappeared.
  if state
    .editing_bookmark
    .as_ref()
    .is_some_and(|edit| !store.nodes.contains_key(&edit.id))
  {
    state.editing_bookmark = None;
  }

  out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalloutKind {
  Message,
  Error,
}

fn callout_dismissible(
  ui: &mut egui::Ui,
  kind: CalloutKind,
  text: &str,
  dismiss_label: &'static str,
) -> bool {
  let visuals = ui.visuals();
  let (icon, stroke, fill) = match kind {
    CalloutKind::Message => {
      let stroke = visuals.selection.stroke;
      let fill = visuals.selection.bg_fill;
      (BrowserIcon::Info, stroke, fill)
    }
    CalloutKind::Error => {
      let color = visuals.error_fg_color;
      let fill = with_alpha(color, 0.12);
      let stroke = egui::Stroke::new(visuals.selection.stroke.width.max(1.0), color);
      (BrowserIcon::Error, stroke, fill)
    }
  };

  let mut dismissed = false;
  egui::Frame::none()
    .fill(fill)
    .stroke(stroke)
    .rounding(visuals.widgets.inactive.rounding)
    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
    .show(ui, |ui| {
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let dismiss = icon_button(ui, BrowserIcon::Close, "Dismiss", true);
        dismiss.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, dismiss_label));
        dismissed = dismiss.clicked();

        ui.add_space(6.0);
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
          icon_tinted(ui, icon, ui.spacing().icon_width, stroke.color);
          ui.label(text);
        });
      });
    });

  dismissed
}

fn callout(ui: &mut egui::Ui, kind: CalloutKind, text: &str) {
  let visuals = ui.visuals();
  let (icon, stroke, fill) = match kind {
    CalloutKind::Message => {
      let stroke = visuals.selection.stroke;
      let fill = visuals.selection.bg_fill;
      (BrowserIcon::Info, stroke, fill)
    }
    CalloutKind::Error => {
      let color = visuals.error_fg_color;
      let fill = with_alpha(color, 0.12);
      let stroke = egui::Stroke::new(visuals.selection.stroke.width.max(1.0), color);
      (BrowserIcon::Error, stroke, fill)
    }
  };

  egui::Frame::none()
    .fill(fill)
    .stroke(stroke)
    .rounding(visuals.widgets.inactive.rounding)
    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
    .show(ui, |ui| {
      ui.horizontal(|ui| {
        icon_tinted(ui, icon, ui.spacing().icon_width, stroke.color);
        ui.label(text);
      });
    });
}

fn section_card(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
  let visuals = ui.visuals();
  egui::Frame::none()
    .fill(visuals.widgets.inactive.bg_fill)
    .stroke(visuals.widgets.inactive.bg_stroke)
    .rounding(visuals.widgets.inactive.rounding)
    .inner_margin(egui::Margin::symmetric(12.0, 10.0))
    .show(ui, |ui| {
      ui.label(egui::RichText::new(title).strong());
      ui.add_space(6.0);
      add_contents(ui);
    });
}

fn search_bar(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  out: &mut BookmarksManagerOutput,
) {
  let search_out = panel_search_field(
    ui,
    "bookmarks_manager_search",
    &mut state.search,
    "Search bookmarks…",
    &mut state.request_focus_search,
    a11y::BOOKMARKS_MANAGER_SEARCH_LABEL,
  );

  if search_out.focus_requested
    || search_out.response.has_focus()
    || search_out.response.clicked()
    || search_out.cleared
  {
    out.unfocus_page = true;
  }
}

fn create_folder_card(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  store: &mut BookmarkStore,
  folder_options: &[(Option<BookmarkId>, String)],
  out: &mut BookmarksManagerOutput,
) {
  let Some(mut create) = state.creating_folder.take() else {
    return;
  };

  let mut create_clicked = false;
  let mut cancel_clicked = false;

  section_card(ui, "Create folder", |ui| {
    ui.label(
      egui::RichText::new("Folder name")
        .small()
        .color(ui.visuals().weak_text_color()),
    );

    let title_id = ui.make_persistent_id("create_folder_title");
    if create.request_focus_title {
      ui.memory_mut(|mem| mem.request_focus(title_id));
      create.request_focus_title = false;
      out.unfocus_page = true;
    }
    let resp = ui.add(
      egui::TextEdit::singleline(&mut create.title)
        .id(title_id)
        .hint_text("Untitled folder")
        .desired_width(f32::INFINITY),
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Folder name"));
    if resp.has_focus() || resp.clicked() {
      out.unfocus_page = true;
    }

    ui.add_space(6.0);
    ui.label(
      egui::RichText::new("Parent folder")
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    folder_combo_box(
      ui,
      "create_folder_parent",
      folder_options,
      &mut create.parent,
      "Parent folder",
    );

    if let Some(err) = create.error.as_deref().filter(|s| !s.trim().is_empty()) {
      ui.add_space(6.0);
      callout(ui, CalloutKind::Error, err);
    }

    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      let primary = primary_button(ui, "Create", !create.title.trim().is_empty());
      if primary.clicked() {
        create_clicked = true;
      }
      if ui.button("Cancel").clicked() {
        cancel_clicked = true;
      }
    });
  });

  if cancel_clicked {
    state.creating_folder = None;
  } else if create_clicked {
    let mut deltas = Vec::new();
    match store.create_folder_with_deltas(create.title.clone(), create.parent, &mut deltas) {
      Ok(_) => {
        out.changed = true;
        out.bookmark_deltas.extend(deltas);
        state.error = None;
        state.message = Some("Folder created.".to_string());
        state.creating_folder = None;
      }
      Err(err) => {
        create.error = Some(format!("{err:?}"));
        state.creating_folder = Some(create);
      }
    }
  } else {
    state.creating_folder = Some(create);
  }
}

fn edit_bookmark_card(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  store: &mut BookmarkStore,
  folder_options: &[(Option<BookmarkId>, String)],
  out: &mut BookmarksManagerOutput,
) {
  let Some(mut edit) = state.editing_bookmark.take() else {
    return;
  };

  let mut save_clicked = false;
  let mut cancel_clicked = false;

  section_card(ui, "Edit bookmark", |ui| {
    ui.label(
      egui::RichText::new("Title")
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    let title_id = ui.make_persistent_id(("edit_bookmark_title", edit.id.0));
    if edit.request_focus_title {
      ui.memory_mut(|mem| mem.request_focus(title_id));
      edit.request_focus_title = false;
      out.unfocus_page = true;
    }
    let resp = ui.add(
      egui::TextEdit::singleline(&mut edit.title)
        .id(title_id)
        .hint_text("Optional")
        .desired_width(f32::INFINITY),
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Bookmark title"));
    if resp.has_focus() || resp.clicked() {
      out.unfocus_page = true;
    }

    ui.add_space(6.0);
    ui.label(
      egui::RichText::new("URL")
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    let url_id = ui.make_persistent_id(("edit_bookmark_url", edit.id.0));
    let resp = ui.add(
      egui::TextEdit::singleline(&mut edit.url)
        .id(url_id)
        .desired_width(f32::INFINITY),
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Bookmark URL"));
    if resp.has_focus() || resp.clicked() {
      out.unfocus_page = true;
    }

    ui.add_space(6.0);
    ui.label(
      egui::RichText::new("Folder")
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    folder_combo_box(
      ui,
      format!("edit_parent_{}", edit.id.0),
      folder_options,
      &mut edit.parent,
      "Folder",
    );

    if let Some(err) = edit.error.as_deref().filter(|s| !s.trim().is_empty()) {
      ui.add_space(6.0);
      callout(ui, CalloutKind::Error, err);
    }

    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      let primary = primary_button(ui, "Save", true);
      if primary.clicked() {
        save_clicked = true;
      }
      if ui.button("Cancel").clicked() {
        cancel_clicked = true;
      }
    });
  });

  if cancel_clicked {
    state.editing_bookmark = None;
    return;
  }

  if save_clicked {
    let title = normalize_optional_string(&edit.title);
    let mut deltas = Vec::new();
    match store.update_with_deltas(edit.id, title, edit.url.clone(), edit.parent, &mut deltas) {
      Ok(()) => {
        out.changed = true;
        out.bookmark_deltas.extend(deltas);
        state.editing_bookmark = None;
        state.error = None;
        state.message = Some("Bookmark updated.".to_string());
      }
      Err(err) => {
        edit.error = Some(format!("{err:?}"));
        state.editing_bookmark = Some(edit);
      }
    }
    return;
  }

  state.editing_bookmark = Some(edit);
}

fn primary_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
  let visuals = ui.visuals();
  ui.add_enabled(
    enabled,
    egui::Button::new(label)
      .fill(visuals.selection.bg_fill)
      .stroke(visuals.selection.stroke),
  )
}

fn bookmarks_list(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  store: &mut BookmarkStore,
  folder_labels: &HashMap<Option<BookmarkId>, String>,
  out: &mut BookmarksManagerOutput,
) {
  let visuals = ui.visuals();
  let frame = egui::Frame::none()
    .fill(visuals.widgets.inactive.bg_fill)
    .stroke(visuals.widgets.inactive.bg_stroke)
    .rounding(visuals.widgets.inactive.rounding)
    .inner_margin(egui::Margin::same(0.0));

  frame.show(ui, |ui| {
    let query = state.search.trim();
    let cache_revision = if query.is_empty() {
      store.structure_revision()
    } else {
      store.revision()
    };
    let open_rev = state.folder_open_revision;

    if store.roots.is_empty() {
      // Free the (potentially large) cached row vector when the store is empty.
      state.list_cache = None;
      panel_empty_state(
        ui,
        BrowserIcon::BookmarkOutline,
        "No bookmarks",
        Some("Press Ctrl/Cmd+D to bookmark the current page."),
        None,
      );
      return;
    }

    let needs_rebuild = match state.list_cache.as_ref() {
      Some(cache) => {
        cache.cache_revision != cache_revision
          || cache.search_query != query
          || cache.folder_open_revision != open_rev
      }
      None => true,
    };

    // Move the cache out temporarily so row rendering can mutably borrow `state` without fighting
    // Rust's borrow checker.
    let prev_cache = state.list_cache.take();
    let mut cache = if needs_rebuild {
      BookmarksListCache {
        cache_revision,
        search_query: query.to_string(),
        folder_open_revision: open_rev,
        rows: build_visible_rows(ui.ctx(), store, query, prev_cache.as_ref()),
      }
    } else {
      prev_cache.expect("list cache present when not rebuilding")
    };

    if !query.is_empty() && cache.rows.is_empty() {
      let empty = panel_empty_state(
        ui,
        BrowserIcon::Search,
        "No matches",
        Some("Try a different search."),
        Some("Clear search"),
      );
      if empty
        .action_response
        .as_ref()
        .is_some_and(|resp| resp.clicked())
      {
        state.search.clear();
        state.request_focus_search = true;
        out.unfocus_page = true;
      }
      state.list_cache = Some(cache);
      return;
    }

    let row_height = list_row_height(ui);

    egui::ScrollArea::vertical()
      .auto_shrink([false, false])
      .show_rows(ui, row_height, cache.rows.len(), |ui, row_range| {
        for row_idx in row_range {
          let row = cache.rows[row_idx];
          match row.kind {
            BookmarkRowKind::Folder => {
              let delete_clicked = match store.nodes.get(&row.id) {
                Some(BookmarkNode::Folder(folder)) => {
                  render_folder_row(
                    ui,
                    state,
                    row.id,
                    folder.title.as_str(),
                    folder.children.len(),
                    row.depth,
                  )
                }
                _ => {
                  // Keep row spacing stable until the cache is rebuilt next frame.
                  list_row(ui, ("missing_folder_row", row.id.0), false, |_| {});
                  continue;
                }
              };

              if delete_clicked {
                let mut deltas = Vec::new();
                if store.remove_by_id_with_deltas(row.id, &mut deltas) {
                  out.changed = true;
                  out.bookmark_deltas.extend(deltas);
                  out.request_flush = true;
                  state.clear_transient();
                }
              }
            }
            BookmarkRowKind::Bookmark => {
              let delete_clicked = match store.nodes.get(&row.id) {
                Some(BookmarkNode::Bookmark(entry)) => {
                  let parent_label = folder_labels
                    .get(&entry.parent)
                    .map(String::as_str)
                    .unwrap_or("Root");
                  render_bookmark_row(ui, state, entry, row.depth, parent_label, out)
                }
                _ => {
                  list_row(ui, ("missing_bookmark_row", row.id.0), false, |_| {});
                  continue;
                }
              };

              if delete_clicked {
                let mut deltas = Vec::new();
                if store.remove_by_id_with_deltas(row.id, &mut deltas) {
                  out.changed = true;
                  out.bookmark_deltas.extend(deltas);
                  out.request_flush = true;
                  state.clear_transient();
                }
              }
            }
          }
        }
      });

    state.list_cache = Some(cache);
  });
}

// ---------------------------------------------------------------------------
// Virtualized tree rendering
// ---------------------------------------------------------------------------

fn folder_open_id(folder_id: BookmarkId) -> egui::Id {
  egui::Id::new(("fastr_bookmarks_manager_folder_open", folder_id.0))
}

fn folder_open(ctx: &egui::Context, folder_id: BookmarkId) -> bool {
  ctx
    .data_mut(|d| d.get_persisted::<bool>(folder_open_id(folder_id)))
    .unwrap_or(false)
}

fn filter_search_rows(
  store: &BookmarkStore,
  query: &str,
  prev_rows: &[BookmarkRow],
) -> Vec<BookmarkRow> {
  // Keep matching behavior consistent with `BookmarkStore::search`.
  let query_lower: Cow<'_, str> = if query.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
    Cow::Owned(query.to_ascii_lowercase())
  } else {
    Cow::Borrowed(query)
  };
  let tokens: Vec<&str> = query_lower
    .split_whitespace()
    .filter(|t| !t.is_empty())
    .collect();
  if tokens.is_empty() {
    return Vec::new();
  }

  let mut out = Vec::with_capacity(prev_rows.len());
  'rows: for row in prev_rows {
    let BookmarkRowKind::Bookmark = row.kind else {
      continue;
    };
    let Some(BookmarkNode::Bookmark(entry)) = store.nodes.get(&row.id) else {
      continue;
    };

    let url = entry.url.trim();
    if url.is_empty() {
      continue;
    }
    let title = entry
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty());

    for token_lower in &tokens {
      if !contains_ascii_case_insensitive(url, token_lower)
        && !title.is_some_and(|t| contains_ascii_case_insensitive(t, token_lower))
      {
        continue 'rows;
      }
    }

    out.push(BookmarkRow {
      kind: BookmarkRowKind::Bookmark,
      id: row.id,
      depth: 0,
    });
  }

  out
}

fn build_visible_rows(
  ctx: &egui::Context,
  store: &BookmarkStore,
  query: &str,
  prev_cache: Option<&BookmarksListCache>,
) -> Vec<BookmarkRow> {
  if !query.is_empty() {
    if let Some(prev) = prev_cache {
      if prev.cache_revision == store.revision()
        && !prev.search_query.is_empty()
        && query.starts_with(prev.search_query.as_str())
      {
        return filter_search_rows(store, query, &prev.rows);
      }
    }

    let ids = store.search(query, usize::MAX);
    return ids
      .into_iter()
      .map(|id| BookmarkRow {
        kind: BookmarkRowKind::Bookmark,
        id,
        depth: 0,
      })
      .collect();
  }

  // Avoid preallocating `store.nodes.len()` here: large bookmark stores can have many nodes hidden
  // behind collapsed folders, and the flattened list is only as big as the *visible* rows.
  let mut out: Vec<BookmarkRow> = Vec::with_capacity(store.roots.len().max(256));
  // Depth-first traversal: push roots in reverse so `pop()` visits them in store order.
  let mut stack: Vec<(usize, BookmarkId)> = store
    .roots
    .iter()
    .rev()
    .copied()
    .map(|id| (0, id))
    .collect();
  while let Some((depth, id)) = stack.pop() {
    let Some(node) = store.nodes.get(&id) else {
      continue;
    };

    match node {
      BookmarkNode::Bookmark(_) => out.push(BookmarkRow {
        kind: BookmarkRowKind::Bookmark,
        id,
        depth,
      }),
      BookmarkNode::Folder(folder) => {
        out.push(BookmarkRow {
          kind: BookmarkRowKind::Folder,
          id,
          depth,
        });

        if folder_open(ctx, folder.id) {
          // Make the common case (opening a large folder) cheaper by reserving for its direct
          // children. This avoids repeated reallocations while keeping collapsed trees cheap.
          out.reserve(folder.children.len());
          stack.reserve(folder.children.len());
          stack.extend(
            folder
              .children
              .iter()
              .rev()
              .copied()
              .map(|child| (depth.saturating_add(1), child)),
          );
        }
      }
    }
  }

  out
}

fn render_folder_row(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  folder_id: BookmarkId,
  title: &str,
  item_count: usize,
  depth: usize,
) -> bool {
  let open_id = folder_open_id(folder_id);
  let mut open = ui
    .ctx()
    .data_mut(|d| d.get_persisted::<bool>(open_id))
    .unwrap_or(false);

  let mut delete_clicked = false;

  let row_resp = list_row(ui, ("folder_row", folder_id.0), false, |ui| {
    let indent = depth as f32 * ui.spacing().indent;
    ui.horizontal(|ui| {
      ui.add_space(indent);
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let del = icon_button(ui, BrowserIcon::Trash, "Delete folder", true);
        del.widget_info({
          let label = format!("Delete folder: {title}");
          move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
        });
        if del.clicked() {
          delete_clicked = true;
        }

        ui.add_space(6.0);
        ui.vertical(|ui| {
          ui.set_width(ui.available_width());
          let arrow = if open { "▾" } else { "▸" };
          ui.label(egui::RichText::new(format!("{arrow} {title}")).strong());
          ui.label(
            egui::RichText::new(format!(
              "{item_count} item{}",
              if item_count == 1 { "" } else { "s" }
            ))
            .small()
            .color(ui.visuals().weak_text_color()),
          );
        });
      });
    });
  });

  row_resp.widget_info({
    let label = if open {
      format!("Collapse folder: {title}")
    } else {
      format!("Expand folder: {title}")
    };
    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
  });

  if delete_clicked {
    return true;
  }

  if row_resp.clicked() {
    open = !open;
    ui.ctx().data_mut(|d| d.insert_persisted(open_id, open));
    // Rebuild the flattened representation next frame.
    state.folder_open_revision = state.folder_open_revision.wrapping_add(1);
    ui.ctx().request_repaint();
  }

  false
}

fn render_bookmark_row(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  entry: &super::bookmarks::BookmarkEntry,
  depth: usize,
  parent_label: &str,
  out: &mut BookmarksManagerOutput,
) -> bool {
  let editing_this = state
    .editing_bookmark
    .as_ref()
    .is_some_and(|edit| edit.id == entry.id);

  let title = entry
    .title
    .as_deref()
    .map(str::trim)
    .filter(|t| !t.is_empty())
    .unwrap_or(entry.url.as_str())
    .to_string();

  let url_display = crate::ui::url_display::truncate_url_middle(&entry.url, 80);
  let folder_display = truncate_middle(parent_label, 48);

  let mut open_new_tab_clicked = false;
  let mut edit_clicked = false;
  let mut delete_clicked = false;

  let row_resp = list_row(ui, ("bookmark_row", entry.id.0), editing_this, |ui| {
    let indent = depth as f32 * ui.spacing().indent;
    ui.horizontal(|ui| {
      ui.add_space(indent);
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let del = icon_button(ui, BrowserIcon::Trash, "Delete bookmark", true);
        del.widget_info({
          let label = format!("Delete bookmark: {title}");
          move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
        });
        if del.clicked() {
          delete_clicked = true;
        }

        let edit_btn = icon_button(ui, BrowserIcon::Edit, "Edit bookmark", true);
        edit_btn.widget_info({
          let label = format!("Edit bookmark: {title}");
          move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
        });
        if edit_btn.clicked() {
          edit_clicked = true;
        }

        let new_tab = icon_button(ui, BrowserIcon::OpenInNewTab, "Open in new tab", true);
        new_tab.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, "Open bookmark in new tab")
        });
        if new_tab.clicked() {
          open_new_tab_clicked = true;
        }

        ui.add_space(8.0);

        ui.vertical(|ui| {
          ui.set_width(ui.available_width());

          let mut title_text = egui::RichText::new(title.clone()).strong();
          if editing_this {
            title_text = title_text.color(ui.visuals().selection.stroke.color);
          }
          ui.label(title_text);
          ui.label(
            egui::RichText::new(url_display.clone())
              .small()
              .color(ui.visuals().weak_text_color()),
          );
          ui.label(
            egui::RichText::new(folder_display.clone())
              .small()
              .color(ui.visuals().weak_text_color()),
          );
        });
      });
    });
  })
  .on_hover_text(entry.url.clone());

  row_resp.widget_info({
    let label = format!("Open bookmark: {title} ({})", entry.url);
    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
  });

  if row_resp.clicked() {
    out
      .actions
      .push(BookmarksManagerAction::Open(entry.url.clone()));
  }

  if open_new_tab_clicked {
    out
      .actions
      .push(BookmarksManagerAction::OpenInNewTab(entry.url.clone()));
  }

  if edit_clicked {
    state.editing_bookmark = Some(EditBookmarkState {
      id: entry.id,
      title: entry.title.clone().unwrap_or_default(),
      url: entry.url.clone(),
      parent: entry.parent,
      error: None,
      request_focus_title: true,
    });
    out.unfocus_page = true;
  }

  delete_clicked
}

fn folder_options(store: &BookmarkStore) -> Vec<(Option<BookmarkId>, String)> {
  let mut out = Vec::new();
  out.push((None, "Root".to_string()));

  for (id, path) in store.folders_in_display_order_joined() {
    out.push((Some(id), path));
  }
  out
}

fn folder_combo_box(
  ui: &mut egui::Ui,
  id_source: impl std::hash::Hash,
  options: &[(Option<BookmarkId>, String)],
  value: &mut Option<BookmarkId>,
  a11y_label: &'static str,
) {
  // Ensure a stable ID for both the combo box and its virtualized scroll area.
  let combo_id = ui.make_persistent_id(id_source);
  let selected = options
    .iter()
    .find(|(id, _)| id == value)
    .map(|(_, label)| label.as_str())
    .unwrap_or("Root");
  let selected = truncate_middle(selected, 48);
  let response = egui::ComboBox::from_id_source(combo_id)
    .selected_text(selected)
    .show_ui(ui, |ui| {
      // Virtualize the dropdown: large bookmark trees can contain thousands of folders and rendering
      // them all every frame (while the popup is open) can cause noticeable jank.
      let row_height = ui.spacing().interact_size.y.max(18.0);
      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(row_height * 12.0)
        .id_source((combo_id, "popup"))
        .show_rows(ui, row_height, options.len(), |ui, row_range| {
          for idx in row_range {
            let (id, label) = &options[idx];
            ui.selectable_value(value, *id, label);
          }
        });
    })
    .response;
  response.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y_label));
}

fn normalize_optional_string(raw: &str) -> Option<String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    None
  } else {
    Some(trimmed.to_string())
  }
}

fn truncate_middle(s: &str, max_chars: usize) -> String {
  let s = s.trim();
  if max_chars == 0 || s.is_empty() {
    return String::new();
  }
  let len = s.chars().count();
  if len <= max_chars {
    return s.to_string();
  }
  if max_chars <= 1 {
    return "…".to_string();
  }
  let head = max_chars / 2;
  let tail = max_chars - head - 1;
  let mut out = String::new();
  out.extend(s.chars().take(head));
  out.push('…');
  // Avoid allocating a full `Vec<char>` for long strings; only keep the tail slice we need.
  let mut tail_rev = String::new();
  tail_rev.extend(s.chars().rev().take(tail));
  out.extend(tail_rev.chars().rev());
  out
}

fn with_alpha(color: egui::Color32, alpha: f32) -> egui::Color32 {
  let [r, g, b, a] = color.to_array();
  let a = ((a as f32) * alpha).round().clamp(0.0, 255.0) as u8;
  egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
  a + (b - a) * t
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
  lerp(a as f32, b as f32, t).round().clamp(0.0, 255.0) as u8
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
  let [ar, ag, ab, aa] = a.to_array();
  let [br, bg, bb, ba] = b.to_array();
  egui::Color32::from_rgba_unmultiplied(
    lerp_u8(ar, br, t),
    lerp_u8(ag, bg, t),
    lerp_u8(ab, bb, t),
    lerp_u8(aa, ba, t),
  )
}

fn list_row_height(ui: &egui::Ui) -> f32 {
  (ui.spacing().interact_size.y * 2.2).max(56.0)
}

fn list_row(
  ui: &mut egui::Ui,
  id_source: impl std::hash::Hash,
  selected: bool,
  add_contents: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
  let row_id = ui.make_persistent_id(id_source);
  let width = ui.available_width().max(0.0);
  let min_height = list_row_height(ui);
  let (_alloc_id, rect) = ui.allocate_space(egui::vec2(width, min_height));
  let response = ui.interact(rect, row_id, egui::Sense::click());

  let visuals = ui.visuals();
  let motion = UiMotion::from_ctx(ui.ctx());
  let hover_t = motion.animate_bool(
    ui.ctx(),
    row_id.with("hover"),
    ui.is_enabled() && response.hovered(),
    motion.durations.hover_fade,
  );

  let base_fill = if selected {
    visuals.selection.bg_fill
  } else {
    visuals.widgets.inactive.bg_fill
  };
  let hover_fill = if selected {
    visuals.selection.bg_fill
  } else {
    visuals.widgets.hovered.bg_fill
  };
  let fill = lerp_color(base_fill, hover_fill, hover_t);

  if ui.is_rect_visible(rect) {
    ui.painter().rect_filled(rect, 0.0, fill);
    ui.painter().line_segment(
      [
        egui::pos2(rect.left(), rect.bottom()),
        egui::pos2(rect.right(), rect.bottom()),
      ],
      egui::Stroke::new(1.0, visuals.widgets.inactive.bg_stroke.color),
    );

    if response.has_focus() {
      let stroke = visuals.selection.stroke;
      ui.painter()
        .rect_stroke(rect.shrink(1.0), egui::Rounding::same(0.0), stroke);
    }
  }

  let inner = rect.shrink2(egui::vec2(12.0, 8.0));
  ui.allocate_ui_at_rect(inner, |ui| {
    ui.set_width(inner.width());
    add_contents(ui);
  });

  response
}
