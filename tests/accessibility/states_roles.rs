use crate::common::accessibility::find_by_id;
use fastrender::accessibility::{CheckState, PressedState};
use fastrender::api::FastRender;
use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::InteractionState;

fn find_dom_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_dom_by_id(child, id))
}

#[test]
fn aria_state_flags_cover_common_controls() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <button id="pressed" aria-pressed="true">Pressed</button>
        <div id="checkbox" role="checkbox" aria-checked="mixed">Check me</div>
        <input id="native-checkbox" type="checkbox" checked />
        <div id="custom-option" role="option" aria-selected="true">Selected</div>
        <select id="list" multiple>
          <option id="list-opt1">One</option>
          <option id="list-opt2" selected disabled>Two</option>
        </select>
        <button id="menu-button" aria-expanded="false" aria-haspopup="menu">Menu</button>
        <details id="details" open>
          <summary>Summary</summary>
          <div>Content</div>
        </details>
        <button id="aria-disabled" aria-disabled="true">Blocked</button>
        <button id="native-disabled" disabled>Native</button>
        <input id="required-invalid" aria-required="true" aria-invalid="true" />
        <a id="visited" href="#">Visited</a>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let ids = enumerate_dom_ids(&dom);
  let visited = find_dom_by_id(&dom, "visited").expect("visited link");
  let visited_id = *ids.get(&(visited as *const DomNode)).expect("node id");
  let mut interaction_state = InteractionState::default();
  interaction_state.visited_links_mut().insert(visited_id);
  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&interaction_state))
    .expect("accessibility tree");

  let pressed = find_by_id(&tree, "pressed").expect("pressed button");
  assert_eq!(pressed.states.pressed, Some(PressedState::True));

  let checkbox = find_by_id(&tree, "checkbox").expect("aria checkbox");
  assert_eq!(checkbox.states.checked, Some(CheckState::Mixed));

  let native_checkbox = find_by_id(&tree, "native-checkbox").expect("native checkbox");
  assert_eq!(native_checkbox.states.checked, Some(CheckState::True));

  let custom_option = find_by_id(&tree, "custom-option").expect("custom option");
  assert_eq!(custom_option.states.selected, Some(true));

  let list_opt1 = find_by_id(&tree, "list-opt1").expect("list option one");
  assert_eq!(list_opt1.states.selected, Some(false));
  let list_opt2 = find_by_id(&tree, "list-opt2").expect("list option two");
  assert_eq!(list_opt2.states.selected, Some(true));
  let list = find_by_id(&tree, "list").expect("listbox select");
  assert_eq!(list.role, "listbox");
  assert_eq!(list.value.as_deref(), Some("Two"));

  let menu_button = find_by_id(&tree, "menu-button").expect("menu button");
  assert_eq!(menu_button.states.expanded, Some(false));
  assert_eq!(menu_button.states.has_popup.as_deref(), Some("menu"));

  let details = find_by_id(&tree, "details").expect("details element");
  assert_eq!(details.states.expanded, Some(true));

  let aria_disabled = find_by_id(&tree, "aria-disabled").expect("aria-disabled button");
  assert!(aria_disabled.states.disabled);
  let native_disabled = find_by_id(&tree, "native-disabled").expect("native disabled");
  assert!(native_disabled.states.disabled);

  let required_invalid = find_by_id(&tree, "required-invalid").expect("required invalid");
  assert!(required_invalid.states.required);
  assert!(required_invalid.states.invalid);

  let visited = find_by_id(&tree, "visited").expect("visited link");
  assert!(visited.states.visited);
  assert!(visited.states.focusable);
}

#[test]
fn native_single_select_last_selected_wins() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="single">
          <option id="single-opt1" selected>One</option>
          <option id="single-opt2" selected>Two</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let first = find_by_id(&tree, "single-opt1").expect("single option one");
  let second = find_by_id(&tree, "single-opt2").expect("single option two");
  assert_eq!(first.states.selected, Some(false));
  assert_eq!(second.states.selected, Some(true));
}

