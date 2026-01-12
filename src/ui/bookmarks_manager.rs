#![cfg(feature = "browser_ui")]

//! Bookmarks manager UI for the windowed browser frontend.
//!
//! This is implemented as an `egui::SidePanel` so it does not overlap the rendered page image
//! (which keeps page hit-testing/pointer forwarding simple).

use std::collections::HashMap;

use super::{icon_button, BookmarkId, BookmarkNode, BookmarkStore, BrowserIcon};

#[derive(Debug, Clone)]
pub enum BookmarksManagerAction {
  Open(String),
  OpenInNewTab(String),
}

#[derive(Debug, Default)]
pub struct BookmarksManagerState {
  pub search: String,
  pub import_json: String,
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
      ui.horizontal(|ui| {
        ui.heading("Bookmarks");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          let close_resp = icon_button(ui, BrowserIcon::Close, "Close", true);
          close_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close bookmarks manager")
          });
          if close_resp.clicked() {
            out.close_requested = true;
          }
        });
      });
      ui.separator();

      if let Some(msg) = state.message.as_deref().filter(|s| !s.trim().is_empty()) {
        ui.label(msg);
      }
      if let Some(err) = state.error.as_deref().filter(|s| !s.trim().is_empty()) {
        ui.colored_label(ui.visuals().error_fg_color, err);
      }

      let folder_options = folder_options(store);
      let folder_labels: HashMap<Option<BookmarkId>, String> =
        folder_options.iter().cloned().collect::<HashMap<_, _>>();

      // -----------------------------------------------------------------------
      // Search + toolbar
      // -----------------------------------------------------------------------
      ui.horizontal(|ui| {
        ui.label("Search:");
        let search_id = ui.make_persistent_id("bookmarks_manager_search");
        let resp = ui.add(
          egui::TextEdit::singleline(&mut state.search)
            .id(search_id)
            .hint_text("Filter by title or URL…")
            .desired_width(f32::INFINITY),
        );
        if state.request_focus_search {
          resp.request_focus();
          state.request_focus_search = false;
          out.unfocus_page = true;
        }
        if resp.has_focus() || resp.clicked() {
          out.unfocus_page = true;
        }

        if ui.button("New folder").clicked() {
          state.creating_folder = Some(CreateFolderState {
            title: String::new(),
            parent: None,
            error: None,
            request_focus_title: true,
          });
        }
      });

      // Create-folder inline form.
      if let Some(create) = state.creating_folder.as_mut() {
        ui.add_space(4.0);
        ui.group(|ui| {
          ui.label(egui::RichText::new("Create folder").strong());
          ui.horizontal(|ui| {
            ui.label("Title:");
            let resp = ui.text_edit_singleline(&mut create.title);
            if create.request_focus_title {
              resp.request_focus();
              create.request_focus_title = false;
              out.unfocus_page = true;
            }
            if resp.has_focus() || resp.clicked() {
              out.unfocus_page = true;
            }
          });
          ui.horizontal(|ui| {
            ui.label("Parent:");
            folder_combo_box(
              ui,
              "create_folder_parent",
              &folder_options,
              &mut create.parent,
            );
          });
          if let Some(err) = create.error.as_deref().filter(|s| !s.trim().is_empty()) {
            ui.colored_label(ui.visuals().error_fg_color, err);
          }
          ui.horizontal(|ui| {
            if ui.button("Create").clicked() {
              match store.create_folder(create.title.clone(), create.parent) {
                Ok(_) => {
                  out.changed = true;
                  state.creating_folder = None;
                  state.error = None;
                  state.message = Some("Folder created.".to_string());
                }
                Err(err) => {
                  create.error = Some(format!("{err:?}"));
                }
              }
            }
            if ui.button("Cancel").clicked() {
              state.creating_folder = None;
            }
          });
        });
      }

      ui.add_space(6.0);
      ui.separator();

      // -----------------------------------------------------------------------
      // Import / export
      // -----------------------------------------------------------------------
      ui.collapsing("Import / Export (JSON)", |ui| {
        ui.horizontal(|ui| {
          if ui.button("Export (copy to clipboard)").clicked() {
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

        if let Some(json) = state.export_json.as_mut() {
          let resp = ui.add(
            egui::TextEdit::multiline(json)
              .code_editor()
              .desired_rows(6)
              .lock_focus(true)
              .desired_width(f32::INFINITY),
          );
          if resp.has_focus() || resp.clicked() {
            out.unfocus_page = true;
          }
        }

        ui.add_space(8.0);
        ui.label("Import (replaces all bookmarks):");
        let resp = ui.add(
          egui::TextEdit::multiline(&mut state.import_json)
            .code_editor()
            .desired_rows(6)
            .hint_text("Paste bookmarks JSON here…")
            .desired_width(f32::INFINITY),
        );
        if resp.has_focus() || resp.clicked() {
          out.unfocus_page = true;
        }

        ui.horizontal(|ui| {
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

      ui.add_space(6.0);
      ui.separator();

      // -----------------------------------------------------------------------
      // Bookmarks list
      // -----------------------------------------------------------------------
      if store.roots.is_empty() {
        ui.label("No bookmarks yet. Press Ctrl/Cmd+D to bookmark the current page.");
        return;
      }

      let query = state.search.trim();
      egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if query.is_empty() {
          let roots = store.roots.clone();
          render_nodes(ui, state, store, &roots, &folder_options, &folder_labels, &mut out);
        } else {
          let results = store.search(query, usize::MAX);
          if results.is_empty() {
            ui.label("No matching bookmarks.");
            return;
          }
          for id in results {
            let Some(BookmarkNode::Bookmark(entry)) = store.nodes.get(&id).cloned() else {
              continue;
            };
            let parent_label = folder_labels
              .get(&entry.parent)
              .map(String::as_str)
              .unwrap_or("Root");
            ui.group(|ui| {
              render_bookmark_row(ui, state, store, entry, &folder_options, parent_label, &mut out);
            });
          }
        }
      });
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
        ui.collapsing(folder.title.clone(), |ui| {
          ui.horizontal(|ui| {
            if ui.small_button("Delete folder").clicked() {
              if store.remove_by_id(folder.id) {
                out.changed = true;
                out.request_flush = true;
                state.clear_transient();
              }
            }
          });
          render_nodes(ui, state, store, &children, folder_options, folder_labels, out);
        });
      }
    }
  }
}

fn render_bookmark_row(
  ui: &mut egui::Ui,
  state: &mut BookmarksManagerState,
  store: &mut BookmarkStore,
  entry: super::bookmarks::BookmarkEntry,
  folder_options: &[(Option<BookmarkId>, String)],
  parent_label: &str,
  out: &mut BookmarksManagerOutput,
) {
  if state
    .editing_bookmark
    .as_ref()
    .is_some_and(|edit| edit.id == entry.id)
  {
    let edit = state
      .editing_bookmark
      .as_mut()
      .expect("edit state must exist");
    ui.label(egui::RichText::new("Edit bookmark").strong());
    ui.horizontal(|ui| {
      ui.label("Title:");
      let resp = ui.text_edit_singleline(&mut edit.title);
      if edit.request_focus_title {
        resp.request_focus();
        edit.request_focus_title = false;
        out.unfocus_page = true;
      }
      if resp.has_focus() || resp.clicked() {
        out.unfocus_page = true;
      }
    });
    ui.horizontal(|ui| {
      ui.label("URL:");
      let resp = ui.text_edit_singleline(&mut edit.url);
      if resp.has_focus() || resp.clicked() {
        out.unfocus_page = true;
      }
    });
    ui.horizontal(|ui| {
      ui.label("Folder:");
      folder_combo_box(ui, format!("edit_parent_{}", entry.id.0), folder_options, &mut edit.parent);
    });
    if let Some(err) = edit.error.as_deref().filter(|s| !s.trim().is_empty()) {
      ui.colored_label(ui.visuals().error_fg_color, err);
    }
    ui.horizontal(|ui| {
      if ui.button("Save").clicked() {
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
          }
        }
      }
      if ui.button("Cancel").clicked() {
        state.editing_bookmark = None;
      }
    });
    return;
  }

  let label = entry
    .title
    .as_deref()
    .map(str::trim)
    .filter(|t| !t.is_empty())
    .unwrap_or(entry.url.as_str())
    .to_string();

  ui.horizontal(|ui| {
    let resp = ui.button(label).on_hover_text(entry.url.clone());
    if resp.clicked() {
      out.actions.push(BookmarksManagerAction::Open(entry.url.clone()));
    }

    if ui.small_button("New tab").clicked() {
      out.actions.push(BookmarksManagerAction::OpenInNewTab(entry.url.clone()));
    }
    if ui.small_button("Edit").clicked() {
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
    if ui.small_button("Delete").clicked() {
      if store.remove_by_id(entry.id) {
        out.changed = true;
        out.request_flush = true;
        state.clear_transient();
      }
    }
  });
  ui.label(
    egui::RichText::new(entry.url)
      .small()
      .color(ui.visuals().weak_text_color()),
  );
  ui.label(
    egui::RichText::new(format!("Folder: {parent_label}"))
      .small()
      .color(ui.visuals().weak_text_color()),
  );
}

