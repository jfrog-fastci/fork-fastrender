#![cfg(feature = "browser_ui")]

use super::super::{bookmarks_manager, BookmarkStore};

pub struct BookmarksManagerInput<'a> {
  pub state: &'a mut bookmarks_manager::BookmarksManagerState,
  pub store: &'a mut BookmarkStore,
}

pub type BookmarksManagerOutput = bookmarks_manager::BookmarksManagerOutput;

pub fn bookmarks_manager_side_panel(
  ctx: &egui::Context,
  input: BookmarksManagerInput<'_>,
) -> BookmarksManagerOutput {
  bookmarks_manager::bookmarks_manager_side_panel(ctx, input.state, input.store)
}

