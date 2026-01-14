use crate::ast::expr::pat::Pat;
use crate::ast::expr::Expr;
use crate::ast::node::Node;
use crate::error::SyntaxError;
use crate::error::SyntaxErrorType;
use crate::error::SyntaxResult;
use crate::lex::lex_next;
use crate::lex::LexMode;
use crate::lex::Lexer;
use crate::loc::Loc;
use crate::operator::Arity;
use crate::token::Token;
use crate::token::TT;
use crate::token::UNRESERVED_KEYWORDS;
use crate::Dialect;
use crate::ParseOptions;
use crate::SourceType;
use expr::pat::ParsePatternRules;
use operator::MULTARY_OPERATOR_MAPPING;
use std::borrow::Cow;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Arc;

pub mod class_or_object;
pub mod drive;
pub mod expr;
pub mod func;
pub mod import_export;
pub mod operator;
pub mod stmt;
#[cfg(test)]
mod tests;
pub mod top_level;
pub mod ts_decl;
pub mod type_expr;

// Almost every parse_* function takes these field values as parameters. Instead of having to enumerate them as parameters on every function and ordered unnamed arguments on every call, we simply pass this struct around. Fields are public to allow destructuring, but the value should be immutable; the with_* methods can be used to create an altered copy for passing into other functions, which is useful as most calls simply pass through the values unchanged. This struct should be received as a value, not a reference (i.e. `ctx: ParseCtx` not `ctx: &ParseCtx`) as the latter will require a separate lifetime.
// All fields except `session` can (although not often) change between calls, so we don't simply put them in Parser, as otherwise we'd have to "unwind" (i.e. reset) those values after each call returns.
#[derive(Clone, Copy)]
pub struct ParseCtx {
  pub rules: ParsePatternRules, // For simplicity, this is a copy, not a non-mutable reference, to avoid having a separate lifetime for it. The value is a small set of booleans, so a reference is probably slower, and it's supposed to be immutable (i.e. changes come from altered copying, not mutating the original single instance), so there shouldn't be any difference between a reference and a copy.
  pub top_level: bool,
  pub in_namespace: bool,
  pub asi: AsiContext,
}

impl ParseCtx {
  pub fn with_rules(&self, rules: ParsePatternRules) -> ParseCtx {
    ParseCtx { rules, ..*self }
  }

  pub fn with_top_level(&self, top_level: bool) -> ParseCtx {
    ParseCtx { top_level, ..*self }
  }

  pub fn with_namespace(&self, in_namespace: bool) -> ParseCtx {
    ParseCtx {
      in_namespace,
      ..*self
    }
  }

  pub fn non_top_level(&self) -> ParseCtx {
    ParseCtx {
      top_level: false,
      ..*self
    }
  }

  pub fn namespace_body(&self) -> ParseCtx {
    ParseCtx {
      top_level: true,
      in_namespace: true,
      ..*self
    }
  }

  pub fn with_asi(&self, asi: AsiContext) -> ParseCtx {
    ParseCtx { asi, ..*self }
  }

  pub fn for_statement_header(&self) -> ParseCtx {
    self.with_asi(AsiContext::StatementHeader)
  }
}

#[derive(Clone, Copy)]
pub enum AsiContext {
  Statements,
  StatementHeader,
}

impl AsiContext {
  pub fn allows_asi(self) -> bool {
    matches!(self, AsiContext::Statements)
  }
}

#[derive(Debug)]
#[must_use]
pub struct MaybeToken {
  typ: TT,
  loc: Loc,
  matched: bool,
}

impl MaybeToken {
  pub fn is_match(&self) -> bool {
    self.matched
  }

  pub fn match_loc(&self) -> Option<Loc> {
    if self.matched {
      Some(self.loc)
    } else {
      None
    }
  }

  pub fn error(&self, err: SyntaxErrorType) -> SyntaxError {
    debug_assert!(!self.matched);
    self.loc.error(err, Some(self.typ))
  }

  pub fn map<R, F: FnOnce(Self) -> R>(self, f: F) -> Option<R> {
    if self.matched {
      Some(f(self))
    } else {
      None
    }
  }

  pub fn and_then<R, F: FnOnce() -> SyntaxResult<R>>(self, f: F) -> SyntaxResult<Option<R>> {
    Ok(if self.matched { Some(f()?) } else { None })
  }
}

#[derive(Clone, Copy)]
pub struct ParserCheckpoint {
  next_tok_i: usize,
}

/// To get the lexer's `next` after this token was lexed, use `token.loc.1`.
struct BufferedToken {
  token: Token,
  lex_mode: LexMode,
}

#[derive(Clone)]
struct LabelInfo {
  name: String,
  is_iteration: bool,
}

