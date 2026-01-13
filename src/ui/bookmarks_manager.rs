#![cfg(feature = "browser_ui")]

//! Bookmarks manager UI for the windowed browser frontend.
//!
//! This is implemented as an `egui::SidePanel` so it does not overlap the rendered page image
//! (which keeps page hit-testing/pointer forwarding simple).

use std::collections::HashMap;
use std::path::Path;

use crate::ui::motion::UiMotion;

use super::{
  icon_button, icon_tinted, panel_empty_state, panel_header, panel_search_field, BookmarkId,
  BookmarkNode, BookmarkStore, BrowserIcon,
};

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

  request_focus_search: bool,
  creating_folder: Option<CreateFolderState>,
  editing_bookmark: Option<EditBookmarkState>,
}

impl BookmarksManagerState {
  pub fn request_focus_search(&mut self) {
    self.request_focus_search = true;
  }

  pub fn clear_transient(&mut self) {
    self.creating_folder = None;
    self.editing_bookmark = None;
  }
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

      let folder_options = folder_options(store);
      let folder_labels: HashMap<Option<BookmarkId>, String> =
        folder_options.iter().cloned().collect::<HashMap<_, _>>();

      ui.add_space(ui.spacing().item_spacing.y.max(6.0));

      // -----------------------------------------------------------------------
      // Messages / errors (callouts)
      // -----------------------------------------------------------------------
      if let Some(msg) = state.message.as_deref().filter(|s| !s.trim().is_empty()) {
        callout(ui, CalloutKind::Message, msg);
        ui.add_space(ui.spacing().item_spacing.y.max(6.0));
      }
      if let Some(err) = state.error.as_deref().filter(|s| !s.trim().is_empty()) {
        callout(ui, CalloutKind::Error, err);
        ui.add_space(ui.spacing().item_spacing.y.max(6.0));
      }

      // -----------------------------------------------------------------------
      // Search + toolbar
      // -----------------------------------------------------------------------
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let new_folder = icon_button(ui, BrowserIcon::Folder, "New folder", true);
        new_folder.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, "Create new folder")
        });
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
        create_folder_card(ui, state, store, &folder_options, &mut out);
        ui.add_space(ui.spacing().item_spacing.y.max(8.0));
      }

      if state.editing_bookmark.is_some() {
        edit_bookmark_card(ui, state, store, &folder_options, &mut out);
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

            if ui.button("Export file").clicked() {
              let raw = state.export_path.trim();
              if raw.is_empty() {
                state.error = Some("Export path is empty.".to_string());
              } else {
                match crate::ui::save_bookmarks_atomic(Path::new(raw), store) {
                  Ok(()) => {
                    state.error = None;
                    state.message = Some(format!("Exported bookmarks to {}.", raw));
                  }
                  Err(err) => {
                    state.error = Some(format!("Failed to export bookmarks: {err}"));
                  }
                }
              }
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

            if ui.button("Import file").clicked() {
              let raw = state.import_path.trim();
              if raw.is_empty() {
                state.error = Some("Import path is empty.".to_string());
              } else {
                match std::fs::read_to_string(raw) {
                  Ok(json) => match BookmarkStore::from_json_str_migrating(&json) {
                    Ok((imported, migration)) => {
                      *store = imported;
                      out.changed = true;
                      out.request_flush = true;
                      state.error = None;
                      state.message =
                        Some(format!("Imported bookmarks from file ({migration:?})."));
                      state.import_json.clear();
                      state.clear_transient();
                    }
                    Err(err) => {
                      state.error = Some(format!("Failed to import bookmarks: {err:?}"));
                    }
                  },
                  Err(err) => {
                    state.error = Some(format!("Failed to read {raw:?}: {err}"));
                  }
                }
              }
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
                  *store = imported;
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
      bookmarks_list(ui, state, store, &folder_options, &folder_labels, &mut out);
    });

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
    "Search bookmarks",
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
    match store.create_folder(create.title.clone(), create.parent) {
      Ok(_) => {
        out.changed = true;
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
    match store.update(edit.id, title, edit.url.clone(), edit.parent) {
      Ok(()) => {
        out.changed = true;
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
  folder_options: &[(Option<BookmarkId>, String)],
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
    if store.roots.is_empty() {
      panel_empty_state(
        ui,
        BrowserIcon::BookmarkOutline,
        "No bookmarks",
        Some("Press Ctrl/Cmd+D to bookmark the current page."),
        None,
      );
      return;
    }

    let query = state.search.trim().to_string();
    if !query.is_empty() {
      let results = store.search(&query, usize::MAX);
      if results.is_empty() {
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
        return;
      }

      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
          for id in results {
            let Some(BookmarkNode::Bookmark(entry)) = store.nodes.get(&id).cloned() else {
              continue;
            };
            let parent_label = folder_labels
              .get(&entry.parent)
              .map(String::as_str)
              .unwrap_or("Root");
            render_bookmark_row(ui, state, store, entry, folder_options, parent_label, out);
          }
        });
      return;
    }

    egui::ScrollArea::vertical()
      .auto_shrink([false, false])
      .show(ui, |ui| {
        let roots = store.roots.clone();
        render_nodes(ui, state, store, &roots, folder_options, folder_labels, out);
      });
  });
}

// ---------------------------------------------------------------------------
// Tree rendering
// ---------------------------------------------------------------------------

