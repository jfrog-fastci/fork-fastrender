#![cfg(feature = "browser_ui")]

//! Bookmarks manager UI for the windowed browser frontend.
//!
//! This is implemented as an `egui::SidePanel` so it does not overlap the rendered page image
//! (which keeps page hit-testing/pointer forwarding simple).

use std::borrow::Cow;
use std::time::Duration;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::ui::bookmarks_io_job::{BookmarksIoJob, BookmarksIoJobUpdate};
use crate::ui::motion::UiMotion;

use super::{
  a11y, a11y_labels, icon_button, icon_tinted, panel_empty_state, panel_header, panel_search_field,
  BookmarkDelta, BookmarkId, BookmarkNode, BookmarkStore, BrowserIcon,
};

use super::string_match::contains_ascii_case_insensitive;

#[derive(Debug, Clone)]
pub enum BookmarksManagerAction {
  Open(String),
  OpenInNewTab(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookmarksManagerRequest {
  /// Request that the windowed browser open a native file picker dialog and, on success, populate
  /// [`BookmarksManagerState::import_path`].
  RequestOpenImportDialog,
  /// Request that the windowed browser open a native save-file dialog and, on success, populate
  /// [`BookmarksManagerState::export_path`].
  RequestOpenExportDialog,
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

  /// Apply the result of a native file picker dialog for the import path.
  ///
  /// Returns `true` if `import_path` changed.
  pub fn apply_import_dialog_selection(&mut self, path: Option<std::path::PathBuf>) -> bool {
    let Some(path) = path else {
      return false;
    };
    let new_value = path.display().to_string();
    if self.import_path == new_value {
      return false;
    }
    self.import_path = new_value;
    true
  }

  /// Apply the result of a native save-file dialog for the export path.
  ///
  /// Returns `true` if `export_path` changed.
  pub fn apply_export_dialog_selection(&mut self, path: Option<std::path::PathBuf>) -> bool {
    let Some(path) = path else {
      return false;
    };
    let new_value = path.display().to_string();
    if self.export_path == new_value {
      return false;
    }
    self.export_path = new_value;
    true
  }

  pub fn clear_transient(&mut self) {
    self.creating_folder = None;
    self.editing_bookmark = None;
  }

  fn poll_io_job(
    &mut self,
    ctx: &egui::Context,
    store: &mut BookmarkStore,
    out: &mut BookmarksManagerOutput,
  ) {
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
      BookmarksIoJobUpdate::ExportJsonFinished { result } => match result {
        Ok(json) => {
          self.export_json = Some(json.clone());
          ctx.output_mut(|o| o.copied_text = json);
          self.error = None;
          self.message = Some("Exported bookmarks JSON copied to clipboard.".to_string());
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
      BookmarksIoJobUpdate::ImportJsonFinished { result } => match result {
        Ok((imported, migration)) => {
          let delta = BookmarkDelta::ReplaceAll(imported);
          match store.apply_delta(&delta) {
            Ok(()) => {
              out.bookmark_deltas.push(delta);
              out.changed = true;
              out.request_flush = true;
              self.error = None;
              self.message = Some(format!("Imported bookmarks from JSON ({migration:?})."));
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
  folder_label_indices: FxHashMap<Option<BookmarkId>, usize>,
}

impl BookmarksFolderCache {
  fn new(store: &BookmarkStore) -> Self {
    let folder_options = folder_options(store);
    let mut folder_label_indices = FxHashMap::with_capacity(folder_options.len());
    for (idx, (id, _)) in folder_options.iter().enumerate() {
      folder_label_indices.insert(*id, idx);
    }
    Self {
      folder_revision: store.folder_revision(),
      folder_options,
      folder_label_indices,
    }
  }

  fn ensure_up_to_date(&mut self, store: &BookmarkStore) {
    let revision = store.folder_revision();
    if self.folder_revision == revision {
      return;
    }
    self.folder_revision = revision;
    self.folder_options = folder_options(store);
    let mut indices = FxHashMap::with_capacity(self.folder_options.len());
    for (idx, (id, _)) in self.folder_options.iter().enumerate() {
      indices.insert(*id, idx);
    }
    self.folder_label_indices = indices;
  }

  fn label_for_parent(&self, parent: Option<BookmarkId>) -> &str {
    let idx = self.folder_label_indices.get(&parent).copied().unwrap_or(0);
    self
      .folder_options
      .get(idx)
      .map(|(_, label)| label.as_str())
      .unwrap_or("Root")
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
  pub requests: Vec<BookmarksManagerRequest>,
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
  state.poll_io_job(ctx, store, &mut out);

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
          create_folder_card(ui, state, store, &folder_cache, &mut out);
        }
        folder_cache.ensure_up_to_date(store);
        ui.add_space(ui.spacing().item_spacing.y.max(8.0));
      }

      if state.editing_bookmark.is_some() {
        {
          edit_bookmark_card(ui, state, store, &folder_cache, &mut out);
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
            let export_json_btn = ui.add_enabled(
              !state.io_job.is_busy(),
              egui::Button::new("Copy JSON to clipboard"),
            );
            if export_json_btn.clicked() {
              if let Err(err) = state.io_job.start_export_json(store.clone()) {
                state.error = Some(err);
              }
            }
            if state.io_job.is_exporting_json() {
              ui.label(
                egui::RichText::new("Working…")
                  .small()
                  .color(ui.visuals().weak_text_color()),
              );
            }
            if let Some(json) = state.export_json.as_ref() {
              let copy_last = ui.add_enabled(
                !state.io_job.is_busy(),
                egui::Button::new("Copy last export"),
              );
              if copy_last.clicked() {
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

          ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let choose_resp = ui.small_button("Choose file…");
            choose_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Choose export file")
            });
            if choose_resp.clicked() {
              out
                .requests
                .push(BookmarksManagerRequest::RequestOpenExportDialog);
            }

            ui.add_space(6.0);

            let resp = ui.add(
              egui::TextEdit::singleline(&mut state.export_path)
                .hint_text(profile_path.display().to_string())
                .desired_width(f32::INFINITY),
            );
            resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Export path"));
            if resp.has_focus() || resp.clicked() {
              out.unfocus_page = true;
            }
          });

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

          ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let choose_resp = ui.small_button("Choose file…");
            choose_resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Choose import file")
            });
            if choose_resp.clicked() {
              out
                .requests
                .push(BookmarksManagerRequest::RequestOpenImportDialog);
            }

            ui.add_space(6.0);

            let resp = ui.add(
              egui::TextEdit::singleline(&mut state.import_path)
                .hint_text(profile_path.display().to_string())
                .desired_width(f32::INFINITY),
            );
            resp
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Import path"));
            if resp.has_focus() || resp.clicked() {
              out.unfocus_page = true;
            }
          });

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
            let import_btn = ui.add_enabled(!state.io_job.is_busy(), egui::Button::new("Import"));
            if import_btn.clicked() {
              if state.import_json.trim().is_empty() {
                state.error = Some("Import JSON is empty.".to_string());
              } else if let Err(err) =
                state.io_job.start_import_json(state.import_json.clone())
              {
                state.error = Some(err);
              }
            }
            if state.io_job.is_importing_json() {
              ui.label(
                egui::RichText::new("Working…")
                  .small()
                  .color(ui.visuals().weak_text_color()),
              );
            }
            let clear_btn = ui.add_enabled(!state.io_job.is_busy(), egui::Button::new("Clear"));
            if clear_btn.clicked() {
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
        bookmarks_list(ui, state, store, &folder_cache, &mut out);
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
  if search_out.request_close {
    out.close_requested = true;
  }

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
  folder_cache: &BookmarksFolderCache,
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
      folder_cache,
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
  folder_cache: &BookmarksFolderCache,
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
      folder_cache,
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
  folder_cache: &BookmarksFolderCache,
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
        Some("Press Ctrl+D (Win/Linux) or Cmd+D (macOS) to bookmark the current page."),
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
    let mut prev_cache = state.list_cache.take();
    let mut cache = if needs_rebuild {
      BookmarksListCache {
        cache_revision,
        search_query: query.to_string(),
        folder_open_revision: open_rev,
        rows: build_visible_rows(ui.ctx(), store, query, prev_cache.take()),
      }
    } else {
      prev_cache.expect("list cache present when not rebuilding") // fastrender-allow-unwrap
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
                Some(BookmarkNode::Folder(folder)) => render_folder_row(
                  ui,
                  state,
                  row.id,
                  folder.title.as_str(),
                  folder.children.len(),
                  row.depth,
                ),
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
                  let parent_label = folder_cache.label_for_parent(entry.parent);
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
    .data(|d| d.get_persisted::<bool>(folder_open_id(folder_id)))
    .unwrap_or(false)
}

fn filter_search_rows_in_place(store: &BookmarkStore, query: &str, rows: &mut Vec<BookmarkRow>) {
  // Keep matching behavior consistent with `BookmarkStore::search`.
  let query_lower: Cow<'_, str> = if query.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
    Cow::Owned(query.to_ascii_lowercase())
  } else {
    Cow::Borrowed(query)
  };
  let mut tokens_iter = query_lower.split_whitespace().filter(|t| !t.is_empty());
  let Some(first_token) = tokens_iter.next() else {
    rows.clear();
    return;
  };
  let tokens: Option<SmallVec<[&str; 4]>> = tokens_iter.next().map(|second_token| {
    let mut tokens: SmallVec<[&str; 4]> = SmallVec::new();
    tokens.push(first_token);
    tokens.push(second_token);
    tokens.extend(tokens_iter);
    tokens
  });

  rows.retain(|row| {
    if row.kind != BookmarkRowKind::Bookmark {
      return false;
    }
    let Some(BookmarkNode::Bookmark(entry)) = store.nodes.get(&row.id) else {
      return false;
    };

    let url = entry.url.trim();
    if url.is_empty() {
      return false;
    }
    let title = entry
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty());

    if let Some(tokens) = &tokens {
      for token_lower in tokens {
        if !contains_ascii_case_insensitive(url, token_lower)
          && !title.is_some_and(|t| contains_ascii_case_insensitive(t, token_lower))
        {
          return false;
        }
      }
      true
    } else {
      contains_ascii_case_insensitive(url, first_token)
        || title.is_some_and(|t| contains_ascii_case_insensitive(t, first_token))
    }
  });
}

fn build_visible_rows(
  ctx: &egui::Context,
  store: &BookmarkStore,
  query: &str,
  prev_cache: Option<BookmarksListCache>,
) -> Vec<BookmarkRow> {
  if query.is_empty() {
    let mut out: Vec<BookmarkRow> = match prev_cache {
      Some(mut prev) => {
        prev.rows.clear();
        prev.rows
      }
      None => {
        // Avoid preallocating `store.nodes.len()` here: large bookmark stores can have many nodes
        // hidden behind collapsed folders, and the flattened list is only as big as the *visible*
        // rows.
        Vec::with_capacity(store.roots.len().max(256))
      }
    };

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

    return out;
  }

  match prev_cache {
    Some(mut prev) => {
      if prev.cache_revision == store.revision()
        && !prev.search_query.is_empty()
        && query.starts_with(prev.search_query.as_str())
      {
        filter_search_rows_in_place(store, query, &mut prev.rows);
        prev.rows
      } else {
        let ids = store.search(query, usize::MAX);
        prev.rows.clear();
        prev.rows.reserve(ids.len());
        prev.rows.extend(ids.into_iter().map(|id| BookmarkRow {
          kind: BookmarkRowKind::Bookmark,
          id,
          depth: 0,
        }));
        prev.rows
      }
    }
    None => {
      let ids = store.search(query, usize::MAX);
      ids
        .into_iter()
        .map(|id| BookmarkRow {
          kind: BookmarkRowKind::Bookmark,
          id,
          depth: 0,
        })
        .collect()
    }
  }
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
    .data(|d| d.get_persisted::<bool>(open_id))
    .unwrap_or(false);

  let mut delete_clicked = false;

  let row_resp = list_row(ui, ("folder_row", folder_id.0), false, |ui| {
    let indent = depth as f32 * ui.spacing().indent;
    ui.horizontal(|ui| {
      ui.add_space(indent);
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let del = icon_button(ui, BrowserIcon::Trash, "Delete folder", true);
        del.widget_info(move || {
          let label = format!("Delete folder: {title}");
          egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
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

  row_resp.widget_info(move || {
    let label = format!("Folder: {title}");
    egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
  });

  if delete_clicked {
    return true;
  }

  // AccessKit may request explicit expand/collapse actions when the node exposes an expanded state.
  // Prefer Expand when both actions are requested in the same frame.
  let expand_requested =
    ui.input(|i| i.has_accesskit_action_request(row_resp.id, accesskit::Action::Expand));
  let collapse_requested =
    ui.input(|i| i.has_accesskit_action_request(row_resp.id, accesskit::Action::Collapse));

  let mut open_changed = false;
  let mut open_changed_via_a11y = false;
  if expand_requested || collapse_requested {
    let desired = if expand_requested { true } else { false };
    if open != desired {
      open = desired;
      open_changed = true;
      open_changed_via_a11y = true;
    }
  } else if row_resp.clicked() {
    open = !open;
    open_changed = true;
  }

  if open_changed {
    ui.ctx().data_mut(|d| d.insert_persisted(open_id, open));
    // Rebuild the flattened representation next frame.
    state.folder_open_revision = state.folder_open_revision.wrapping_add(1);
    ui.ctx().request_repaint();
    if open_changed_via_a11y {
      row_resp.request_focus();
    }
  }

  // Expose expanded state and explicit Expand/Collapse actions to assistive tech (AccessKit) so
  // screen readers can announce the expanded/collapsed state and request explicit actions.
  let _ = row_resp.ctx.accesskit_node_builder(row_resp.id, |builder| {
    builder.set_expanded(open);
    if open {
      builder.add_action(accesskit::Action::Collapse);
      builder.remove_action(accesskit::Action::Expand);
    } else {
      builder.add_action(accesskit::Action::Expand);
      builder.remove_action(accesskit::Action::Collapse);
    }
  });

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
    .unwrap_or(entry.url.as_str());

  let url_display = crate::ui::url_display::truncate_url_middle_cow(&entry.url, 80);
  let folder_display = truncate_middle(parent_label, 48);

  let mut open_new_tab_clicked = false;
  let mut edit_clicked = false;
  let mut delete_clicked = false;

  let row_resp = list_row(ui, ("bookmark_row", entry.id.0), editing_this, move |ui| {
    let indent = depth as f32 * ui.spacing().indent;
    ui.horizontal(|ui| {
      ui.add_space(indent);
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let del = icon_button(ui, BrowserIcon::Trash, "Delete bookmark", true);
        del.widget_info(move || {
          let label =
            a11y_labels::bookmark_delete_label(entry.title.as_deref(), entry.url.as_str());
          egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
        });
        if del.clicked() {
          delete_clicked = true;
        }

        let edit_btn = icon_button(ui, BrowserIcon::Edit, "Edit bookmark", true);
        edit_btn.widget_info(move || {
          let label = a11y_labels::bookmark_edit_label(entry.title.as_deref(), entry.url.as_str());
          egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
        });
        if edit_btn.clicked() {
          edit_clicked = true;
        }

        let new_tab = icon_button(ui, BrowserIcon::OpenInNewTab, "Open in new tab", true);
        new_tab.widget_info(move || {
          let label =
            a11y_labels::bookmark_open_in_new_tab_label(entry.title.as_deref(), entry.url.as_str());
          egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
        });
        if new_tab.clicked() {
          open_new_tab_clicked = true;
        }

        ui.add_space(8.0);

        ui.vertical(|ui| {
          ui.set_width(ui.available_width());

          let mut title_text = egui::RichText::new(title).strong();
          if editing_this {
            title_text = title_text.color(ui.visuals().selection.stroke.color);
          }
          ui.label(title_text);
          ui.label(
            egui::RichText::new(url_display.as_ref())
              .small()
              .color(ui.visuals().weak_text_color()),
          );
          ui.label(
            egui::RichText::new(folder_display.as_ref())
              .small()
              .color(ui.visuals().weak_text_color()),
          );
        });
      });
    });
  })
  .on_hover_text(entry.url.as_str());

  row_resp.widget_info(move || {
    let label = format!("Open bookmark: {title} ({})", entry.url);
    egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
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
  let folders = store.folders_in_display_order_joined();
  let mut out = Vec::with_capacity(folders.len().saturating_add(1));
  out.push((None, "Root".to_string()));
  out.extend(folders.into_iter().map(|(id, path)| (Some(id), path)));
  out
}

fn folder_combo_box(
  ui: &mut egui::Ui,
  id_source: impl std::hash::Hash,
  folder_cache: &BookmarksFolderCache,
  value: &mut Option<BookmarkId>,
  a11y_label: &'static str,
) {
  // Ensure a stable ID for both the combo box and its virtualized scroll area.
  let combo_id = ui.make_persistent_id(id_source);
  let selected = folder_cache.label_for_parent(*value);
  let selected = truncate_middle(selected, 48);
  let response = egui::ComboBox::from_id_source(combo_id)
    .selected_text(selected.as_ref())
    .show_ui(ui, |ui| {
      // Virtualize the dropdown: large bookmark trees can contain thousands of folders and rendering
      // them all every frame (while the popup is open) can cause noticeable jank.
      let row_height = ui.spacing().interact_size.y.max(18.0);
      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(row_height * 12.0)
        .id_source((combo_id, "popup"))
        .show_rows(ui, row_height, folder_cache.folder_options.len(), |ui, row_range| {
          for idx in row_range {
            let (id, label) = &folder_cache.folder_options[idx];
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

fn truncate_middle<'a>(s: &'a str, max_chars: usize) -> Cow<'a, str> {
  let s = s.trim();
  if max_chars == 0 || s.is_empty() {
    return Cow::Borrowed("");
  }
  let len = s.chars().count();
  if len <= max_chars {
    return Cow::Borrowed(s);
  }
  if max_chars <= 1 {
    return Cow::Borrowed("…");
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
  Cow::Owned(out)
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
  // Scope all nested widgets (buttons, etc.) under a stable per-row ID so their egui IDs (and thus
  // AccessKit node IDs) do not shift when the list is reordered/filtered.
  ui.push_id(row_id, |ui| {
    ui.allocate_ui_at_rect(inner, |ui| {
      ui.set_width(inner.width());
      add_contents(ui);
    });
  });

  response
}

// -----------------------------------------------------------------------------
// Tests (AccessKit / a11y labels)
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::{
    bookmarks_manager_side_panel, BookmarksManagerOutput, BookmarksManagerRequest,
    BookmarksManagerState, CreateFolderState, EditBookmarkState,
  };
  use crate::ui::{a11y_test_util, BookmarkDelta, BookmarkId, BookmarkStore};
  use crate::ui::bookmarks_io_job::{BookmarksIoJob, BookmarksIoJobUpdate};
  use std::sync::mpsc;
  use std::time::{Duration, Instant};

  fn begin_frame(ctx: &egui::Context, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    raw.events = events;
    ctx.begin_frame(raw);
  }

  fn poll_io_job_and_end_frame(
    ctx: &egui::Context,
    state: &mut BookmarksManagerState,
    store: &mut BookmarkStore,
    out: &mut BookmarksManagerOutput,
  ) -> egui::FullOutput {
    begin_frame(ctx, Vec::new());
    state.poll_io_job(ctx, store, out);
    ctx.end_frame()
  }

  fn wait_for_io_job(
    ctx: &egui::Context,
    state: &mut BookmarksManagerState,
    store: &mut BookmarkStore,
    out: &mut BookmarksManagerOutput,
    predicate: impl Fn(&BookmarksManagerState, &BookmarkStore, &BookmarksManagerOutput) -> bool,
  ) -> egui::FullOutput {
    let start = Instant::now();
    loop {
      let output = poll_io_job_and_end_frame(ctx, state, store, out);
      if predicate(state, store, out) {
        return output;
      }
      assert!(
        start.elapsed() < Duration::from_secs(2),
        "timed out waiting for IO job completion"
      );
      std::thread::sleep(Duration::from_millis(5));
    }
  }

  fn render_bookmarks_manager(
    ctx: &egui::Context,
    state: &mut BookmarksManagerState,
    store: &mut BookmarkStore,
  ) -> egui::FullOutput {
    begin_frame(ctx, Vec::new());
    let _ = bookmarks_manager_side_panel(ctx, state, store);
    ctx.end_frame()
  }

  fn accesskit_node_id_by_name(output: &egui::FullOutput, name: &str) -> String {
    let snapshot = a11y_test_util::accesskit_snapshot_from_full_output(output);
    snapshot
      .nodes
      .iter()
      .find(|node| node.name == name)
      .map(|node| node.id.clone())
      .unwrap_or_else(|| {
        panic!(
          "expected AccessKit node named {name:?}.\n\nsnapshot:\n{}",
          a11y_test_util::accesskit_pretty_json_from_full_output(output)
        )
      })
  }

  fn accesskit_node_by_name<'a>(
    output: &'a egui::FullOutput,
    expected_name: &str,
  ) -> (accesskit::NodeId, &'a accesskit::Node) {
    let update = output
      .platform_output
      .accesskit_update
      .as_ref()
      .expect("expected AccessKit update to be emitted");
    update
      .nodes
      .iter()
      .find_map(|(id, node)| {
        let name = node.name().unwrap_or("").trim();
        (name == expected_name).then_some((*id, node))
      })
      .unwrap_or_else(|| {
        panic!(
          "expected AccessKit node named {expected_name:?}.\n\nsnapshot:\n{}",
          a11y_test_util::accesskit_pretty_json_from_full_output(output)
        )
      })
  }

  #[test]
  fn row_action_button_accesskit_id_is_stable_across_reorder() {
    let ctx = egui::Context::default();
    // AccessKit output is typically enabled/disabled by the platform adapter (egui-winit).
    // In headless unit tests we force it on to ensure egui emits an update.
    ctx.enable_accesskit();

    let mut state = BookmarksManagerState::default();
    let mut store = BookmarkStore::default();
    let example_id = store
      .add(
        "https://example.com/".to_string(),
        Some("Example".to_string()),
        None,
      )
      .expect("failed to add Example bookmark");

    let output_a = render_bookmarks_manager(&ctx, &mut state, &mut store);
    let delete_id_a = accesskit_node_id_by_name(&output_a, "Delete bookmark: Example");

    // Mutate the store so the Example row shifts position.
    let first_id = store
      .add(
        "https://a.example/".to_string(),
        Some("A".to_string()),
        None,
      )
      .expect("failed to add A bookmark");
    store
      .reorder_root(&[first_id, example_id])
      .expect("failed to reorder roots");

    let output_b = render_bookmarks_manager(&ctx, &mut state, &mut store);
    let delete_id_b = accesskit_node_id_by_name(&output_b, "Delete bookmark: Example");

    assert_eq!(
      delete_id_a, delete_id_b,
      "expected per-row action button AccessKit node ID to remain stable across list reorder.\n\nbefore:\n{}\n\nafter:\n{}",
      a11y_test_util::accesskit_pretty_json_from_full_output(&output_a),
      a11y_test_util::accesskit_pretty_json_from_full_output(&output_b)
    );
  }

  #[test]
  fn open_in_new_tab_buttons_have_contextual_accessible_names() {
    let ctx = egui::Context::default();
    // AccessKit output is typically enabled/disabled by the platform adapter (egui-winit).
    // In headless unit tests we force it on to ensure egui emits an update.
    ctx.enable_accesskit();

    let mut store = BookmarkStore::default();
    store
      .add(
        "https://example.com".to_string(),
        Some(" Example\nTitle ".to_string()),
        None,
      )
      .expect("bookmark add should succeed");
    store
      .add("https://second.example/path".to_string(), None, None)
      .expect("bookmark add should succeed");

    let mut state = BookmarksManagerState::default();
    begin_frame(&ctx, Vec::new());
    let _out = bookmarks_manager_side_panel(&ctx, &mut state, &mut store);
    let output = ctx.end_frame();

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);

    for expected in [
      // Whitespace in bookmark titles should be collapsed for screen readers.
      "Open bookmark in new tab: Example Title",
      // When the title is missing, the label should fall back to the URL.
      "Open bookmark in new tab: https://second.example/path",
    ] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in bookmarks manager output.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
      );
    }

    assert!(
      !names.iter().any(|n| n == "Open bookmark in new tab"),
      "expected open-in-new-tab buttons to use contextual labels, not a generic-only label.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
    );
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

  fn bm_frame(
    ctx: &egui::Context,
    state: &mut BookmarksManagerState,
    store: &mut BookmarkStore,
    events: Vec<egui::Event>,
  ) -> (super::BookmarksManagerOutput, egui::FullOutput) {
    begin_frame(ctx, events);
    let out = bookmarks_manager_side_panel(ctx, state, store);
    let output = ctx.end_frame();
    (out, output)
  }

  fn find_text_centers(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Vec<egui::Pos2> {
    fn in_shape(shape: &egui::epaint::Shape, needle: &str, out: &mut Vec<egui::Pos2>) {
      match shape {
        egui::epaint::Shape::Text(text) => {
          if text.galley.text().contains(needle) {
            out.push(text.pos + text.galley.size() / 2.0);
          }
        }
        egui::epaint::Shape::Vec(shapes) => {
          for shape in shapes {
            in_shape(shape, needle, out);
          }
        }
        _ => {}
      }
    }

    let mut out = Vec::new();
    for clipped in shapes {
      in_shape(&clipped.shape, needle, &mut out);
    }
    out
  }

  fn find_text_center(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Option<egui::Pos2> {
    find_text_centers(shapes, needle).into_iter().next()
  }

  fn ensure_import_export_open(
    ctx: &egui::Context,
    state: &mut BookmarksManagerState,
    store: &mut BookmarkStore,
  ) {
    let (_out, output) = bm_frame(ctx, state, store, Vec::new());
    if find_text_center(&output.shapes, "Choose file…").is_some() {
      return;
    }

    let header_pos = find_text_center(&output.shapes, "Import / Export")
      .expect("failed to find Import / Export collapsing header");
    let (_out, _output) = bm_frame(ctx, state, store, left_click_at(header_pos));
    // `CollapsingHeader` state can update after processing input; render an additional frame so we
    // can reliably locate the contents in the painted shapes.
    let (_out, output) = bm_frame(ctx, state, store, Vec::new());
    assert!(
      find_text_center(&output.shapes, "Choose file…").is_some(),
      "expected Import / Export to be open after clicking the header"
    );
  }

  #[test]
  fn choose_file_button_emits_import_dialog_request() {
    let ctx = egui::Context::default();
    let mut state = BookmarksManagerState::default();
    let mut store = BookmarkStore::default();
    ensure_import_export_open(&ctx, &mut state, &mut store);

    let (_out, output) = bm_frame(&ctx, &mut state, &mut store, Vec::new());
    let mut choose_positions = find_text_centers(&output.shapes, "Choose file…");
    choose_positions.sort_by(|a, b| a.y.total_cmp(&b.y));
    let choose_pos = *choose_positions
      .last()
      .expect("failed to find Choose file… button for import");

    let (out, _output) = bm_frame(&ctx, &mut state, &mut store, left_click_at(choose_pos));
    assert!(
      out
        .requests
        .iter()
        .any(|r| *r == BookmarksManagerRequest::RequestOpenImportDialog),
      "expected clicking Choose file… to emit RequestOpenImportDialog, got {:?}",
      out.requests
    );
  }

  #[test]
  fn choose_file_button_emits_export_dialog_request() {
    let ctx = egui::Context::default();
    let mut state = BookmarksManagerState::default();
    let mut store = BookmarkStore::default();
    ensure_import_export_open(&ctx, &mut state, &mut store);

    let (_out, output) = bm_frame(&ctx, &mut state, &mut store, Vec::new());
    let mut choose_positions = find_text_centers(&output.shapes, "Choose file…");
    choose_positions.sort_by(|a, b| a.y.total_cmp(&b.y));
    let choose_pos = *choose_positions
      .first()
      .expect("failed to find Choose file… button for export");

    let (out, _output) = bm_frame(&ctx, &mut state, &mut store, left_click_at(choose_pos));
    assert!(
      out
        .requests
        .iter()
        .any(|r| *r == BookmarksManagerRequest::RequestOpenExportDialog),
      "expected clicking Choose file… to emit RequestOpenExportDialog, got {:?}",
      out.requests
    );
  }

  #[test]
  fn dialog_selection_updates_state_paths() {
    let mut state = BookmarksManagerState::default();
    state.import_path = "old_import.json".to_string();
    state.export_path = "old_export.json".to_string();

    let import_changed = state.apply_import_dialog_selection(Some(std::path::PathBuf::from(
      "/tmp/import.json",
    )));
    assert!(import_changed);
    assert_eq!(state.import_path, "/tmp/import.json");

    let export_changed = state.apply_export_dialog_selection(Some(std::path::PathBuf::from(
      "/tmp/export.json",
    )));
    assert!(export_changed);
    assert_eq!(state.export_path, "/tmp/export.json");

    let before_import = state.import_path.clone();
    let before_export = state.export_path.clone();
    assert!(!state.apply_import_dialog_selection(None));
    assert!(!state.apply_export_dialog_selection(None));
    assert_eq!(state.import_path, before_import);
    assert_eq!(state.export_path, before_export);
  }

  #[test]
  fn export_json_job_failure_sets_error_and_does_not_touch_clipboard() {
    let ctx = egui::Context::default();
    let mut store = BookmarkStore::default();
    assert!(store.toggle("https://example.com/", Some("Example")));

    let mut state = BookmarksManagerState::default();
    state.export_json = Some("{\"old\":true}".to_string());
    let mut out = BookmarksManagerOutput::default();

    let (tx, rx) = mpsc::channel::<BookmarksIoJobUpdate>();
    drop(tx);
    state.io_job = BookmarksIoJob::ExportingJson { rx };

    let output = poll_io_job_and_end_frame(&ctx, &mut state, &mut store, &mut out);
    assert!(state.error.as_ref().is_some_and(|s| s.contains("disconnected")));
    assert_eq!(state.export_json.as_deref(), Some("{\"old\":true}"));
    assert_eq!(output.platform_output.copied_text, "");
  }

  #[test]
  fn folder_row_exposes_expanded_state_and_expand_collapse_actions() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut store = BookmarkStore::default();
    store
      .create_folder("Work".to_string(), None)
      .expect("create folder");
    let mut state = BookmarksManagerState::default();

    let output = render_bookmarks_manager(&ctx, &mut state, &mut store);
    let (node_id, node) = accesskit_node_by_name(&output, "Folder: Work");
    assert_eq!(node.is_expanded(), Some(false));
    assert!(node.supports_action(accesskit::Action::Expand));
    assert!(!node.supports_action(accesskit::Action::Collapse));

    let (_out, output) = bm_frame(
      &ctx,
      &mut state,
      &mut store,
      vec![egui::Event::AccessKitActionRequest(accesskit::ActionRequest {
        action: accesskit::Action::Expand,
        target: node_id,
        data: None,
      })],
    );
    let (_node_id, node) = accesskit_node_by_name(&output, "Folder: Work");
    assert_eq!(node.is_expanded(), Some(true));
    assert!(node.supports_action(accesskit::Action::Collapse));
    assert!(!node.supports_action(accesskit::Action::Expand));
  }

  #[test]
  fn folder_row_expand_collapse_action_requests_toggle_open_state() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut store = BookmarkStore::default();
    store
      .create_folder("Work".to_string(), None)
      .expect("create folder");
    let mut state = BookmarksManagerState::default();

    let output = render_bookmarks_manager(&ctx, &mut state, &mut store);
    let (node_id, node) = accesskit_node_by_name(&output, "Folder: Work");
    assert_eq!(node.is_expanded(), Some(false));

    let (_out, output) = bm_frame(
      &ctx,
      &mut state,
      &mut store,
      vec![egui::Event::AccessKitActionRequest(accesskit::ActionRequest {
        action: accesskit::Action::Expand,
        target: node_id,
        data: None,
      })],
    );
    let (_node_id, node) = accesskit_node_by_name(&output, "Folder: Work");
    assert_eq!(node.is_expanded(), Some(true));
    assert_eq!(
      output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("expected AccessKit update")
        .focus,
      Some(node_id),
      "expected folder row to retain focus after Expand action request"
    );

    let (_out, output) = bm_frame(
      &ctx,
      &mut state,
      &mut store,
      vec![egui::Event::AccessKitActionRequest(accesskit::ActionRequest {
        action: accesskit::Action::Collapse,
        target: node_id,
        data: None,
      })],
    );
    let (_node_id, node) = accesskit_node_by_name(&output, "Folder: Work");
    assert_eq!(node.is_expanded(), Some(false));
    assert_eq!(
      output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("expected AccessKit update")
        .focus,
      Some(node_id),
      "expected folder row to retain focus after Collapse action request"
    );

    let (_out, output) = bm_frame(
      &ctx,
      &mut state,
      &mut store,
      vec![
        egui::Event::AccessKitActionRequest(accesskit::ActionRequest {
          action: accesskit::Action::Expand,
          target: node_id,
          data: None,
        }),
        egui::Event::AccessKitActionRequest(accesskit::ActionRequest {
          action: accesskit::Action::Collapse,
          target: node_id,
          data: None,
        }),
      ],
    );
    let (_node_id, node) = accesskit_node_by_name(&output, "Folder: Work");
    assert_eq!(
      node.is_expanded(),
      Some(true),
      "expected Expand to win when both Expand and Collapse are requested in the same frame"
    );
  }

  fn key_press(key: egui::Key) -> egui::Event {
    egui::Event::Key {
      key,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers::default(),
    }
  }

  #[test]
  fn escape_clears_search_then_requests_close() {
    let ctx = egui::Context::default();
    let mut state = BookmarksManagerState::default();
    let mut store = BookmarkStore::default();

    state.request_focus_search();

    // Frame 1: open panel and focus the search field.
    begin_frame(&ctx, Vec::new());
    let out = bookmarks_manager_side_panel(&ctx, &mut state, &mut store);
    let _ = ctx.end_frame();
    assert!(
      !out.close_requested,
      "focusing the search field should not request closing the panel"
    );

    // Frame 2: with a non-empty query, Escape clears the search but keeps the panel open.
    state.search = "example".to_string();
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let out = bookmarks_manager_side_panel(&ctx, &mut state, &mut store);
    let _ = ctx.end_frame();
    assert_eq!(state.search, "");
    assert!(
      !out.close_requested,
      "Escape should clear a non-empty query before closing the panel"
    );

    // Frame 3: with an empty query, Escape requests panel close.
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let out = bookmarks_manager_side_panel(&ctx, &mut state, &mut store);
    let _ = ctx.end_frame();
    assert_eq!(state.search, "");
    assert!(
      out.close_requested,
      "Escape with an empty query should request closing the panel"
    );
  }
  #[test]
  fn export_json_job_completion_updates_state_and_copies_text() {
    let ctx = egui::Context::default();
    let mut store = BookmarkStore::default();
    assert!(store.toggle("https://a.example/", Some("A")));
    assert!(store.toggle("https://b.example/", Some("B")));

    let mut state = BookmarksManagerState::default();
    let mut out = BookmarksManagerOutput::default();

    state.io_job.start_export_json(store.clone()).unwrap();

    let output = wait_for_io_job(
      &ctx,
      &mut state,
      &mut store,
      &mut out,
      |state, _store, _out| state.export_json.is_some() || state.error.is_some(),
    );

    let json = state.export_json.clone().expect("export_json should be set");

    assert_eq!(output.platform_output.copied_text, json);
    assert_eq!(
      state.message.as_deref(),
      Some("Exported bookmarks JSON copied to clipboard.")
    );
    assert!(state.error.is_none());
    assert!(!out.changed);
    assert!(!out.request_flush);
    assert!(out.bookmark_deltas.is_empty());
  }

  #[test]
  fn import_json_job_completion_replaces_store_and_clears_transient_state() {
    let ctx = egui::Context::default();
    let mut store = BookmarkStore::default();
    assert!(store.toggle("https://initial.example/", Some("Initial")));

    let mut imported = BookmarkStore::default();
    assert!(imported.toggle("https://imported.example/", Some("Imported")));
    let expected_store = imported.clone();
    let json = serde_json::to_string_pretty(&imported).unwrap();

    let mut state = BookmarksManagerState::default();
    state.import_json = json.clone();
    state.creating_folder = Some(CreateFolderState {
      title: "Folder".to_string(),
      parent: None,
      error: None,
      request_focus_title: false,
    });
    state.editing_bookmark = Some(EditBookmarkState {
      id: BookmarkId(123),
      title: "Title".to_string(),
      url: "https://example.com/".to_string(),
      parent: None,
      error: None,
      request_focus_title: false,
    });

    let mut out = BookmarksManagerOutput::default();
    state.io_job.start_import_json(json).unwrap();

    let _output = wait_for_io_job(
      &ctx,
      &mut state,
      &mut store,
      &mut out,
      |state, _store, out| out.changed || state.error.is_some(),
    );

    assert_eq!(store, expected_store);
    assert!(out.changed);
    assert!(out.request_flush);
    assert_eq!(state.import_json, "");
    assert!(state.creating_folder.is_none());
    assert!(state.editing_bookmark.is_none());
    assert_eq!(
      state.message.as_deref(),
      Some("Imported bookmarks from JSON (None).")
    );
    assert!(state.error.is_none());
    assert!(
      matches!(out.bookmark_deltas.as_slice(), [BookmarkDelta::ReplaceAll(s)] if *s == expected_store),
      "expected ReplaceAll delta, got {:?}",
      out.bookmark_deltas
    );
  }

  #[test]
  fn import_json_job_failure_sets_error_and_preserves_store() {
    let ctx = egui::Context::default();
    let mut store = BookmarkStore::default();
    assert!(store.toggle("https://initial.example/", Some("Initial")));
    let before = store.clone();

    let mut state = BookmarksManagerState::default();
    state.import_json = "not valid json".to_string();

    let mut out = BookmarksManagerOutput::default();
    state
      .io_job
      .start_import_json(state.import_json.clone())
      .unwrap();

    let _output = wait_for_io_job(
      &ctx,
      &mut state,
      &mut store,
      &mut out,
      |state, _store, _out| state.error.is_some(),
    );

    assert_eq!(store, before);
    assert!(!out.changed);
    assert!(!out.request_flush);
    assert!(out.bookmark_deltas.is_empty());
    assert!(state.error.as_ref().unwrap().contains("Failed to import bookmarks"));
    assert_eq!(state.import_json, "not valid json");
  }
}
