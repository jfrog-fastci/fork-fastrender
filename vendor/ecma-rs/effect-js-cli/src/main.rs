use clap::{Parser, Subcommand};
use diagnostics::FileId;
use effect_js::{recognize_patterns_best_effort_untyped, resolve_api_call_best_effort_untyped, ApiId, RecognizedPattern};
use hir_js::{ExprId, ExprKind, FileKind, ObjectKey};
use knowledge_base::KnowledgeBase;
use parse_js::{parse_with_options, ParseOptions};
use std::fs;
use std::path::PathBuf;
use std::process::exit;

#[derive(Parser, Debug)]
#[command(author, version)]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
  /// Inspect the semantic knowledge base.
  Kb {
    #[command(subcommand)]
    command: KbCommand,
  },
  /// Parse and analyze a single source file.
  Analyze {
    #[arg(value_name = "FILE")]
    file: PathBuf,

    /// Parse as TypeScript (default).
    #[arg(long, conflicts_with = "tsx")]
    ts: bool,

    /// Parse as TSX.
    #[arg(long)]
    tsx: bool,
  },
}

#[derive(Subcommand, Debug)]
enum KbCommand {
  /// Print canonical API names (sorted).
  List,
  /// Show a single entry by canonical name or alias.
  Show {
    #[arg(value_name = "NAME")]
    name: String,
  },
}

fn main() {
  let cli = Cli::parse();

  match cli.command {
    Command::Kb { command } => run_kb(command),
    Command::Analyze { file, ts, tsx } => run_analyze(file, ts, tsx),
  }
}

fn run_kb(command: KbCommand) {
  let kb = load_kb_or_exit();
  match command {
    KbCommand::List => {
      for (name, _) in kb.iter() {
        println!("{name}");
      }
    }
    KbCommand::Show { name } => {
      let Some(entry) = kb
        .get(&name)
        .or_else(|| kb.iter().find(|(_, entry)| entry.aliases.iter().any(|a| a == &name)).map(|(_, entry)| entry))
      else {
        eprintln!("unknown API entry: {name}");
        exit(2);
      };

      println!("name: {}", entry.name);
      if !entry.aliases.is_empty() {
        println!("aliases:");
        for alias in entry.aliases.iter() {
          println!("  - {alias}");
        }
      }
      println!("effects: {:?}", entry.effects);
      println!("purity: {:?}", entry.purity);
      if !entry.properties.is_empty() {
        println!("properties:");
        for (k, v) in entry.properties.iter() {
          println!("  {k}: {v}");
        }
      }
    }
  }
}

fn run_analyze(file: PathBuf, ts: bool, tsx: bool) {
  let source = fs::read_to_string(&file).unwrap_or_else(|err| {
    eprintln!("failed to read {}: {err}", file.display());
    std::process::exit(2);
  });

  let dialect = if tsx {
    parse_js::Dialect::Tsx
  } else if ts {
    parse_js::Dialect::Ts
  } else {
    // Default.
    parse_js::Dialect::Ts
  };

  let parsed = parse_with_options(
    &source,
    ParseOptions {
      dialect,
      source_type: parse_js::SourceType::Module,
    },
  )
  .unwrap_or_else(|err| {
    eprintln!("parse error: {err}");
    std::process::exit(1);
  });

  let file_kind = match dialect {
    parse_js::Dialect::Tsx => FileKind::Tsx,
    _ => FileKind::Ts,
  };

  let lowered = hir_js::lower_file(FileId(0), file_kind, &parsed);
  let kb = load_kb_or_exit();

  let calls = resolve_calls_best_effort(&lowered);
  println!("== API resolution (best-effort) ==");
  if calls.is_empty() {
    println!("(no resolved calls)");
  } else {
    for call in calls {
      // Show KB details when available; otherwise fall back to the canonical API name.
      if let Some(entry) = kb.get(call.api.as_str()) {
        println!(
          "[{}..{}] {} => {} (effects={:?}, purity={:?})",
          call.span.start,
          call.span.end,
          call.call_text,
          call.api,
          entry.effects,
          entry.purity,
        );
      } else {
        println!(
          "[{}..{}] {} => {}",
          call.span.start, call.span.end, call.call_text, call.api
        );
      }
    }
  }

  let patterns = recognize_patterns_best_effort(&lowered);
  println!();
  println!("== Patterns ==");
  if patterns.is_empty() {
    println!("(no patterns)");
  } else {
    for pattern in patterns {
      println!("{}", pattern);
    }
  }
}

fn load_kb_or_exit() -> KnowledgeBase {
  KnowledgeBase::load_default().unwrap_or_else(|err| {
    eprintln!("failed to load default knowledge base: {err}");
    exit(1);
  })
}