#[test]
fn focus_states_use_interaction_state_hints() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <input id="focused" type="text" />
        <input id="focus-only" type="text" />
        <input id="visible-only" type="text" />
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let ids = enumerate_dom_ids(&dom);
  let focused = find_dom_by_id(&dom, "focused").expect("focused input");
  let focused_id = *ids.get(&(focused as *const DomNode)).expect("node id");
  let focus_only = find_dom_by_id(&dom, "focus-only").expect("focus-only input");
  let focus_only_id = *ids.get(&(focus_only as *const DomNode)).expect("node id");

  let mut interaction_state = InteractionState::default();
  interaction_state.focused = Some(focused_id);
  interaction_state.focus_visible = true;
  interaction_state.set_focus_chain(vec![focused_id]);
  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&interaction_state))
    .expect("accessibility tree");

  let focused = find_by_id(&tree, "focused").expect("focused input");
  assert!(focused.states.focused);
  assert!(focused.states.focus_visible);

  let focus_only = find_by_id(&tree, "focus-only").expect("focus-only input");
  assert!(!focus_only.states.focused);
  assert!(!focus_only.states.focus_visible);

  let visible_only = find_by_id(&tree, "visible-only").expect("visible-only input");
  assert!(!visible_only.states.focused);
  assert!(!visible_only.states.focus_visible);

  let mut interaction_state = InteractionState::default();
  interaction_state.focused = Some(focus_only_id);
  interaction_state.focus_visible = false;
  interaction_state.set_focus_chain(vec![focus_only_id]);
  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&interaction_state))
    .expect("accessibility tree");
  let focus_only = find_by_id(&tree, "focus-only").expect("focus-only input");
  assert!(focus_only.states.focused);
  assert!(!focus_only.states.focus_visible);
}

#[test]
fn native_select_combobox_expanded_state_tracks_open_dropdown_overlay() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="s">
          <option selected>A</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let ids = enumerate_dom_ids(&dom);
  let select = find_dom_by_id(&dom, "s").expect("select");
  let select_id = *ids.get(&(select as *const DomNode)).expect("select id");

  let mut interaction_state = InteractionState::default();
  interaction_state.open_select_dropdown = Some(select_id);
  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&interaction_state))
    .expect("accessibility tree");
  let select = find_by_id(&tree, "s").expect("select node");
  assert_eq!(select.role, "combobox");
  assert_eq!(select.states.expanded, Some(true));

  let interaction_state = InteractionState::default();
  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&interaction_state))
    .expect("accessibility tree");
  let select = find_by_id(&tree, "s").expect("select node");
  assert_eq!(select.states.expanded, Some(false));
}

#[test]
fn native_single_select_all_disabled_defaults_to_first() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="all-disabled">
          <option id="disabled-opt1" disabled>One</option>
          <option id="disabled-opt2" disabled>Two</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let first = find_by_id(&tree, "disabled-opt1").expect("disabled option one");
  let second = find_by_id(&tree, "disabled-opt2").expect("disabled option two");
  assert_eq!(first.states.selected, Some(true));
  assert_eq!(second.states.selected, Some(false));
}

#[test]
fn closed_details_only_exposes_first_summary_and_hides_rest() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <details id="details">
          <summary id="summary-1">Summary</summary>
          <summary id="summary-2">Extra</summary>
          <div id="content">Content</div>
        </details>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let details = find_by_id(&tree, "details").expect("details element");
  assert_eq!(
    details.states.expanded,
    Some(false),
    "closed <details> should be reported as collapsed"
  );

  let summary_1 = find_by_id(&tree, "summary-1").expect("first summary should be exposed");
  assert_eq!(
    summary_1.states.expanded,
    Some(false),
    "summary button should inherit expanded state from its parent <details>"
  );

  assert!(
    find_by_id(&tree, "summary-2").is_none(),
    "only the first <summary> should be exposed when <details> is closed"
  );
  assert!(
    find_by_id(&tree, "content").is_none(),
    "details contents should be hidden from the accessibility tree when closed"
  );
}

