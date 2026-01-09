use std::time::{Duration, Instant};

use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::parse_html;
use fastrender::style::cascade::{
  apply_styles_with_media, capture_style_sharing_stats, reset_style_sharing_stats,
  set_style_sharing_stats_enabled, StyledNode,
};
use fastrender::style::color::Rgba;
use fastrender::style::media::MediaContext;

const MAX_CASCADE_PERF_RUNS: usize = 5;

fn apply_styles_best_of<F>(budget: Duration, mut apply: F) -> (Duration, Vec<Duration>, StyledNode)
where
  F: FnMut() -> StyledNode,
{
  let mut best = Duration::MAX;
  let mut samples = Vec::with_capacity(MAX_CASCADE_PERF_RUNS);
  let mut last = None;

  for _ in 0..MAX_CASCADE_PERF_RUNS {
    let start = Instant::now();
    let styled = apply();
    let elapsed = start.elapsed();

    if elapsed < best {
      best = elapsed;
    }
    samples.push(elapsed);
    last = Some(styled);

    if best < budget {
      break;
    }
  }

  (
    best,
    samples,
    last.expect("cascade run produced styled tree"),
  )
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn display(node: &StyledNode) -> String {
  node.styles.display.to_string()
}

#[test]
fn cascade_handles_large_rule_sets_under_budget() {
  let variants = 600usize;
  let node_count = 180usize;

  let mut css = String::from(".card { display: inline-flex; }\n");
  for idx in 0..variants {
    css.push_str(&format!(
      ".c{idx} {{ padding: {}px; border-width: {}px; }}\n",
      (idx % 5) + 1,
      idx % 4
    ));
    css.push_str(&format!(
      ".c{idx}:has(.flag.f{}) {{ margin-left: {}px; }}\n",
      idx % 50,
      idx % 9
    ));
    css.push_str(&format!(
      ".c{idx} .content {{ min-height: {}px; }}\n",
      24 + (idx % 10)
    ));
  }
  let stylesheet = parse_stylesheet(&css).expect("stylesheet parses");

  let mut html = String::from("<div id=\"root\">");
  for idx in 0..node_count {
    html.push_str(&format!(
      "<div id=\"card{idx}\" class=\"card c{cls}\"><div class=\"content\"><span class=\"flag f{flag}\"></span></div></div>",
      cls = idx % variants,
      flag = idx % 50
    ));
  }
  html.push_str("</div>");
  let dom = parse_html(&html).expect("html parses");

  let media = MediaContext::screen(1280.0, 720.0);
  let budget = Duration::from_millis(1500);
  let (elapsed, samples, styled) = apply_styles_best_of(budget, || {
    apply_styles_with_media(&dom, &stylesheet, &media)
  });
  let samples_ms: Vec<u128> = samples.iter().map(|sample| sample.as_millis()).collect();

  assert!(
    elapsed < budget,
    "cascade perf regression: best {}ms (samples: {:?}) for {} rules over {} nodes",
    elapsed.as_millis(),
    samples_ms,
    variants * 3 + 1,
    node_count
  );

  assert_eq!(
    display(find_by_id(&styled, "card0").expect("card0 styled")),
    "inline-flex"
  );
}

#[test]
fn cascade_handles_thousands_of_has_rules_under_budget() {
  let variants = 1200usize;
  let node_count = 160usize;

  let mut css = String::from(".item { display: inline-flex; }\n");
  for idx in 0..variants {
    css.push_str(&format!(
      ".v{idx} {{ padding: {}px; border-width: {}px; }}\n",
      (idx % 5) + 1,
      idx % 3
    ));
    css.push_str(&format!(
      ".v{idx}:has(.flag{}) {{ margin-left: {}px; }}\n",
      idx % 60,
      (idx % 7) + 1
    ));
    css.push_str(&format!(
      ".v{idx} .body {{ min-height: {}px; }}\n",
      12 + (idx % 16)
    ));
  }
  let stylesheet = parse_stylesheet(&css).expect("stylesheet parses");

  let mut html = String::from("<div id=\"root\">");
  for idx in 0..node_count {
    html.push_str(&format!(
      "<div id=\"item{idx}\" class=\"item v{class}\"><div class=\"body\"><span class=\"flag{flag}\"></span></div></div>",
      class = idx % variants,
      flag = idx % 60,
    ));
  }
  html.push_str("</div>");
  let dom = parse_html(&html).expect("html parses");

  let media = MediaContext::screen(1440.0, 900.0);
  let budget = Duration::from_millis(2000);
  let (elapsed, samples, styled) = apply_styles_best_of(budget, || {
    apply_styles_with_media(&dom, &stylesheet, &media)
  });
  let samples_ms: Vec<u128> = samples.iter().map(|sample| sample.as_millis()).collect();

  assert!(
    elapsed < budget,
    "cascade perf regression: best {}ms (samples: {:?}) for {} rules over {} nodes",
    elapsed.as_millis(),
    samples_ms,
    variants * 3 + 1,
    node_count
  );

  assert_eq!(
    display(find_by_id(&styled, "item0").expect("item0 styled")),
    "inline-flex"
  );
}

#[test]
fn cascade_handles_many_custom_properties_under_budget() {
  // Heavy custom-property pages (e.g. GitHub, large design systems) often define hundreds or
  // thousands of variables on the root element. The cascade must not clone that entire map for
  // every node.
  let var_count = 800usize;
  let item_count = 2500usize;

  let mut css = String::new();
  css.push_str("#root {");
  for idx in 0..var_count {
    css.push_str(&format!("--v{idx}: {}px;", (idx % 50) + 1));
  }
  css.push_str("}\n");
  css.push_str(
    ".item { padding-left: var(--v0); padding-right: var(--v1); margin-top: var(--v2); }\n",
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet parses");

  let mut html = String::from("<div id=\"root\">");
  for idx in 0..item_count {
    html.push_str(&format!(
      "<div id=\"item{idx}\" class=\"item\"><span class=\"inner\"></span></div>"
    ));
  }
  html.push_str("</div>");
  let dom = parse_html(&html).expect("html parses");

  let media = MediaContext::screen(1280.0, 720.0);
  let budget = Duration::from_millis(2500);
  let (elapsed, samples, styled) = apply_styles_best_of(budget, || {
    apply_styles_with_media(&dom, &stylesheet, &media)
  });
  let samples_ms: Vec<u128> = samples.iter().map(|sample| sample.as_millis()).collect();

  assert!(
    elapsed < budget,
    "cascade perf regression: best {}ms (samples: {:?}) for {} custom properties over {} nodes",
    elapsed.as_millis(),
    samples_ms,
    var_count,
    item_count * 2 + 1
  );

  let first = find_by_id(&styled, "item0").expect("item0 styled");
  assert_eq!(first.styles.padding_left.value, 1.0);
  assert_eq!(first.styles.padding_left.unit, fastrender::LengthUnit::Px);
}

#[test]
fn cascade_handles_many_keyword_declarations_under_budget() {
  let variants = 400usize;
  let node_count = 600usize;
  let classes_per_node = 20usize;

  let mut css = String::new();
  for idx in 0..variants {
    css.push_str(&format!(
      ".c{idx} {{ display: block; position: relative; overflow: hidden; visibility: visible; float: none; clear: none; box-sizing: border-box; text-transform: none; white-space: normal; word-break: normal; }}\n"
    ));
  }
  let stylesheet = parse_stylesheet(&css).expect("stylesheet parses");

  let mut html = String::from("<div id=\"root\">");
  for idx in 0..node_count {
    let mut classes = String::new();
    for class_idx in 0..classes_per_node {
      if class_idx > 0 {
        classes.push(' ');
      }
      classes.push_str(&format!("c{}", (idx + class_idx) % variants));
    }

    html.push_str(&format!("<div id=\"node{idx}\" class=\"{classes}\"></div>"));
  }
  html.push_str("</div>");
  let dom = parse_html(&html).expect("html parses");

  let media = MediaContext::screen(1280.0, 720.0);
  let budget = Duration::from_millis(800);
  let (elapsed, samples, styled) = apply_styles_best_of(budget, || {
    apply_styles_with_media(&dom, &stylesheet, &media)
  });
  let samples_ms: Vec<u128> = samples.iter().map(|sample| sample.as_millis()).collect();

  assert!(
    elapsed < budget,
    "cascade perf regression: best {}ms (samples: {:?}) for {} rules over {} nodes",
    elapsed.as_millis(),
    samples_ms,
    variants,
    node_count
  );

  assert_eq!(
    display(find_by_id(&styled, "node0").expect("node0 styled")),
    "block"
  );
}

#[test]
fn style_sharing_hits_for_repeated_simple_elements() {
  set_style_sharing_stats_enabled(true);
  reset_style_sharing_stats();

  let css = ".item { display: block; padding-left: 4px; padding-right: 8px; }\n";
  let stylesheet = parse_stylesheet(css).expect("stylesheet parses");

  let count = 2000usize;
  let mut html = String::from("<div id=\"root\">");
  for _ in 0..count {
    html.push_str("<div class=\"item\"></div>");
  }
  html.push_str("</div>");
  let dom = parse_html(&html).expect("html parses");

  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_styles_with_media(&dom, &stylesheet, &media);

  let stats = capture_style_sharing_stats();
  set_style_sharing_stats_enabled(false);

  let root = find_by_id(&styled, "root").expect("root styled");
  let first = root
    .children
    .first()
    .expect("root should contain repeated children");
  assert_eq!(first.styles.padding_left.value, 4.0);
  assert_eq!(first.styles.padding_left.unit, fastrender::LengthUnit::Px);

  assert!(
    stats.hits >= (count.saturating_sub(1) as u64),
    "expected style sharing hits for repeated nodes, got {stats:?}"
  );
}

#[test]
fn style_sharing_disabled_for_nth_child_selectors() {
  set_style_sharing_stats_enabled(true);
  reset_style_sharing_stats();

  let css = ".item { color: rgb(0, 0, 255); }\n.item:nth-child(odd) { color: rgb(255, 0, 0); }\n";
  let stylesheet = parse_stylesheet(css).expect("stylesheet parses");

  let count = 64usize;
  let mut html = String::from("<div id=\"root\">");
  for _ in 0..count {
    html.push_str("<div class=\"item\"></div>");
  }
  html.push_str("</div>");
  let dom = parse_html(&html).expect("html parses");

  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_styles_with_media(&dom, &stylesheet, &media);

  let stats = capture_style_sharing_stats();
  set_style_sharing_stats_enabled(false);

  assert_eq!(
    stats.hits, 0,
    "expected no style sharing hits, got {stats:?}"
  );

  let root = find_by_id(&styled, "root").expect("root styled");
  let first = root.children.get(0).expect("first item styled");
  let second = root.children.get(1).expect("second item styled");
  assert_eq!(first.styles.color, Rgba::rgb(255, 0, 0));
  assert_eq!(second.styles.color, Rgba::rgb(0, 0, 255));
}

#[test]
fn style_sharing_does_not_mix_case_sensitive_attribute_values() {
  set_style_sharing_stats_enabled(true);
  reset_style_sharing_stats();

  let css =
    ".item { color: rgb(0, 0, 255); }\n[data-x=\"Foo\"] { color: rgb(255, 0, 0); }\n";
  let stylesheet = parse_stylesheet(css).expect("stylesheet parses");

  let html = "<div id=\"root\"><div class=\"item\" data-x=\"Foo\"></div><div class=\"item\" data-x=\"foo\"></div></div>";
  let dom = parse_html(html).expect("html parses");

  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_styles_with_media(&dom, &stylesheet, &media);

  let _stats = capture_style_sharing_stats();
  set_style_sharing_stats_enabled(false);

  let root = find_by_id(&styled, "root").expect("root styled");
  let first = root.children.get(0).expect("first item styled");
  let second = root.children.get(1).expect("second item styled");
  assert_eq!(first.styles.color, Rgba::rgb(255, 0, 0));
  assert_eq!(second.styles.color, Rgba::rgb(0, 0, 255));
}