#[derive(Debug, Clone)]
struct ResolvedCall {
  span: diagnostics::TextRange,
  call_text: String,
  api: ApiId,
}

fn resolve_calls_best_effort(lowered: &hir_js::LowerResult) -> Vec<ResolvedCall> {
  let mut out = Vec::new();
  for (body_id, _) in lowered.body_index.iter() {
    let Some(body) = lowered.body(*body_id) else {
      continue;
    };
    for (idx, expr) in body.exprs.iter().enumerate() {
      if !matches!(expr.kind, ExprKind::Call(_)) {
        continue;
      }
      let expr_id = ExprId(idx as u32);
      let Some(api) = resolve_api_call_best_effort_untyped(lowered, *body_id, expr_id) else {
        continue;
      };
      let call_text = format_expr_path(body, &lowered.names, expr_id, 8);
      out.push(ResolvedCall {
        span: expr.span,
        call_text,
        api,
      });
    }
  }
  out.sort_by(|a, b| {
    (a.span.start, a.span.end, a.api.as_str(), a.call_text.as_str()).cmp(&(
      b.span.start,
      b.span.end,
      b.api.as_str(),
      b.call_text.as_str(),
    ))
  });
  out
}

fn recognize_patterns_best_effort(lowered: &hir_js::LowerResult) -> Vec<PatternLine> {
  let mut out = Vec::new();
  for (body_id, _) in lowered.body_index.iter() {
    let Some(body) = lowered.body(*body_id) else {
      continue;
    };
    for pat in recognize_patterns_best_effort_untyped(lowered, *body_id) {
      match pat {
        RecognizedPattern::CanonicalCall { .. } => {}
        RecognizedPattern::MapFilterReduce {
          base,
          map_call,
          filter_call,
          reduce_call,
        } => {
          let span = diagnostics::TextRange::new(
            body.exprs[base.0 as usize].span.start,
            body.exprs[reduce_call.0 as usize].span.end,
          );
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] MapFilterReduce: base={} map={} filter={} reduce={}",
              span.start, span.end, base.0, map_call.0, filter_call.0, reduce_call.0
            ),
          });
        }
        RecognizedPattern::PromiseAllFetch { all_call, fetch_calls } => {
          let span = body.exprs[all_call.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] PromiseAllFetch: Promise.all + fetch (fetch_calls={})",
              span.start,
              span.end,
              fetch_calls.len()
            ),
          });
        }
        RecognizedPattern::MapGetOrDefault { map, key, default } => {
          let span = diagnostics::TextRange::new(
            body.exprs[map.0 as usize].span.start,
            body.exprs[default.0 as usize].span.end,
          );
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] MapGetOrDefault: map={} key={} default={}",
              span.start, span.end, map.0, key.0, default.0
            ),
          });
        }
        RecognizedPattern::JsonParseTyped { call, target } => {
          let span = body.exprs[call.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] JsonParseTyped: call={} target_type={}",
              span.start, span.end, call.0, target.0
            ),
          });
        }
      }
    }
  }
  out.sort_by(|a, b| (a.span.start, a.span.end, a.text.as_str()).cmp(&(b.span.start, b.span.end, b.text.as_str())));
  out
}

#[derive(Debug, Clone)]
struct PatternLine {
  span: diagnostics::TextRange,
  text: String,
}

impl std::fmt::Display for PatternLine {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.text)
  }
}

fn format_expr_path(
  body: &hir_js::Body,
  names: &hir_js::NameInterner,
  expr: ExprId,
  budget: usize,
) -> String {
  if budget == 0 {
    return "…".to_string();
  }
  let Some(expr) = body.exprs.get(expr.0 as usize) else {
    return "<missing>".to_string();
  };
  match &expr.kind {
    ExprKind::Ident(id) => names.resolve(*id).unwrap_or("<unknown>").to_string(),
    ExprKind::This => "this".to_string(),
    ExprKind::Super => "super".to_string(),
    ExprKind::Member(member) => {
      let object = format_expr_path(body, names, member.object, budget.saturating_sub(1));
      let prop = match &member.property {
        ObjectKey::Ident(id) => names.resolve(*id).unwrap_or("<unknown>"),
        ObjectKey::String(s) => s.as_str(),
        ObjectKey::Number(n) => n.as_str(),
        ObjectKey::Computed(_) => "[computed]",
      };
      format!("{object}.{prop}")
    }
    ExprKind::Call(call) => {
      let callee = format_expr_path(body, names, call.callee, budget.saturating_sub(1));
      format!("{callee}(...)")
    }
    _ => "<expr>".to_string(),
  }
}