#[test]
fn aria_state_does_not_negate_native_semantics() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <button id="x" disabled aria-disabled="false">Disabled</button>
        <input id="r" required aria-required="false" />
        <input id="inv" required aria-invalid="false" />
        <textarea id="ta" aria-label="Multiline" aria-multiline="false"></textarea>
        <input id="ml" type="text" aria-label="Single line" aria-multiline="true" />
        <input id="ro" readonly aria-readonly="false" />
        <input id="c" type="checkbox" checked aria-checked="false" />
        <select>
          <option id="o" selected aria-selected="false">Option</option>
        </select>
        <div id="custom" role="checkbox" aria-checked="mixed" tabindex="0">Custom</div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let disabled = find_by_id(&tree, "x").expect("disabled button");
  assert!(disabled.states.disabled);

  let required = find_by_id(&tree, "r").expect("required input");
  assert!(required.states.required);

  let invalid = find_by_id(&tree, "inv").expect("invalid input");
  assert!(invalid.states.invalid);

  let textarea = find_by_id(&tree, "ta").expect("textarea");
  assert_eq!(textarea.states.multiline, Some(true));

  let input = find_by_id(&tree, "ml").expect("input");
  assert_eq!(input.states.multiline, Some(false));

  let readonly = find_by_id(&tree, "ro").expect("readonly input");
  assert!(readonly.states.readonly);

  let checkbox = find_by_id(&tree, "c").expect("checkbox input");
  assert_eq!(checkbox.states.checked, Some(CheckState::True));

  let option = find_by_id(&tree, "o").expect("option element");
  assert_eq!(option.states.selected, Some(true));

  let custom = find_by_id(&tree, "custom").expect("custom checkbox");
  assert_eq!(custom.states.checked, Some(CheckState::Mixed));
}

#[test]
fn role_inference_and_heading_levels() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
     <html>
       <body>
        <button id="btn">Button text</button>
        <a id="link" href="#">Link text</a>
        <input id="checkbox" type="checkbox" />
        <input id="radio" type="radio" />
        <input id="textbox" type="text" value="Hello" />
        <select id="combo">
          <option id="combo-opt" selected>Combo option</option>
        </select>
        <select id="combo-size0" size="0">
          <option>Size zero</option>
        </select>
        <select id="combo-all-disabled">
          <option disabled>First</option>
          <option disabled>Second</option>
        </select>
        <select id="listbox" multiple>
          <option id="list-opt">List option</option>
        </select>
        <div id="custom-option" role="option" aria-selected="true" tabindex="0">Custom option</div>
        <div id="aria-heading" role="heading" aria-level="4">Aria heading</div>
        <div id="aria-heading-zero" role="heading" aria-level="0">Bad heading</div>
        <nav id="nav">Nav area</nav>
        <aside id="aside" aria-label="Sidebar">Sidebar</aside>
        <main id="page-main">Main area</main>
        <article id="article">
          <header id="article-header">Article heading</header>
          <footer id="article-footer">Article footer</footer>
          <main id="nested-main">Nested main</main>
        </article>
        <header id="page-header">Page header</header>
        <footer id="page-footer">Page footer</footer>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let button = find_by_id(&tree, "btn").expect("button");
  assert_eq!(button.role, "button");
  assert_eq!(button.name.as_deref(), Some("Button text"));

  let link = find_by_id(&tree, "link").expect("link");
  assert_eq!(link.role, "link");

  let checkbox = find_by_id(&tree, "checkbox").expect("checkbox");
  assert_eq!(checkbox.role, "checkbox");
  assert_eq!(checkbox.states.checked, Some(CheckState::False));

  let radio = find_by_id(&tree, "radio").expect("radio");
  assert_eq!(radio.role, "radio");
  assert_eq!(radio.states.checked, Some(CheckState::False));

  let textbox = find_by_id(&tree, "textbox").expect("textbox");
  assert_eq!(textbox.role, "textbox");
  assert_eq!(textbox.value.as_deref(), Some("Hello"));

  let combo = find_by_id(&tree, "combo").expect("combobox");
  assert_eq!(combo.role, "combobox");
  assert_eq!(combo.value.as_deref(), Some("Combo option"));
  let combo_opt = find_by_id(&tree, "combo-opt").expect("combo option");
  assert_eq!(combo_opt.role, "option");

  let combo_size0 = find_by_id(&tree, "combo-size0").expect("size=0 select");
  assert_eq!(combo_size0.role, "combobox");

  let combo_all_disabled = find_by_id(&tree, "combo-all-disabled").expect("all-disabled select");
  assert_eq!(combo_all_disabled.role, "combobox");
  assert_eq!(combo_all_disabled.value.as_deref(), Some("First"));

  let listbox = find_by_id(&tree, "listbox").expect("listbox");
  assert_eq!(listbox.role, "listbox");
  let list_opt = find_by_id(&tree, "list-opt").expect("list option");
  assert_eq!(list_opt.role, "option");
  assert_eq!(list_opt.states.selected, Some(false));

  let custom_option = find_by_id(&tree, "custom-option").expect("custom option");
  assert_eq!(custom_option.role, "option");
  assert_eq!(custom_option.states.selected, Some(true));
  assert!(custom_option.states.focusable);

  let aria_heading = find_by_id(&tree, "aria-heading").expect("aria heading");
  assert_eq!(aria_heading.role, "heading");
  assert_eq!(aria_heading.level, Some(4));

  let aria_heading_zero = find_by_id(&tree, "aria-heading-zero").expect("aria heading 0");
  assert_eq!(aria_heading_zero.role, "heading");
  assert_eq!(aria_heading_zero.level, Some(2));

  let nav = find_by_id(&tree, "nav").expect("nav");
  assert_eq!(nav.role, "navigation");

  let aside = find_by_id(&tree, "aside").expect("aside");
  assert_eq!(aside.role, "complementary");
  assert_eq!(aside.name.as_deref(), Some("Sidebar"));

  let page_main = find_by_id(&tree, "page-main").expect("page main");
  assert_eq!(page_main.role, "main");
  let nested_main = find_by_id(&tree, "nested-main").expect("nested main");
  assert_eq!(nested_main.role, "generic");

  let page_header = find_by_id(&tree, "page-header").expect("page header");
  assert_eq!(page_header.role, "banner");
  let article_header = find_by_id(&tree, "article-header").expect("article header");
  assert_eq!(article_header.role, "generic");

  let page_footer = find_by_id(&tree, "page-footer").expect("page footer");
  assert_eq!(page_footer.role, "contentinfo");
  let article_footer = find_by_id(&tree, "article-footer").expect("article footer");
  assert_eq!(article_footer.role, "generic");
}