fn folder_options(store: &BookmarkStore) -> Vec<(Option<BookmarkId>, String)> {
  let mut out = Vec::new();
  out.push((None, "Root".to_string()));

  fn visit(
    store: &BookmarkStore,
    ids: &[BookmarkId],
    prefix: &str,
    out: &mut Vec<(Option<BookmarkId>, String)>,
  ) {
    for id in ids {
      let Some(BookmarkNode::Folder(folder)) = store.nodes.get(id) else {
        continue;
      };
      let path = if prefix.is_empty() {
        folder.title.clone()
      } else {
        format!("{prefix}/{}", folder.title)
      };
      out.push((Some(folder.id), path.clone()));
      visit(store, &folder.children, &path, out);
    }
  }

  visit(store, &store.roots, "", &mut out);
  out
}

fn folder_combo_box(
  ui: &mut egui::Ui,
  id_source: impl std::hash::Hash,
  options: &[(Option<BookmarkId>, String)],
  value: &mut Option<BookmarkId>,
) {
  let selected = options
    .iter()
    .find(|(id, _)| id == value)
    .map(|(_, label)| label.as_str())
    .unwrap_or("Root");
  egui::ComboBox::from_id_source(id_source)
    .selected_text(selected)
    .show_ui(ui, |ui| {
      for (id, label) in options {
        ui.selectable_value(value, *id, label);
      }
    });
}

fn normalize_optional_string(raw: &str) -> Option<String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    None
  } else {
    Some(trimmed.to_string())
  }
}
