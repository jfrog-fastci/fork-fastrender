use proc_macro2::Span;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Attribute, ImplItem, Item, Meta, Token, TraitItem};
use walkdir::WalkDir;

#[test]
fn no_production_panics_in_core_modules() {
  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let src_dir = manifest_dir.join("src");

  let mut offenders: BTreeMap<PathBuf, Vec<Span>> = BTreeMap::new();

  for entry in WalkDir::new(&src_dir)
    .into_iter()
    .filter_map(std::result::Result::ok)
    .filter(|entry| entry.file_type().is_file())
  {
    let path = entry.path();
    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
      continue;
    }

    let rel = path.strip_prefix(&src_dir).expect("walkdir root mismatch");

    // Exclude delegated modules owned by other workers.
    if rel == Path::new("resource.rs") {
      continue;
    }
    if let Some(first) = rel.components().next().and_then(|c| c.as_os_str().to_str()) {
      if matches!(
        first,
        "api"
          | "bin"
          | "css"
          | "js"
          | "layout"
          | "paint"
          | "resource"
          | "style"
          | "text"
          | "webidl"
      ) {
        continue;
      }
    }

    // Exclude test-only module files (these are usually pulled in via `#[cfg(test)] mod ...;` from
    // their parent module).
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if file_name == "tests.rs" || file_name.ends_with("_tests.rs") {
      continue;
    }

    let source = fs::read_to_string(path).expect("read source file");
    let parsed = syn::parse_file(&source)
      .unwrap_or_else(|err| panic!("failed to parse {}: {err}", rel.display()));

    // Skip files that are test-only via a file-level inner attribute (e.g. `#![cfg(test)]` or
    // `#![cfg(all(test, feature = "..."))]`).
    if attrs_have_cfg_test(&parsed.attrs) {
      continue;
    }

    let mut visitor = PanicVisitor::default();
    visitor.visit_file(&parsed);
    if !visitor.panics.is_empty() {
      offenders.insert(rel.to_path_buf(), visitor.panics);
    }
  }

  if offenders.is_empty() {
    return;
  }

  let mut formatted = String::new();
  for (path, spans) in offenders {
    let mut lines: Vec<_> = spans.into_iter().map(|span| span.start().line).collect();
    lines.sort_unstable();
    lines.dedup();
    formatted.push_str(&format!("- {}:{}\n", path.display(), join_lines(&lines)));
  }

  panic!(
    "Found `panic!` in production code (must use `Error`/fallback/`debug_assert!` instead):\n{formatted}"
  );
}

fn join_lines(lines: &[usize]) -> String {
  let mut out = String::new();
  for (idx, line) in lines.iter().enumerate() {
    if idx > 0 {
      out.push(',');
    }
    out.push_str(&line.to_string());
  }
  out
}

#[derive(Default)]
struct PanicVisitor {
  panics: Vec<Span>,
}

impl<'ast> Visit<'ast> for PanicVisitor {
  fn visit_item(&mut self, item: &'ast Item) {
    if item_has_cfg_test(item) {
      return;
    }
    syn::visit::visit_item(self, item);
  }

  fn visit_impl_item(&mut self, item: &'ast ImplItem) {
    if impl_item_has_cfg_test(item) {
      return;
    }
    syn::visit::visit_impl_item(self, item);
  }

  fn visit_trait_item(&mut self, item: &'ast TraitItem) {
    if trait_item_has_cfg_test(item) {
      return;
    }
    syn::visit::visit_trait_item(self, item);
  }

  fn visit_macro(&mut self, mac: &'ast syn::Macro) {
    if mac
      .path
      .segments
      .last()
      .is_some_and(|segment| segment.ident == "panic")
    {
      self.panics.push(mac.span());
    }
    collect_panic_spans_in_token_stream(mac.tokens.clone(), &mut self.panics);
    syn::visit::visit_macro(self, mac);
  }
}