fn render_nodes(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  store: &mut BookmarkStore,
  ids: &[BookmarkId],
  folder_options: &[(Option<BookmarkId>, String)],
  folder_labels: &HashMap<Option<BookmarkId>, String>,
  out: &mut BookmarksManagerOutput,
) {
  for id in ids {
    let Some(node) = store.nodes.get(id).cloned() else {
      continue;
    };
    match node {
      BookmarkNode::Bookmark(entry) => {
        let parent_label = folder_labels
          .get(&entry.parent)
          .map(String::as_str)
          .unwrap_or("Root");
        render_bookmark_row(ui, state, store, entry, folder_options, parent_label, out);
      }
      BookmarkNode::Folder(folder) => {
        let children = folder.children.clone();
        let folder_id = folder.id;

        let open_id = ui.make_persistent_id(("bookmarks_folder_open", folder_id.0));
        let mut open = ui
          .ctx()
          .data_mut(|d| d.get_persisted::<bool>(open_id))
          .unwrap_or(false);

        let mut delete_clicked = false;

        let title = folder.title.clone();
        let item_count = children.len();

        let row_resp = list_row(ui, ("folder_row", folder_id.0), false, |ui| {
          ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let del = icon_button(ui, BrowserIcon::Trash, "Delete folder", true);
            del.widget_info({
              let label = format!("Delete folder: {title}");
              move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
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

        row_resp.widget_info({
          let title = folder.title.clone();
          move || {
            egui::WidgetInfo::labeled(
              egui::WidgetType::Button,
              if open {
                format!("Collapse folder: {title}")
              } else {
                format!("Expand folder: {title}")
              },
            )
          }
        });

        if delete_clicked {
          if store.remove_by_id(folder_id) {
            out.changed = true;
            out.request_flush = true;
            state.clear_transient();
          }
          continue;
        }

        if row_resp.clicked() {
          open = !open;
          ui.ctx().data_mut(|d| d.insert_persisted(open_id, open));
        }

        if open {
          ui.indent(open_id.with("indent"), |ui| {
            ui.add_space(2.0);
            render_nodes(
              ui,
              state,
              store,
              &children,
              folder_options,
              folder_labels,
              out,
            );
          });
        }
      }
    }
  }
}

fn render_bookmark_row(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  store: &mut BookmarkStore,
  entry: super::bookmarks::BookmarkEntry,
  _folder_options: &[(Option<BookmarkId>, String)],
  parent_label: &str,
  out: &mut BookmarksManagerOutput,
) {
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
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      let del = icon_button(ui, BrowserIcon::Trash, "Delete bookmark", true);
      del.widget_info({
        let label = format!("Delete bookmark: {title}");
        move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
      });
      if del.clicked() {
        delete_clicked = true;
      }

      let edit_btn = icon_button(ui, BrowserIcon::Edit, "Edit bookmark", true);
      edit_btn.widget_info({
        let label = format!("Edit bookmark: {title}");
        move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
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

        let title_text = if editing_this {
          egui::RichText::new(title.clone())
            .strong()
            .color(ui.visuals().selection.stroke.color)
        } else {
          egui::RichText::new(title.clone()).strong()
        };
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
  })
  .on_hover_text(entry.url.clone());

  row_resp.widget_info({
    let title = title.clone();
    let url = entry.url.clone();
    move || {
      egui::WidgetInfo::labeled(
        egui::WidgetType::Button,
        format!("Open bookmark: {title} ({url})"),
      )
    }
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
      title: entry.title.unwrap_or_default(),
      url: entry.url.clone(),
      parent: entry.parent,
      error: None,
      request_focus_title: true,
    });
    out.unfocus_page = true;
  }

  if delete_clicked {
    if store.remove_by_id(entry.id) {
      out.changed = true;
      out.request_flush = true;
      state.clear_transient();
    }
  }
}

fn folder_options(store: &BookmarkStore) -> Vec<(Option<BookmarkId>, String)> {
  let mut out = Vec::new();
  out.push((None, "Root".to_string()));

  for (id, path) in store.folders_in_display_order() {
    out.push((Some(id), path.join("/")));
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
  let selected = options
    .iter()
    .find(|(id, _)| id == value)
    .map(|(_, label)| label.as_str())
    .unwrap_or("Root");
  let selected = truncate_middle(selected, 48);
  let response = egui::ComboBox::from_id_source(id_source)
    .selected_text(selected)
    .show_ui(ui, |ui| {
      for (id, label) in options {
        ui.selectable_value(value, *id, label);
      }
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
  let chars: Vec<char> = s.chars().collect();
  if chars.len() <= max_chars {
    return s.to_string();
  }
  if max_chars <= 1 {
    return "…".to_string();
  }
  let head = max_chars / 2;
  let tail = max_chars - head - 1;
  let mut out = String::new();
  out.extend(chars.iter().take(head));
  out.push('…');
  out.extend(chars.iter().skip(chars.len().saturating_sub(tail)));
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

fn list_row(
  ui: &mut egui::Ui,
  id_source: impl std::hash::Hash,
  selected: bool,
  add_contents: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
  let row_id = ui.make_persistent_id(id_source);
  let width = ui.available_width().max(0.0);
  let min_height = (ui.spacing().interact_size.y * 2.2).max(56.0);
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
