#![cfg(feature = "browser_ui")]

use crate::accessibility::{AccessibilityState, CheckState, PressedState};

/// Map a FastRender accessibility role string (as produced by
/// `crate::accessibility::compute_role`) to an AccessKit role.
///
/// This translation layer is intentionally conservative:
/// - Unknown roles never panic; they fall back to `Role::GenericContainer`.
/// - Input is treated as ASCII-case-insensitive because ARIA role values are.
pub fn accesskit_role_for_fastr_role(role: &str) -> accesskit::Role {
  let role = role.trim();
  if role.is_empty() {
    return accesskit::Role::GenericContainer;
  }

  match role.to_ascii_lowercase().as_str() {
    // Core document/container roles.
    "document" => accesskit::Role::Document,
    "generic" => accesskit::Role::GenericContainer,

    // Interactive controls.
    "link" => accesskit::Role::Link,
    "button" => accesskit::Role::Button,
    "textbox" => accesskit::Role::TextField,
    "searchbox" => accesskit::Role::SearchBox,
    // AccessKit 0.11 does not expose a dedicated `ComboBox` role; represent HTML/ARIA comboboxes as
    // a popup button (the closest UIA/AX concept across platforms).
    "combobox" => accesskit::Role::PopupButton,
    "listbox" => accesskit::Role::ListBox,
    "option" => accesskit::Role::ListBoxOption,
    "checkbox" => accesskit::Role::CheckBox,
    "radio" => accesskit::Role::RadioButton,
    "slider" => accesskit::Role::Slider,
    "spinbutton" => accesskit::Role::SpinButton,
    "switch" => accesskit::Role::Switch,

    // Lists/groups.
    "list" => accesskit::Role::List,
    "listitem" => accesskit::Role::ListItem,
    "group" => accesskit::Role::Group,

    // Text.
    "heading" => accesskit::Role::Heading,
    "paragraph" => accesskit::Role::Paragraph,

    // Images/figures.
    "img" => accesskit::Role::Image,
    "figure" => accesskit::Role::Figure,
    "caption" => accesskit::Role::Caption,

    // Tables/grids.
    "table" => accesskit::Role::Table,
    "grid" => accesskit::Role::Grid,
    "treegrid" => accesskit::Role::TreeGrid,
    "rowgroup" => accesskit::Role::RowGroup,
    "row" => accesskit::Role::Row,
    "cell" => accesskit::Role::Cell,
    // AccessKit 0.11 does not distinguish between `cell` and `gridcell`.
    "gridcell" => accesskit::Role::Cell,
    "rowheader" => accesskit::Role::RowHeader,
    "columnheader" => accesskit::Role::ColumnHeader,

    // Range/status.
    "progressbar" => accesskit::Role::ProgressIndicator,
    "meter" => accesskit::Role::Meter,
    "status" => accesskit::Role::Status,

    // Landmarks.
    "navigation" => accesskit::Role::Navigation,
    "main" => accesskit::Role::Main,
    "banner" => accesskit::Role::Banner,
    "contentinfo" => accesskit::Role::ContentInfo,
    "complementary" => accesskit::Role::Complementary,
    "region" => accesskit::Role::Region,
    "form" => accesskit::Role::Form,
    "article" => accesskit::Role::Article,
    "search" => accesskit::Role::Search,

    // Dialogs/alerts.
    "dialog" => accesskit::Role::Dialog,
    "alert" => accesskit::Role::Alert,
    "alertdialog" => accesskit::Role::AlertDialog,

    // Menus.
    "menu" => accesskit::Role::Menu,
    "menubar" => accesskit::Role::MenuBar,
    "menuitem" => accesskit::Role::MenuItem,
    "menuitemcheckbox" => accesskit::Role::MenuItemCheckBox,
    "menuitemradio" => accesskit::Role::MenuItemRadio,

    // Tabs.
    "tab" => accesskit::Role::Tab,
    "tablist" => accesskit::Role::TabList,
    "tabpanel" => accesskit::Role::TabPanel,

    // Tree widgets.
    "tree" => accesskit::Role::Tree,
    "treeitem" => accesskit::Role::TreeItem,

    // Misc.
    // `separator` maps to the platform concept of a splitter.
    "separator" => accesskit::Role::Splitter,
    "math" => accesskit::Role::Math,
    "application" => accesskit::Role::Application,
    "directory" => accesskit::Role::Directory,
    "feed" => accesskit::Role::Feed,
    "log" => accesskit::Role::Log,
    "marquee" => accesskit::Role::Marquee,
    "note" => accesskit::Role::Note,
    "radiogroup" => accesskit::Role::RadioGroup,
    "term" => accesskit::Role::Term,
    "definition" => accesskit::Role::Definition,
    "timer" => accesskit::Role::Timer,
    "toolbar" => accesskit::Role::Toolbar,
    "tooltip" => accesskit::Role::Tooltip,

    // Roles that map poorly or imply "no semantics". Expose them as generic containers so they are
    // still addressable in the tree if the node is present.
    "none" | "presentation" => accesskit::Role::GenericContainer,

    _ => accesskit::Role::GenericContainer,
  }
}

