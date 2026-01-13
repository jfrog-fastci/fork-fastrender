#![cfg(feature = "browser_ui")]

use accesskit::Role;

/// Map a FastRender accessibility `role` string into an AccessKit [`Role`].
///
/// FastRender emits ARIA role tokens (lowercase), plus `generic` as a fallback for unnamed elements
/// that are still exposed to assistive technology. When adding new ARIA roles to
/// `FASTRENDER_VALID_ARIA_ROLE_TOKENS`, this mapping must be updated as well.
pub(crate) fn accesskit_role_for_fastr_role(role: &str) -> Role {
  match role {
    // -------------------------------------------------------------------------
    // Common UI roles
    // -------------------------------------------------------------------------
    "button" => Role::Button,
    "checkbox" => Role::CheckBox,
    "radio" => Role::RadioButton,
    "textbox" => Role::TextField,
    "searchbox" => Role::SearchBox,
    // AccessKit 0.11 doesn't expose a dedicated `ComboBox` role. Model the common (select-only)
    // combobox pattern as a popup button.
    "combobox" => Role::PopupButton,
    "slider" => Role::Slider,
    "spinbutton" => Role::SpinButton,
    "switch" => Role::Switch,
    "progressbar" => Role::ProgressIndicator,
    "meter" => Role::Meter,

    // -------------------------------------------------------------------------
    // Tabs / menus / toolbars (chrome UI)
    // -------------------------------------------------------------------------
    "tablist" => Role::TabList,
    "tab" => Role::Tab,
    "tabpanel" => Role::TabPanel,
    "toolbar" => Role::Toolbar,
    "menubar" => Role::MenuBar,
    "menu" => Role::Menu,
    "menuitem" => Role::MenuItem,
    "menuitemcheckbox" => Role::MenuItemCheckBox,
    "menuitemradio" => Role::MenuItemRadio,

    // -------------------------------------------------------------------------
    // Document / structure
    // -------------------------------------------------------------------------
    "document" => Role::Document,
    "heading" => Role::Heading,
    "paragraph" => Role::Paragraph,
    "img" => Role::Image,
    "link" => Role::Link,
    "list" => Role::List,
    "listbox" => Role::ListBox,
    "listitem" => Role::ListItem,
    "option" => Role::ListBoxOption,
    "group" => Role::Group,
    "generic" => Role::GenericContainer,
    "none" | "presentation" => Role::GenericContainer,

    // -------------------------------------------------------------------------
    // Tables / grids / trees
    // -------------------------------------------------------------------------
    "table" => Role::Table,
    "grid" => Role::Grid,
    "treegrid" => Role::TreeGrid,
    "rowgroup" => Role::RowGroup,
    "row" => Role::Row,
    "cell" => Role::Cell,
    // AccessKit 0.11 does not distinguish `gridcell` from `cell`.
    "gridcell" => Role::Cell,
    "columnheader" => Role::ColumnHeader,
    "rowheader" => Role::RowHeader,
    "tree" => Role::Tree,
    "treeitem" => Role::TreeItem,

    // -------------------------------------------------------------------------
    // Landmarks
    // -------------------------------------------------------------------------
    "banner" => Role::Banner,
    "main" => Role::Main,
    "navigation" => Role::Navigation,
    "search" => Role::Search,
    "contentinfo" => Role::ContentInfo,
    "complementary" => Role::Complementary,
    "region" => Role::Region,

    // -------------------------------------------------------------------------
    // Misc ARIA roles supported by FastRender.
    // -------------------------------------------------------------------------
    "alert" => Role::Alert,
    "alertdialog" => Role::AlertDialog,
    "application" => Role::Application,
    "article" => Role::Article,
    "caption" => Role::Caption,
    "definition" => Role::Definition,
    "dialog" => Role::Dialog,
    "directory" => Role::Directory,
    "feed" => Role::Feed,
    "figure" => Role::Figure,
    "form" => Role::Form,
    "log" => Role::Log,
    "marquee" => Role::Marquee,
    "math" => Role::Math,
    "note" => Role::Note,
    "radiogroup" => Role::RadioGroup,
    // ARIA's `separator` role is best represented as a splitter in native accessibility APIs.
    "separator" => Role::Splitter,
    "status" => Role::Status,
    "term" => Role::Term,
    "timer" => Role::Timer,
    "tooltip" => Role::Tooltip,

    other => {
      // Unknown roles should not silently fall back: the caller should either update the mapping
      // (when FastRender adds new roles) or clamp upstream to a known token.
      debug_assert!(
        false,
        "unknown FastRender accessibility role {other:?}; update accesskit_role_for_fastr_role"
      );
      Role::GenericContainer
    }
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::accesskit_role_for_fastr_role;
  use accesskit::Role;

  #[test]
  fn accesskit_role_mapping_is_exhaustive_for_supported_aria_role_tokens() {
    for role in crate::accessibility::FASTRENDER_VALID_ARIA_ROLE_TOKENS {
      let _ = accesskit_role_for_fastr_role(role);
    }
  }

  #[test]
  fn chrome_role_mappings_match_expected_accesskit_roles() {
    assert_eq!(accesskit_role_for_fastr_role("tablist"), Role::TabList);
    assert_eq!(accesskit_role_for_fastr_role("tab"), Role::Tab);
    assert_eq!(accesskit_role_for_fastr_role("toolbar"), Role::Toolbar);
    assert_eq!(accesskit_role_for_fastr_role("menubar"), Role::MenuBar);
    assert_eq!(accesskit_role_for_fastr_role("menu"), Role::Menu);
    assert_eq!(
      accesskit_role_for_fastr_role("menuitemcheckbox"),
      Role::MenuItemCheckBox
    );
    assert_eq!(
      accesskit_role_for_fastr_role("menuitemradio"),
      Role::MenuItemRadio
    );
    assert_eq!(accesskit_role_for_fastr_role("tabpanel"), Role::TabPanel);
  }
}