#[test]
fn input_list_exposes_combobox_role_and_popup() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <input id="q" list="dl" />
        <input id="search" type="search" list="dl" />
        <input id="url" type="url" list="dl" />
        <input id="email" type="email" list="dl" />
        <input id="tel" type="tel" list="dl" />
        <input id="plain-search" type="search" />
        <input id="chk" type="checkbox" list="dl" />
        <datalist id="dl">
          <option value="One"></option>
          <option value="Two"></option>
        </datalist>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  for id in ["q", "search", "url", "email", "tel"] {
    let node = find_by_id(&tree, id).unwrap_or_else(|| panic!("missing {id} input"));
    assert_eq!(node.role, "combobox", "{id} should expose combobox role");
    assert_eq!(
      node.states.has_popup.as_deref(),
      Some("listbox"),
      "{id} should expose listbox popup"
    );
  }

  let plain_search = find_by_id(&tree, "plain-search").expect("plain search input");
  assert_eq!(plain_search.role, "searchbox");

  let checkbox = find_by_id(&tree, "chk").expect("checkbox input");
  assert_eq!(checkbox.role, "checkbox");
  assert_eq!(checkbox.states.has_popup, None);
}

#[test]
fn select_value_includes_disabled_selected_placeholder() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="s">
          <option id="placeholder" disabled selected>Placeholder</option>
          <option id="real">Real</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let select = find_by_id(&tree, "s").expect("select");
  assert_eq!(select.value.as_deref(), Some("Placeholder"));

  let placeholder = find_by_id(&tree, "placeholder").expect("placeholder option");
  assert_eq!(placeholder.states.selected, Some(true));
}

#[test]
fn required_multi_select_invalid_state_uses_dom_validity() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="only-disabled" multiple required>
          <option id="only-disabled-placeholder" value="placeholder" disabled selected>
            Placeholder
          </option>
          <option id="only-disabled-a" value="a">A</option>
        </select>

        <select id="with-enabled" multiple required>
          <option id="with-enabled-placeholder" value="placeholder" disabled selected>
            Placeholder
          </option>
          <option id="with-enabled-a" value="a" selected>A</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let only_disabled = find_by_id(&tree, "only-disabled").expect("only-disabled select");
  assert!(only_disabled.states.required);
  assert!(only_disabled.states.invalid);

  let only_disabled_placeholder =
    find_by_id(&tree, "only-disabled-placeholder").expect("only-disabled placeholder option");
  assert_eq!(only_disabled_placeholder.states.selected, Some(true));
  let only_disabled_a = find_by_id(&tree, "only-disabled-a").expect("only-disabled A option");
  assert_eq!(only_disabled_a.states.selected, Some(false));

  let with_enabled = find_by_id(&tree, "with-enabled").expect("with-enabled select");
  assert!(with_enabled.states.required);
  let with_enabled_placeholder =
    find_by_id(&tree, "with-enabled-placeholder").expect("with-enabled placeholder option");
  assert_eq!(with_enabled_placeholder.states.selected, Some(true));
  let with_enabled_a = find_by_id(&tree, "with-enabled-a").expect("with-enabled A option");
  assert_eq!(with_enabled_a.states.selected, Some(true));

  assert!(!with_enabled.states.invalid);
}