/// Apply FastRender `AccessibilityState` to an AccessKit `NodeBuilder`.
///
/// Only state properties that have a direct AccessKit equivalent are currently mapped.
pub fn apply_fastr_states_to_accesskit(
  builder: &mut accesskit::NodeBuilder,
  states: &AccessibilityState,
) {
  if states.disabled {
    builder.set_disabled();
  } else {
    builder.clear_disabled();
  }

  // AccessKit models both ARIA `aria-checked` and `aria-pressed` with the same `checked_state`
  // property (tri-state toggle).
  let checked_state = states
    .checked
    .map(|checked| match checked {
      CheckState::True => accesskit::CheckedState::True,
      CheckState::False => accesskit::CheckedState::False,
      CheckState::Mixed => accesskit::CheckedState::Mixed,
    })
    .or_else(|| {
      states.pressed.map(|pressed| match pressed {
        PressedState::True => accesskit::CheckedState::True,
        PressedState::False => accesskit::CheckedState::False,
        PressedState::Mixed => accesskit::CheckedState::Mixed,
      })
    });

  if let Some(state) = checked_state {
    builder.set_checked_state(state);
  } else {
    builder.clear_checked_state();
  }

  if let Some(selected) = states.selected {
    builder.set_selected(selected);
  } else {
    builder.clear_selected();
  }

  if let Some(expanded) = states.expanded {
    builder.set_expanded(expanded);
  } else {
    builder.clear_expanded();
  }

  // Note: `states.pressed` is consumed via the `checked_state` mapping above.

  if states.readonly {
    builder.set_read_only();
  } else {
    builder.clear_read_only();
  }

  if let Some(multiline) = states.multiline {
    if multiline {
      builder.set_multiline();
    } else {
      builder.clear_multiline();
    }
  } else {
    builder.clear_multiline();
  }

  // AccessKit models focus via the `TreeUpdate::focus` field rather than a per-node boolean. We can
  // still expose focusability by ensuring the node advertises the `Focus` action.
  if states.focusable {
    builder.add_action(accesskit::Action::Focus);
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;

  #[test]
  fn role_mapping_covers_common_html_and_aria_roles() {
    use accesskit::Role;

    let cases: &[(&str, Role)] = &[
      ("document", Role::Document),
      ("generic", Role::GenericContainer),
      ("link", Role::Link),
      ("button", Role::Button),
      ("textbox", Role::TextField),
      ("searchbox", Role::SearchBox),
      ("combobox", Role::PopupButton),
      ("listbox", Role::ListBox),
      ("option", Role::ListBoxOption),
      ("checkbox", Role::CheckBox),
      ("radio", Role::RadioButton),
      ("slider", Role::Slider),
      ("spinbutton", Role::SpinButton),
      ("list", Role::List),
      ("listitem", Role::ListItem),
      ("group", Role::Group),
      ("heading", Role::Heading),
      ("paragraph", Role::Paragraph),
      ("img", Role::Image),
      ("figure", Role::Figure),
      ("caption", Role::Caption),
      ("table", Role::Table),
      ("rowgroup", Role::RowGroup),
      ("row", Role::Row),
      ("cell", Role::Cell),
      ("gridcell", Role::Cell),
      ("rowheader", Role::RowHeader),
      ("columnheader", Role::ColumnHeader),
      ("progressbar", Role::ProgressIndicator),
      ("meter", Role::Meter),
      ("status", Role::Status),
      ("navigation", Role::Navigation),
      ("main", Role::Main),
      ("banner", Role::Banner),
      ("contentinfo", Role::ContentInfo),
      ("complementary", Role::Complementary),
      ("region", Role::Region),
      ("form", Role::Form),
      ("article", Role::Article),
      ("dialog", Role::Dialog),
      ("separator", Role::Splitter),
      ("math", Role::Math),
    ];

    for (input, expected) in cases {
      assert_eq!(
        accesskit_role_for_fastr_role(input),
        *expected,
        "role={input}"
      );
    }

    // Unknown roles should never panic and should fall back to a stable container role.
    assert_eq!(
      accesskit_role_for_fastr_role("totally-unknown-role"),
      Role::GenericContainer
    );
  }

  #[test]
  fn state_mapping_sets_expected_accesskit_properties() {
    use accesskit::{CheckedState, NodeBuilder, NodeClassSet, Role};

    let mut builder = NodeBuilder::new(Role::CheckBox);
    let states = AccessibilityState {
      focusable: true,
      focused: true,
      focus_visible: false,
      disabled: true,
      required: false,
      invalid: false,
      visited: false,
      busy: false,
      readonly: true,
      has_popup: None,
      multiline: Some(true),
      checked: Some(CheckState::Mixed),
      selected: Some(true),
      pressed: None,
      expanded: Some(true),
      current: None,
      modal: None,
      live: None,
      atomic: None,
      relevant: None,
    };

    apply_fastr_states_to_accesskit(&mut builder, &states);
    let mut classes = NodeClassSet::default();
    let node = builder.build(&mut classes);

    assert!(node.is_disabled());
    assert_eq!(node.checked_state(), Some(CheckedState::Mixed));
    assert_eq!(node.is_selected(), Some(true));
    assert_eq!(node.is_expanded(), Some(true));
    assert!(node.is_read_only());
    assert!(node.is_multiline());
  }

  #[test]
  fn pressed_state_maps_to_accesskit_checked_state() {
    use accesskit::{CheckedState, NodeBuilder, NodeClassSet, Role};

    let mut builder = NodeBuilder::new(Role::Button);
    let states = AccessibilityState {
      focusable: false,
      focused: false,
      focus_visible: false,
      disabled: false,
      required: false,
      invalid: false,
      visited: false,
      busy: false,
      readonly: false,
      has_popup: None,
      multiline: None,
      checked: None,
      selected: None,
      pressed: Some(PressedState::Mixed),
      expanded: None,
      current: None,
      modal: None,
      live: None,
      atomic: None,
      relevant: None,
    };

    apply_fastr_states_to_accesskit(&mut builder, &states);
    let mut classes = NodeClassSet::default();
    let node = builder.build(&mut classes);

    assert_eq!(node.checked_state(), Some(CheckedState::Mixed));
  }
}
