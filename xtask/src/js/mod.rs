use anyhow::Result;
use clap::{Args, Subcommand};

mod test262;
mod test262_negative_parse;
mod test262_parser;
mod test262_report;
mod wpt_dom;

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
pub struct JsArgs {
  #[command(subcommand)]
  command: JsCommand,
}

#[derive(Subcommand, Debug)]
enum JsCommand {
  /// Run a curated subset of tc39/test262 language semantics tests.
  Test262(test262::Test262Args),
  /// Rebuild and run only negative parse SyntaxError tests from the curated suite and list parse-vs-runtime mismatches.
  ///
  /// Note: this command always rebuilds the `test262-semantic` runner first to avoid stale-binary
  /// false negatives when iterating on the JS engine.
  #[command(name = "test262-negative-parse", alias = "test262_negative_parse")]
  Test262NegativeParse(test262_negative_parse::Test262NegativeParseArgs),
  /// Run the tc39/test262-parser-tests harness (via ecma-rs `test262`).
  #[command(name = "test262-parser", alias = "test262_parser")]
  Test262Parser(test262_parser::Test262ParserArgs),
  /// Run the offline WPT DOM (`testharness.js`) corpus under `tests/wpt_dom/`.
  #[command(name = "wpt-dom")]
  WptDom(wpt_dom::WptDomArgs),
}

pub fn run_js(args: JsArgs) -> Result<()> {
  match args.command {
    JsCommand::Test262(args) => test262::run_test262(args),
    JsCommand::Test262NegativeParse(args) => {
      test262_negative_parse::run_test262_negative_parse(args)
    }
    JsCommand::Test262Parser(args) => test262_parser::run_test262_parser(args),
    JsCommand::WptDom(args) => wpt_dom::run_wpt_dom(args),
  }
}