#[test]
fn select_last_selected_option_wins_for_single_select() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="s">
          <option id="first" selected>First</option>
          <option id="second" selected>Second</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let select = find_by_id(&tree, "s").expect("select");
  assert_eq!(select.value.as_deref(), Some("Second"));

  let first = find_by_id(&tree, "first").expect("first option");
  assert_eq!(first.states.selected, Some(false));
  let second = find_by_id(&tree, "second").expect("second option");
  assert_eq!(second.states.selected, Some(true));
}

#[test]
fn select_value_ignores_hidden_selected_options() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="s">
          <option id="visible">Visible</option>
          <option id="hidden" hidden selected>Hidden</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let select = find_by_id(&tree, "s").expect("select");
  assert_eq!(select.value.as_deref(), Some("Visible"));

  let visible = find_by_id(&tree, "visible").expect("visible option");
  assert_eq!(visible.states.selected, Some(true));

  assert!(find_by_id(&tree, "hidden").is_none());
}

#[test]
fn shadow_dom_nodes_keep_roles_and_names() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <div id="host">
          <template shadowroot="open">
            <button id="shadow-button" aria-pressed="true">Shadow Action</button>
            <header id="shadow-header">Shadow Header</header>
          </template>
        </div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let button = find_by_id(&tree, "shadow-button").expect("shadow button");
  assert_eq!(button.role, "button");
  assert_eq!(button.name.as_deref(), Some("Shadow Action"));
  assert_eq!(button.states.pressed, Some(PressedState::True));

  let header = find_by_id(&tree, "shadow-header").expect("shadow header");
  assert_eq!(header.name.as_deref(), Some("Shadow Header"));
  assert_eq!(header.role, "generic");
}

#[test]
fn select_placeholder_disabled_selected_exposes_value_text() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="placeholder" required>
          <option value="" disabled selected>Choose</option>
          <option value="x">X</option>
        </select>

        <select id="last-selected">
          <option selected>First</option>
          <option selected>Last</option>
        </select>

        <select id="hidden-selected">
          <option selected>Visible</option>
          <option selected hidden>Hidden</option>
        </select>

        <select id="first-enabled">
          <option disabled>Disabled</option>
          <option>Enabled</option>
        </select>

         <select id="label-attr">
           <option label="Label value">Text value</option>
         </select>

         <select id="empty-label-attr">
           <option label="">Text value</option>
         </select>

         <select id="all-disabled">
           <option disabled>A</option>
           <option disabled>B</option>
         </select>
       </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let placeholder = find_by_id(&tree, "placeholder").expect("placeholder select");
  assert_eq!(placeholder.value.as_deref(), Some("Choose"));
  assert!(placeholder.states.required);
  assert!(placeholder.states.invalid);

  let last_selected = find_by_id(&tree, "last-selected").expect("last-selected select");
  assert_eq!(last_selected.value.as_deref(), Some("Last"));

  let hidden_selected = find_by_id(&tree, "hidden-selected").expect("hidden-selected select");
  assert_eq!(hidden_selected.value.as_deref(), Some("Visible"));

  let first_enabled = find_by_id(&tree, "first-enabled").expect("first-enabled select");
  assert_eq!(first_enabled.value.as_deref(), Some("Enabled"));

  let label_attr = find_by_id(&tree, "label-attr").expect("label-attr select");
  assert_eq!(label_attr.value.as_deref(), Some("Label value"));

  let empty_label_attr = find_by_id(&tree, "empty-label-attr").expect("empty-label-attr select");
  assert_eq!(empty_label_attr.value.as_deref(), Some("Text value"));

  let all_disabled = find_by_id(&tree, "all-disabled").expect("all-disabled select");
  assert_eq!(all_disabled.value.as_deref(), Some("A"));
}