fn collect_panic_spans_in_token_stream(tokens: proc_macro2::TokenStream, out: &mut Vec<Span>) {
  use proc_macro2::TokenTree;

  let mut iter = tokens.into_iter().peekable();
  while let Some(tt) = iter.next() {
    match tt {
      TokenTree::Group(group) => collect_panic_spans_in_token_stream(group.stream(), out),
      TokenTree::Ident(ident) => {
        if ident.to_string() != "panic" {
          continue;
        }

        let Some(TokenTree::Punct(punct)) = iter.peek() else {
          continue;
        };
        if punct.as_char() != '!' {
          continue;
        }

        // Consume `!` so nested patterns like `panic!!()` don't repeatedly match.
        let _ = iter.next();
        if matches!(iter.peek(), Some(TokenTree::Group(_))) {
          out.push(ident.span());
        }
      }
      _ => {}
    }
  }
}

fn item_has_cfg_test(item: &Item) -> bool {
  match item {
    Item::Const(item) => attrs_have_cfg_test(&item.attrs),
    Item::Enum(item) => attrs_have_cfg_test(&item.attrs),
    Item::ExternCrate(item) => attrs_have_cfg_test(&item.attrs),
    Item::Fn(item) => attrs_have_cfg_test(&item.attrs),
    Item::ForeignMod(item) => attrs_have_cfg_test(&item.attrs),
    Item::Impl(item) => attrs_have_cfg_test(&item.attrs),
    Item::Macro(item) => attrs_have_cfg_test(&item.attrs),
    Item::Mod(item) => attrs_have_cfg_test(&item.attrs),
    Item::Static(item) => attrs_have_cfg_test(&item.attrs),
    Item::Struct(item) => attrs_have_cfg_test(&item.attrs),
    Item::Trait(item) => attrs_have_cfg_test(&item.attrs),
    Item::TraitAlias(item) => attrs_have_cfg_test(&item.attrs),
    Item::Type(item) => attrs_have_cfg_test(&item.attrs),
    Item::Union(item) => attrs_have_cfg_test(&item.attrs),
    Item::Use(item) => attrs_have_cfg_test(&item.attrs),
    _ => false,
  }
}

fn impl_item_has_cfg_test(item: &ImplItem) -> bool {
  match item {
    ImplItem::Const(item) => attrs_have_cfg_test(&item.attrs),
    ImplItem::Fn(item) => attrs_have_cfg_test(&item.attrs),
    ImplItem::Macro(item) => attrs_have_cfg_test(&item.attrs),
    ImplItem::Type(item) => attrs_have_cfg_test(&item.attrs),
    ImplItem::Verbatim(_) => false,
    _ => false,
  }
}

fn trait_item_has_cfg_test(item: &TraitItem) -> bool {
  match item {
    TraitItem::Const(item) => attrs_have_cfg_test(&item.attrs),
    TraitItem::Fn(item) => attrs_have_cfg_test(&item.attrs),
    TraitItem::Macro(item) => attrs_have_cfg_test(&item.attrs),
    TraitItem::Type(item) => attrs_have_cfg_test(&item.attrs),
    TraitItem::Verbatim(_) => false,
    _ => false,
  }
}

fn attrs_have_cfg_test(attrs: &[Attribute]) -> bool {
  attrs.iter().any(|attr| {
    if !attr.path().is_ident("cfg") {
      return false;
    }
    let Ok(meta) = attr.parse_args::<Meta>() else {
      return false;
    };
    cfg_expr_implies_test(&meta)
  })
}

fn cfg_expr_implies_test(meta: &Meta) -> bool {
  match meta {
    Meta::Path(path) => path.is_ident("test"),
    Meta::List(list) => {
      let Some(ident) = list.path.get_ident() else {
        return false;
      };

      let nested: Punctuated<Meta, Token![,]> = Punctuated::parse_terminated
        .parse2(list.tokens.clone())
        .unwrap_or_default();

      if ident == "all" {
        return nested.iter().any(cfg_expr_implies_test);
      }
      if ident == "any" {
        // `cfg(any())` is always false and should not be treated as test-only.
        return !nested.is_empty() && nested.iter().all(cfg_expr_implies_test);
      }

      false
    }
    Meta::NameValue(_) => false,
  }
}