pub struct Parser<'a> {
  lexer: Lexer<'a>,
  buf: Vec<BufferedToken>,
  next_tok_i: usize,
  options: ParseOptions,
  allow_bare_ts_type_args: bool,
  allow_top_level_await_in_script: bool,
  allow_top_level_yield: bool,
  strict_mode: u32,
  in_function: u32,
  new_target_allowed: u32,
  super_prop_allowed: u32,
  super_call_allowed: u32,
  class_is_derived: Vec<bool>,
  in_iteration: u32,
  in_switch: u32,
  labels: Vec<LabelInfo>,
  /// Depth of the nearest non-arrow function scope that provides an `arguments` binding.
  ///
  /// - Non-arrow functions introduce an `arguments` binding for their parameters/body.
  /// - Arrow functions do **not** introduce an `arguments` binding and instead inherit this value
  ///   from their enclosing scope.
  ///
  /// This is used to implement early errors for class field initializers / static blocks where
  /// `arguments` is syntactically disallowed unless shadowed by an inner non-arrow function.
  arguments_allowed: u32,
  /// Depth of "class initialization" parsing contexts where identifier reference to `arguments` is
  /// disallowed (class field initializers and `static {}` blocks).
  disallow_arguments_in_class_init: u32,
  cancel: Option<Arc<AtomicBool>>,
  cancel_check: Option<Box<dyn FnMut() -> bool + 'a>>,
}

// We extend this struct with added methods in the various submodules, instead of simply using free functions and passing `&mut Parser` around, for several reasons:
// - Avoid needing to redeclare `<'a>` on every function.
// - More lifetime elision is available for `self` than if it was just another reference parameter.
// - Don't need to import each function.
// - Autocomplete is more specific since `self.*` narrows down the options instead of just listing all visible functions.
// - For general consistency; if there's no reason why it should be a free function (e.g. more than one ambiguous base type), it should be a method.
// - Makes free functions truly separate independent utility functions.
impl<'a> Parser<'a> {
  pub fn new(lexer: Lexer<'a>, options: ParseOptions) -> Parser<'a> {
    Self::new_cancellable(lexer, options, None)
  }

  pub fn new_cancellable(
    lexer: Lexer<'a>,
    options: ParseOptions,
    cancel: Option<Arc<AtomicBool>>,
  ) -> Parser<'a> {
    Parser {
      lexer,
      buf: Vec::new(),
      next_tok_i: 0,
      options,
      allow_bare_ts_type_args: false,
      allow_top_level_await_in_script: false,
      allow_top_level_yield: false,
      strict_mode: 0,
      in_function: 0,
      new_target_allowed: 0,
      super_prop_allowed: 0,
      super_call_allowed: 0,
      class_is_derived: Vec::new(),
      in_iteration: 0,
      in_switch: 0,
      labels: Vec::new(),
      arguments_allowed: 0,
      disallow_arguments_in_class_init: 0,
      cancel,
      cancel_check: None,
    }
  }