#[test]
fn option_value_uses_label_attribute_text() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="s">
          <option id="o" label="Label value">Text value</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let select = find_by_id(&tree, "s").expect("select");
  assert_eq!(select.value.as_deref(), Some("Label value"));

  let option = find_by_id(&tree, "o").expect("option");
  assert_eq!(option.name.as_deref(), Some("Label value"));
  assert_eq!(option.value.as_deref(), Some("Label value"));
}

#[test]
fn select_required_empty_value_is_not_invalid_when_not_placeholder_label() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="single" required>
          <option value="x">X</option>
          <option value="" selected>Empty</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let single = find_by_id(&tree, "single").expect("single select");
  assert!(!single.states.invalid);
}

#[test]
fn select_required_multi_select_empty_value_is_not_invalid() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="multi" required multiple>
          <option value="" selected>Empty</option>
          <option value="x">X</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let multi = find_by_id(&tree, "multi").expect("multi select");
  assert!(!multi.states.invalid);
}

#[test]
fn form_control_invalid_state_uses_control_semantics_not_dom_children() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <select id="optgroup-empty" required>
          <optgroup label="Group">
            <option id="optgroup-empty-opt" selected value="">Empty</option>
          </optgroup>
          <option value="x">X</option>
        </select>

        <select id="multi-missing" multiple required aria-label="Missing">
          <option>One</option>
        </select>

        <select id="multi-disabled-selected" multiple required aria-label="Selected">
          <option selected disabled>One</option>
        </select>

        <textarea id="ta-filled" required>Hi</textarea>
        <textarea id="ta-empty" required></textarea>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let optgroup_empty = find_by_id(&tree, "optgroup-empty").expect("optgroup select");
  assert!(optgroup_empty.states.required);
  assert!(
    !optgroup_empty.states.invalid,
    "empty value option inside optgroup is not a placeholder label option"
  );

  let multi_missing = find_by_id(&tree, "multi-missing").expect("missing multi-select");
  assert!(multi_missing.states.required);
  assert!(multi_missing.states.invalid);

  let multi_selected = find_by_id(&tree, "multi-disabled-selected").expect("selected multi-select");
  assert!(multi_selected.states.required);
  assert!(
    multi_selected.states.invalid,
    "disabled selections do not satisfy <select multiple required>"
  );

  let ta_filled = find_by_id(&tree, "ta-filled").expect("filled textarea");
  assert!(ta_filled.states.required);
  assert!(!ta_filled.states.invalid);

  let ta_empty = find_by_id(&tree, "ta-empty").expect("empty textarea");
  assert!(ta_empty.states.required);
  assert!(ta_empty.states.invalid);
}

#[test]
fn fieldset_disabled_exempts_first_legend_descendants() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <fieldset disabled>
          <legend>
            Legend
            <input id="in-legend" required />
          </legend>
          <input id="outside" required />
        </fieldset>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let in_legend = find_by_id(&tree, "in-legend").expect("legend input");
  assert!(!in_legend.states.disabled);
  assert!(in_legend.states.invalid);

  let outside = find_by_id(&tree, "outside").expect("outside input");
  assert!(outside.states.disabled);
  assert!(!outside.states.invalid);
}

#[test]
fn textarea_required_uses_text_content_value() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <textarea id="ta" required>ok</textarea>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let textarea = find_by_id(&tree, "ta").expect("textarea");
  assert!(textarea.states.required);
  assert!(!textarea.states.invalid);
  assert_eq!(textarea.value.as_deref(), Some("ok"));
}

#[test]
fn aria_idref_lists_dedupe_duplicate_tokens() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <div id="controller" aria-controls="target target target" aria-owns="owned owned">
          Controller
        </div>
        <div id="target">Target</div>
        <div id="owned">Owned</div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let controller = find_by_id(&tree, "controller").expect("controller");
  let relations = controller.relations.as_ref().expect("relations");
  assert_eq!(relations.controls, vec!["target".to_string()]);
  assert_eq!(relations.owns, vec!["owned".to_string()]);
}

#[test]
fn number_input_non_finite_value_is_invalid() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <input id="nan" type="number" value="NaN" />
        <input id="inf" type="number" value="Infinity" />
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let nan = find_by_id(&tree, "nan").expect("nan input");
  assert!(nan.states.invalid);

  let inf = find_by_id(&tree, "inf").expect("inf input");
  assert!(inf.states.invalid);
}