  pub fn new_cancellable_by(
    lexer: Lexer<'a>,
    options: ParseOptions,
    cancel_check: Box<dyn FnMut() -> bool + 'a>,
  ) -> Parser<'a> {
    Parser {
      lexer,
      buf: Vec::new(),
      next_tok_i: 0,
      options,
      allow_bare_ts_type_args: false,
      allow_top_level_await_in_script: false,
      allow_top_level_yield: false,
      strict_mode: 0,
      in_function: 0,
      new_target_allowed: 0,
      super_prop_allowed: 0,
      super_call_allowed: 0,
      class_is_derived: Vec::new(),
      in_iteration: 0,
      in_switch: 0,
      labels: Vec::new(),
      arguments_allowed: 0,
      disallow_arguments_in_class_init: 0,
      cancel: None,
      cancel_check: Some(cancel_check),
    }
  }

  /// Overrides the parser's initial grammar context for `new.target` and `super` expressions.
  ///
  /// `new.target` and `super` are only syntactically valid when the surrounding lexical context
  /// provides the corresponding bindings (ECMA-262 grammar parameters `AllowNewTarget`,
  /// `AllowSuperProperty`, and `AllowSuperCall`).
  ///
  /// This hook exists primarily for embeddings that parse source **snippets** extracted from a
  /// larger program (for example, `vm-js` lazy function parsing). In such cases the snippet itself
  /// may start with an arrow function, which does *not* introduce its own `new.target` or `super`
  /// bindings and instead inherits them from its enclosing scope.
  ///
  /// Callers should only use this to widen the initial grammar context *before* parsing begins.
  pub fn set_initial_meta_property_context(
    &mut self,
    allow_new_target: bool,
    allow_super_property: bool,
    allow_super_call: bool,
  ) {
    self.new_target_allowed = allow_new_target as u32;
    self.super_prop_allowed = allow_super_property as u32;
    self.super_call_allowed = allow_super_call as u32;
  }

  /// Allows parsing top-level `await` expressions in classic scripts (`SourceType::Script`).
  ///
  /// This is an embedder hook used to implement "async classic scripts" (top-level await) without
  /// globally changing the script grammar, which would break valid scripts that use `await` as an
  /// identifier (e.g. `await(0)`).
  ///
  /// Callers should only use this to widen the initial grammar context *before* parsing begins.
  pub fn set_allow_top_level_await_in_script(&mut self, allow: bool) {
    self.allow_top_level_await_in_script = allow;
  }

  /// Allows parsing top-level `yield` expressions in non-generator contexts.
  ///
  /// In standard ECMAScript parsing, `yield` expressions are only permitted inside generator
  /// functions. This hook exists for embeddings that parse **source snippets** extracted from a
  /// larger program (for example, `vm-js` lazy function parsing): when parsing an object/class
  /// member snippet in isolation, a computed property name may contain `yield` that is *actually*
  /// evaluated in an enclosing generator function body.
  ///
  /// Callers should only use this to widen the initial grammar context *before* parsing begins.
  pub fn set_allow_top_level_yield(&mut self, allow: bool) {
    self.allow_top_level_yield = allow;
  }

  /// Execute `f` while treating `arguments` as disallowed in class initialization code.
  ///
  /// This models the early-error behavior for class field initializers and `static {}` blocks:
  /// `arguments` is a syntax error unless it is *shadowed* by an inner non-arrow function (which
  /// provides its own `arguments` binding).
  pub fn with_disallow_arguments_in_class_init<R, F: FnOnce(&mut Self) -> R>(&mut self, f: F) -> R {
    if !self.is_strict_ecmascript() {
      return f(self);
    }
    let prev_disallow = self.disallow_arguments_in_class_init;
    let prev_arguments_allowed = self.arguments_allowed;
    self.disallow_arguments_in_class_init = prev_disallow.saturating_add(1);
    // Class initialization code does not have access to an outer `arguments` binding (even when the
    // class is nested inside a function). Nested non-arrow functions within the initializer/block
    // will increment `arguments_allowed` as usual.
    self.arguments_allowed = 0;
    let out = f(self);
    self.arguments_allowed = prev_arguments_allowed;
    self.disallow_arguments_in_class_init = prev_disallow;
    out
  }

  pub fn validate_arguments_not_disallowed_in_class_init(
    &self,
    loc: Loc,
    raw_name: &str,
  ) -> SyntaxResult<()> {
    if !self.is_strict_ecmascript() {
      return Ok(());
    }
    if self.disallow_arguments_in_class_init == 0 {
      return Ok(());
    }
    if self.arguments_allowed > 0 {
      return Ok(());
    }
    let Some(name) = self.identifier_name_string_value(raw_name) else {
      return Err(loc.error(SyntaxErrorType::ExpectedSyntax("identifier"), None));
    };
    if name.as_ref() != "arguments" {
      return Ok(());
    }
    Err(loc.error(
      SyntaxErrorType::ExpectedSyntax(
        "`arguments` is not allowed in class field initializers or static initialization blocks",
      ),
      None,
    ))
  }
  pub fn options(&self) -> ParseOptions {
    self.options
  }

  pub fn dialect(&self) -> Dialect {
    self.options.dialect
  }

  pub fn source_type(&self) -> SourceType {
    self.options.source_type
  }

  pub fn is_module(&self) -> bool {
    matches!(self.source_type(), SourceType::Module)
  }

  pub fn is_strict_mode(&self) -> bool {
    self.is_module() || self.strict_mode > 0
  }

  pub fn allows_jsx(&self) -> bool {
    self.dialect().allows_jsx()
  }

  pub fn is_typescript(&self) -> bool {
    self.dialect().is_typescript()
  }

  pub fn allows_angle_bracket_type_assertions(&self) -> bool {
    self.dialect().allows_angle_bracket_type_assertions()
  }

  pub fn is_strict_ecmascript(&self) -> bool {
    self.dialect().is_strict_ecmascript()
  }

  pub fn should_recover(&self) -> bool {
    !self.is_strict_ecmascript()
  }

  pub(crate) fn is_strict_mode_reserved_word(name: &str) -> bool {
    matches!(
      name,
      "implements"
        | "interface"
        | "let"
        | "package"
        | "private"
        | "protected"
        | "public"
        | "static"
        | "yield"
    )
  }

  pub(crate) fn is_strict_mode_restricted_binding_identifier(name: &str) -> bool {
    matches!(name, "eval" | "arguments")
  }

  pub(crate) fn is_strict_mode_restricted_assignment_target(name: &str) -> bool {
    // ES strict mode: `eval` and `arguments` are not valid assignment targets.
    Self::is_strict_mode_restricted_binding_identifier(name)
  }

  pub(crate) fn validate_strict_binding_identifier_name(
    &self,
    loc: Loc,
    name: &str,
  ) -> SyntaxResult<()> {
    if !self.is_strict_ecmascript() || !self.is_strict_mode() {
      return Ok(());
    }

    let Some(string_value) = self.identifier_name_string_value(name) else {
      // Identifier names should have already been validated by the lexer; treat this as a syntax
      // error to avoid silently accepting malformed escape sequences.
      return Err(loc.error(SyntaxErrorType::ExpectedSyntax("identifier"), None));
    };

    if Self::is_strict_mode_reserved_word(string_value.as_ref())
      || Self::is_strict_mode_restricted_binding_identifier(string_value.as_ref())
    {
      return Err(loc.error(SyntaxErrorType::ExpectedSyntax("identifier"), None));
    }
    Ok(())
  }

  fn validate_strict_assignment_target_name(&self, loc: Loc, name: &str) -> SyntaxResult<()> {
    if !self.is_strict_ecmascript() || !self.is_strict_mode() {
      return Ok(());
    }

    let Some(string_value) = self.identifier_name_string_value(name) else {
      return Err(loc.error(
        SyntaxErrorType::ExpectedSyntax(
          "assignment to `eval` or `arguments` is not allowed in strict mode",
        ),
        None,
      ));
    };

    if Self::is_strict_mode_restricted_assignment_target(string_value.as_ref()) {
      return Err(loc.error(
        SyntaxErrorType::ExpectedSyntax(
          "assignment to `eval` or `arguments` is not allowed in strict mode",
        ),
        None,
      ));
    }
    Ok(())
  }

  pub(crate) fn validate_strict_assignment_target_expr(
    &self,
    expr: &Node<Expr>,
  ) -> SyntaxResult<()> {
    if !self.is_strict_ecmascript() || !self.is_strict_mode() {
      return Ok(());
    }
    match expr.stx.as_ref() {
      Expr::Id(id) => self.validate_strict_assignment_target_name(expr.loc, &id.stx.name),
      Expr::IdPat(id) => self.validate_strict_assignment_target_name(id.loc, &id.stx.name),
      Expr::ArrPat(arr) => {
        for elem in arr.stx.elements.iter() {
          if let Some(elem) = elem.as_ref() {
            self.validate_strict_assignment_target_pat(&elem.target)?;
          }
        }
        if let Some(rest) = arr.stx.rest.as_ref() {
          self.validate_strict_assignment_target_pat(rest)?;
        }
        Ok(())
      }
      Expr::ObjPat(obj) => {
        for prop in obj.stx.properties.iter() {
          self.validate_strict_assignment_target_pat(&prop.stx.target)?;
        }
        if let Some(rest) = obj.stx.rest.as_ref() {
          self.validate_strict_assignment_target_pat(rest)?;
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  pub(crate) fn with_disallow_arguments_in_class_init<R>(
    &mut self,
    f: impl FnOnce(&mut Self) -> SyntaxResult<R>,
  ) -> SyntaxResult<R> {
    if !self.is_strict_ecmascript() {
      return f(self);
    }
    let prev_arguments_allowed = self.arguments_allowed;
    let prev_disallow_arguments_in_class_init = self.disallow_arguments_in_class_init;
    self.disallow_arguments_in_class_init = prev_disallow_arguments_in_class_init + 1;
    // Class field initializers and static initialization blocks do not inherit an `arguments`
    // binding from an enclosing non-arrow function.
    self.arguments_allowed = 0;
    let res = f(self);
    self.arguments_allowed = prev_arguments_allowed;
    self.disallow_arguments_in_class_init = prev_disallow_arguments_in_class_init;
    res
  }

  pub(crate) fn validate_arguments_not_disallowed_in_class_init(
    &self,
    loc: Loc,
    name: &str,
  ) -> SyntaxResult<()> {
    if !self.is_strict_ecmascript() || self.disallow_arguments_in_class_init == 0 {
      return Ok(());
    }

    // Nested non-arrow functions provide their own `arguments` binding; allow references even
    // within a disallowed class-init context.
    //
    // Note: parameter initializers are parsed before we enter the function body, so we can't rely
    // solely on `with_arguments_bound_in_class_init` to clear the class-init disallow flag.
    if self.arguments_allowed > 0 {
      return Ok(());
    }

    let Some(string_value) = self.identifier_name_string_value(name) else {
      // Identifier names should have already been validated by the lexer; treat this as a syntax
      // error to avoid silently accepting malformed escape sequences.
      return Err(loc.error(SyntaxErrorType::ExpectedSyntax("identifier"), None));
    };

    if string_value.as_ref() == "arguments" {
      return Err(loc.error(
        SyntaxErrorType::ExpectedSyntax(
          "`arguments` is not allowed in class field initializers or static initialization blocks",
        ),
        None,
      ));
    }

    Ok(())
  }

  pub(crate) fn with_arguments_bound_in_class_init<R>(
    &mut self,
    f: impl FnOnce(&mut Self) -> SyntaxResult<R>,
  ) -> SyntaxResult<R> {
    if !self.is_strict_ecmascript() || self.disallow_arguments_in_class_init == 0 {
      return f(self);
    }
    let prev_disallow_arguments_in_class_init = self.disallow_arguments_in_class_init;
    self.disallow_arguments_in_class_init = 0;
    let res = f(self);
    self.disallow_arguments_in_class_init = prev_disallow_arguments_in_class_init;
    res
  }
  /// Validate an *assignable reference* (simple assignment target), as required by update
  /// expressions (`++x`, `x--`, etc.).
  ///
  /// This is stricter than `lhs_expr_to_assign_target_with_recover` because update expressions
  /// do not accept destructuring patterns.
  pub(crate) fn validate_update_target_expr(&self, expr: &Node<Expr>) -> SyntaxResult<()> {
    match expr.stx.as_ref() {
      Expr::Id(_) => Ok(()),
      Expr::Member(member) if !member.stx.optional_chaining => Ok(()),
      Expr::ComputedMember(member) if !member.stx.optional_chaining => Ok(()),
      _ => Err(expr.error(SyntaxErrorType::InvalidAssigmentTarget)),
    }
  }

  pub(crate) fn validate_strict_assignment_target_pat(&self, pat: &Node<Pat>) -> SyntaxResult<()> {
    if !self.is_strict_ecmascript() || !self.is_strict_mode() {
      return Ok(());
    }
    match pat.stx.as_ref() {
      Pat::Id(id) => self.validate_strict_assignment_target_name(id.loc, &id.stx.name),
      Pat::Arr(arr) => {
        for elem in arr.stx.elements.iter() {
          if let Some(elem) = elem.as_ref() {
            self.validate_strict_assignment_target_pat(&elem.target)?;
          }
        }
        if let Some(rest) = arr.stx.rest.as_ref() {
          self.validate_strict_assignment_target_pat(rest)?;
        }
        Ok(())
      }
      Pat::Obj(obj) => {
        for prop in obj.stx.properties.iter() {
          self.validate_strict_assignment_target_pat(&prop.stx.target)?;
        }
        if let Some(rest) = obj.stx.rest.as_ref() {
          self.validate_strict_assignment_target_pat(rest)?;
        }
        Ok(())
      }
      Pat::AssignTarget(expr) => self.validate_strict_assignment_target_expr(expr),
    }
  }

  fn token_continues_expression_after_directive_string(next: &Token) -> bool {
    // Tagged templates allow line terminators between the tag expression and the template.
    if matches!(
      next.typ,
      TT::LiteralTemplatePartString | TT::LiteralTemplatePartStringEnd
    ) {
      return true;
    }
    // Postfix update operators require no line terminator between operand and operator.
    if matches!(next.typ, TT::PlusPlus | TT::HyphenHyphen) {
      return !next.preceded_by_line_terminator;
    }
    MULTARY_OPERATOR_MAPPING
      .get(&next.typ)
      .is_some_and(|op| !matches!(op.arity, Arity::Unary))
  }

  pub(crate) fn has_use_strict_directive_in_prologue(&mut self, end: TT) -> SyntaxResult<bool> {
    let checkpoint = self.checkpoint();
    let mut found = false;
    loop {
      let t = self.peek();
      if t.typ != TT::LiteralString {
        break;
      }
      // Strict mode directives are matched against the *raw* string literal text.
      // For example, `"use\\x20strict"` evaluates to `"use strict"` at runtime,
      // but it does **not** enable strict mode.
      let tok = self.consume_with_mode(LexMode::Standard);
      let is_use_strict = {
        let raw = self.bytes(tok.loc);
        raw == "\"use strict\"" || raw == "'use strict'"
      };
      let next = self.peek();
      if Self::token_continues_expression_after_directive_string(&next) {
        break;
      }
      if is_use_strict {
        found = true;
      }
      if next.typ == TT::Semicolon {
        self.consume();
      }
      if self.peek().typ == end {
        break;
      }
    }
    self.restore_checkpoint(checkpoint);
    Ok(found)
  }

  pub(crate) fn has_use_strict_directive_in_block_body(&mut self) -> SyntaxResult<bool> {
    let checkpoint = self.checkpoint();
    self.require(TT::BraceOpen)?;
    let found = self.has_use_strict_directive_in_prologue(TT::BraceClose)?;
    self.restore_checkpoint(checkpoint);
    Ok(found)
  }

  pub fn source_range(&self) -> Loc {
    self.lexer.source_range()
  }

  pub fn bytes(&self, loc: Loc) -> &str {
    let limit = self.source_range().1;
    if loc.0 > loc.1 {
      return "";
    }
    let start = loc.0.min(limit);
    let end = loc.1.min(limit);
    if start >= end {
      ""
    } else {
      &self.lexer[Loc(start, end)]
    }
  }

  pub fn str(&self, loc: Loc) -> &str {
    self.bytes(loc)
  }

  pub fn string(&self, loc: Loc) -> String {
    self.str(loc).to_string()
  }

  /// Get the ECMAScript *StringValue* of an identifier-like token.
  ///
  /// `parse-js` stores tokens as raw source spans (`Loc`). For `TT::Identifier` tokens that
  /// include Unicode escape sequences (e.g. `l\\u0065t`, `\\u0061sync`, `yi\\u0065ld`), the raw
  /// source slice is **not** the identifier's StringValue. Downstream passes (binding resolution,
  /// early errors keyed off IdentifierName StringValue, etc.) must operate on the decoded form.
  ///
  /// This helper decodes `\\uXXXX` and `\\u{...}` sequences for identifier tokens and returns the
  /// decoded identifier string. Other token types cannot contain escapes by construction and are
  /// returned as their raw source text.
  pub(crate) fn identifier_string_from_token(&self, t: &Token) -> SyntaxResult<String> {
    match t.typ {
      TT::Identifier | TT::PrivateMember => self.decode_identifier_escapes(t.loc, Some(t.typ)),
      _ => Ok(self.string(t.loc)),
    }
  }

  fn decode_identifier_escapes(&self, loc: Loc, actual_token: Option<TT>) -> SyntaxResult<String> {
    let raw = self.str(loc);
    self
      .decode_identifier_escapes_inner(raw)
      .map_err(|_| loc.error(SyntaxErrorType::InvalidCharacterEscape, actual_token))
  }

  fn decode_identifier_escapes_inner(&self, raw: &str) -> Result<String, ()> {
    if !raw.as_bytes().contains(&b'\\') {
      return Ok(raw.to_string());
    }

    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
      if bytes[i] != b'\\' {
        let ch = raw[i..].chars().next().ok_or(())?;
        out.push(ch);
        i += ch.len_utf8();
        continue;
      }

      // UnicodeEscapeSequence in IdentifierName: `\uXXXX` or `\u{...}`
      if i + 1 >= bytes.len() || bytes[i + 1] != b'u' {
        return Err(());
      }

      if i + 2 < bytes.len() && bytes[i + 2] == b'{' {
        let mut j = i + 3;
        while j < bytes.len() && bytes[j] != b'}' {
          j += 1;
        }
        if j >= bytes.len() {
          return Err(());
        }
        let hex = &raw[i + 3..j];
        if hex.is_empty() || !hex.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
          return Err(());
        }
        let value = u32::from_str_radix(hex, 16).map_err(|_| ())?;
        if value > 0x10FFFF {
          return Err(());
        }
        let ch = char::from_u32(value).ok_or(())?;
        out.push(ch);
        i = j + 1;
      } else {
        if i + 6 > bytes.len() {
          return Err(());
        }
        let hex = &raw[i + 2..i + 6];
        if !hex.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
          return Err(());
        }
        let value = u32::from_str_radix(hex, 16).map_err(|_| ())?;
        let ch = char::from_u32(value).ok_or(())?;
        out.push(ch);
        i += 6;
      }
    }

    Ok(out)
  }

  pub fn checkpoint(&self) -> ParserCheckpoint {
    ParserCheckpoint {
      next_tok_i: self.next_tok_i,
    }
  }

  pub fn since_checkpoint(&self, checkpoint: &ParserCheckpoint) -> Loc {
    // `Lexer::next()` tracks the end of the **furthest lexed** token, not the end of the
    // **furthest consumed** token. Many parser routines use lookahead (`peek`, `peek_n`) which
    // lexes tokens into `buf` without advancing `next_tok_i`; using `lexer.next()` would
    // incorrectly widen node spans to include unconsumed terminators (e.g. `)` that ends the
    // surrounding call expression).
    //
    // For accurate node locations (and correct downstream source slicing, e.g. in `vm-js` lazy
    // function parsing), compute the span based on the last token actually consumed since the
    // checkpoint.
    let start = self
      .buf
      .get(checkpoint.next_tok_i)
      .map(|tok| tok.token.loc.0)
      .unwrap_or_else(|| self.lexer.next());
    let end = if self.next_tok_i > checkpoint.next_tok_i {
      self
        .buf
        .get(self.next_tok_i.saturating_sub(1))
        .map(|tok| tok.token.loc.1)
        .unwrap_or(start)
    } else {
      start
    };
    Loc(start, end)
  }

  pub fn restore_checkpoint(&mut self, checkpoint: ParserCheckpoint) {
    self.next_tok_i = checkpoint.next_tok_i;
  }

  fn panic_if_cancelled(&mut self) {
    if let Some(cancel) = self.cancel.as_ref() {
      if cancel.load(AtomicOrdering::Relaxed) {
        std::panic::panic_any(crate::ParseCancelled);
      }
    }
    if self
      .cancel_check
      .as_mut()
      .is_some_and(|cancel_check| cancel_check())
    {
      std::panic::panic_any(crate::ParseCancelled);
    }
  }

  fn reset_to(&mut self, n: usize) {
    self.next_tok_i = n;
    self.buf.truncate(n);
    match self.buf.last() {
      Some(t) => self.lexer.set_next(t.token.loc.1),
      None => self.lexer.set_next(0),
    };
  }

  fn forward<K: FnOnce(&Token) -> bool>(&mut self, mode: LexMode, keep: K) -> (bool, Token) {
    self.panic_if_cancelled();
    if self
      .buf
      .get(self.next_tok_i)
      .is_some_and(|t| t.lex_mode != mode)
    {
      self.reset_to(self.next_tok_i);
    }
    assert!(self.next_tok_i <= self.buf.len());
    if self.buf.len() == self.next_tok_i {
      let dialect = self.dialect();
      let source_type = self.source_type();
      let token = lex_next(&mut self.lexer, mode, dialect, source_type);
      self.buf.push(BufferedToken {
        token,
        lex_mode: mode,
      });
    }
    let t = self.buf[self.next_tok_i].token.clone();
    let k = keep(&t);
    if k {
      self.next_tok_i += 1;
    };
    (k, t)
  }

  pub fn consume_with_mode(&mut self, mode: LexMode) -> Token {
    self.forward(mode, |_| true).1
  }

  pub fn consume(&mut self) -> Token {
    self.consume_with_mode(LexMode::Standard)
  }

  /// Consumes the next token regardless of type, and returns its raw source code as a string.
  pub fn consume_as_string(&mut self) -> String {
    let loc = self.consume().loc;
    self.string(loc)
  }

  pub fn peek_with_mode(&mut self, mode: LexMode) -> Token {
    self.forward(mode, |_| false).1
  }

  pub fn peek(&mut self) -> Token {
    self.peek_with_mode(LexMode::Standard)
  }

  pub fn peek_n_with_mode<const N: usize>(&mut self, modes: [LexMode; N]) -> [Token; N] {
    let cp = self.checkpoint();
    let tokens = modes
      .into_iter()
      .map(|m| self.forward(m, |_| true).1)
      .collect::<Vec<_>>();
    let tokens: [Token; N] = tokens.try_into().unwrap();
    self.restore_checkpoint(cp);
    tokens
  }

  pub fn peek_n<const N: usize>(&mut self) -> [Token; N] {
    let cp = self.checkpoint();
    let tokens = (0..N)
      .map(|_| self.forward(LexMode::Standard, |_| true).1)
      .collect::<Vec<_>>();
    let tokens: [Token; N] = tokens.try_into().unwrap();
    self.restore_checkpoint(cp);
    tokens
  }

  pub fn maybe_consume_with_mode(&mut self, typ: TT, mode: LexMode) -> MaybeToken {
    let (matched, t) = self.forward(mode, |t| t.typ == typ);
    MaybeToken {
      typ,
      matched,
      loc: t.loc,
    }
  }

  pub fn consume_if(&mut self, typ: TT) -> MaybeToken {
    self.maybe_consume_with_mode(typ, LexMode::Standard)
  }

  pub fn consume_if_pred<F: FnOnce(&Token) -> bool>(&mut self, pred: F) -> MaybeToken {
    let (matched, t) = self.forward(LexMode::Standard, pred);
    MaybeToken {
      typ: t.typ,
      matched,
      loc: t.loc,
    }
  }

  pub fn require_with_mode(&mut self, typ: TT, mode: LexMode) -> SyntaxResult<Token> {
    let t = self.consume_with_mode(mode);
    if t.typ != typ {
      Err(t.error(SyntaxErrorType::RequiredTokenNotFound(typ)))
    } else {
      Ok(t)
    }
  }

  pub fn require_predicate<P: FnOnce(TT) -> bool>(
    &mut self,
    pred: P,
    expected: &'static str,
  ) -> SyntaxResult<Token> {
    let t = self.consume_with_mode(LexMode::Standard);
    if !pred(t.typ) {
      Err(t.error(SyntaxErrorType::ExpectedSyntax(expected)))
    } else {
      Ok(t)
    }
  }

  pub fn require(&mut self, typ: TT) -> SyntaxResult<Token> {
    self.require_with_mode(typ, LexMode::Standard)
  }

  /// Require ChevronRight with support for splitting >> and >>> tokens
  /// This is needed for parsing nested generic types like List<List<T>>
  pub fn require_chevron_right(&mut self) -> SyntaxResult<Token> {
    let t = self.peek();
    match t.typ {
      TT::ChevronRight => {
        // Normal case - consume and return
        Ok(self.consume())
      }
      TT::ChevronRightEquals => {
        // Split >= into > and =
        self.consume();
        let equals_token = Token {
          typ: TT::Equals,
          loc: Loc(t.loc.1 - 1, t.loc.1),
          preceded_by_line_terminator: false,
        };
        self.buf.insert(
          self.next_tok_i,
          BufferedToken {
            token: equals_token,
            lex_mode: LexMode::Standard,
          },
        );
        Ok(Token {
          typ: TT::ChevronRight,
          loc: Loc(t.loc.0, t.loc.0 + 1),
          preceded_by_line_terminator: t.preceded_by_line_terminator,
        })
      }
      TT::ChevronRightChevronRight => {
        // Split >> into > and >
        self.consume(); // Consume the >>
                        // Create a replacement > token to push back
        let split_token = Token {
          typ: TT::ChevronRight,
          loc: Loc(t.loc.0 + 1, t.loc.1), // Second > starts one char later
          preceded_by_line_terminator: false,
        };
        // Insert the second > into the buffer at current position
        self.buf.insert(
          self.next_tok_i,
          BufferedToken {
            token: split_token,
            lex_mode: LexMode::Standard,
          },
        );
        // Return a token representing the first >
        Ok(Token {
          typ: TT::ChevronRight,
          loc: Loc(t.loc.0, t.loc.0 + 1),
          preceded_by_line_terminator: t.preceded_by_line_terminator,
        })
      }
      TT::ChevronRightChevronRightEquals => {
        // Split >>= into >, >, =
        self.consume();
        let equals_token = Token {
          typ: TT::Equals,
          loc: Loc(t.loc.1 - 1, t.loc.1),
          preceded_by_line_terminator: false,
        };
        let second = Token {
          typ: TT::ChevronRight,
          loc: Loc(t.loc.0 + 1, t.loc.1 - 1),
          preceded_by_line_terminator: false,
        };
        // Insert in reverse order so the second > is seen before =
        self.buf.insert(
          self.next_tok_i,
          BufferedToken {
            token: equals_token,
            lex_mode: LexMode::Standard,
          },
        );
        self.buf.insert(
          self.next_tok_i,
          BufferedToken {
            token: second,
            lex_mode: LexMode::Standard,
          },
        );
        Ok(Token {
          typ: TT::ChevronRight,
          loc: Loc(t.loc.0, t.loc.0 + 1),
          preceded_by_line_terminator: t.preceded_by_line_terminator,
        })
      }
      TT::ChevronRightChevronRightChevronRight => {
        // Split >>> into > and >>
        self.consume(); // Consume the >>>
                        // Create a >> token to push back
        let split_token = Token {
          typ: TT::ChevronRightChevronRight,
          loc: Loc(t.loc.0 + 1, t.loc.1), // >> starts one char later
          preceded_by_line_terminator: false,
        };
        // Insert the >> into the buffer at current position
        self.buf.insert(
          self.next_tok_i,
          BufferedToken {
            token: split_token,
            lex_mode: LexMode::Standard,
          },
        );
        // Return a token representing the first >
        Ok(Token {
          typ: TT::ChevronRight,
          loc: Loc(t.loc.0, t.loc.0 + 1),
          preceded_by_line_terminator: t.preceded_by_line_terminator,
        })
      }
      TT::ChevronRightChevronRightChevronRightEquals => {
        // Split >>>= into >, >>, =
        self.consume();
        let equals_token = Token {
          typ: TT::Equals,
          loc: Loc(t.loc.1 - 1, t.loc.1),
          preceded_by_line_terminator: false,
        };
        let split_token = Token {
          typ: TT::ChevronRightChevronRight,
          loc: Loc(t.loc.0 + 1, t.loc.1 - 1),
          preceded_by_line_terminator: false,
        };
        self.buf.insert(
          self.next_tok_i,
          BufferedToken {
            token: equals_token,
            lex_mode: LexMode::Standard,
          },
        );
        self.buf.insert(
          self.next_tok_i,
          BufferedToken {
            token: split_token,
            lex_mode: LexMode::Standard,
          },
        );
        Ok(Token {
          typ: TT::ChevronRight,
          loc: Loc(t.loc.0, t.loc.0 + 1),
          preceded_by_line_terminator: t.preceded_by_line_terminator,
        })
      }
      _ => Err(t.error(SyntaxErrorType::RequiredTokenNotFound(TT::ChevronRight))),
    }
  }

  /// Require and consume an identifier, returning its string value
  pub fn require_identifier(&mut self) -> SyntaxResult<String> {
    let t = self.consume();
    if t.typ != TT::Identifier {
      return Err(t.error(SyntaxErrorType::ExpectedSyntax("identifier")));
    }
    self.identifier_string_from_token(&t)
  }

  /// Require an identifier, but allow TypeScript type keywords as identifiers
  /// TypeScript allows unreserved/contextual keywords like "as", "of", etc. as identifiers in some contexts.
  /// For error recovery, it also allows type keywords like "any", "string", "number", etc. as identifiers.
  pub fn require_identifier_or_ts_keyword(&mut self) -> SyntaxResult<String> {
    let t = self.consume();
    // Allow regular identifiers and unreserved/contextual keywords.
    if t.typ == TT::Identifier
      || UNRESERVED_KEYWORDS.contains(&t.typ)
      || (t.typ == TT::KeywordAwait && !self.is_module())
      // NOTE: `yield` is treated as an identifier outside generator contexts for parse recovery.
      || t.typ == TT::KeywordYield
    {
      return self.identifier_string_from_token(&t);
    }
    // Allow TypeScript type keywords as identifiers
    match t.typ {
      TT::KeywordAny
      | TT::KeywordBooleanType
      | TT::KeywordNumberType
      | TT::KeywordStringType
      | TT::KeywordSymbolType
      | TT::KeywordVoid
      | TT::KeywordNever
      | TT::KeywordUndefinedType
      | TT::KeywordUnknown
      | TT::KeywordObjectType
      | TT::KeywordBigIntType => Ok(self.string(t.loc)),
      _ => Err(t.error(SyntaxErrorType::ExpectedSyntax("identifier"))),
    }
  }

  /// Get string value of a template part literal
  pub fn lit_template_part_str_val(&mut self) -> SyntaxResult<String> {
    let t = self.require(TT::LiteralTemplatePartString)?;
    let raw = self.str(t.loc);
    // Template part tokens include the surrounding delimiters, e.g.:
    // - head:   `foo${
    // - middle: bar${
    // - tail:   baz`
    //
    // This helper is used by the type-expression parser for template literal
    // types, where we want the *cooked* string content of the chunk.
    let raw = raw.strip_prefix('`').unwrap_or(raw);
    let Some(body) = raw.strip_suffix("${") else {
      return Err(t.error(SyntaxErrorType::ExpectedSyntax(
        "template literal continuation",
      )));
    };

    // Be permissive: TypeScript allows parsing templates with invalid escape
    // sequences (semantic errors are reported later). If decoding fails, fall
    // back to the raw body so we still produce an AST.
    Ok(
      crate::parse::expr::lit::normalise_literal_string_or_template_inner(body)
        .unwrap_or_else(|| body.to_string()),
    )
  }

  /// Returns the *StringValue* of an `IdentifierName` token, decoding any Unicode escape sequences.
  ///
  /// `parse-js` stores identifier names using their original source spelling (so escapes like
  /// `\u0061` are preserved). ECMAScript grammar and early errors, however, operate on the cooked
  /// StringValue with escape sequences interpreted.
  ///
  /// Callers that need to compare identifier names against keywords or restricted identifiers
  /// (e.g. `"await"`, `"yield"`, `"eval"`) should use this helper.
  pub(crate) fn identifier_name_string_value<'s>(&self, raw: &'s str) -> Option<Cow<'s, str>> {
    if !raw.contains('\\') {
      return Some(Cow::Borrowed(raw));
    }

    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
      if ch != '\\' {
        out.push(ch);
        continue;
      }

      match chars.next() {
        Some('u') => {}
        _ => return None,
      }

      if chars.peek() == Some(&'{') {
        // `\u{...}`
        let _ = chars.next();
        let mut value_u32: u32 = 0;
        let mut saw_digit = false;
        let mut closed = false;
        while let Some(next) = chars.next() {
          if next == '}' {
            closed = true;
            break;
          }
          let digit = next.to_digit(16)?;
          saw_digit = true;
          value_u32 = value_u32.checked_mul(16)?.checked_add(digit)?;
        }
        if !closed || !saw_digit {
          return None;
        }
        let decoded = char::from_u32(value_u32)?;
        out.push(decoded);
        continue;
      }

      // `\uXXXX`
      let mut value_u32: u32 = 0;
      for _ in 0..4 {
        let next = chars.next()?;
        let digit = next.to_digit(16)?;
        value_u32 = (value_u32 << 4) | digit;
      }
      let decoded = char::from_u32(value_u32)?;
      out.push(decoded);
    }

    Some(Cow::Owned(out))
  }
}
