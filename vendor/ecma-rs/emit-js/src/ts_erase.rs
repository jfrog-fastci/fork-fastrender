//! TypeScript → JavaScript type erasure.
//!
//! This module provides a small, deterministic TS "transpile" that only erases
//! type-level syntax so the result is valid ECMAScript.
//!
//! It intentionally does **not** implement TypeScript runtime lowering (enums,
//! namespaces, parameter properties, etc). Those constructs produce diagnostics.

use diagnostics::{Diagnostic, FileId};
use parse_js::ast::class_or_object::{ClassMember, ClassOrObjKey, ClassOrObjVal, ObjMember, ObjMemberType};
use parse_js::ast::expr::lit::{LitArrElem, LitNullExpr, LitTemplatePart};
use parse_js::ast::expr::pat::Pat;
use parse_js::ast::expr::Expr;
use parse_js::ast::func::{Func, FuncBody};
use parse_js::ast::import_export::{ExportNames, ImportNames};
use parse_js::ast::node::Node;
use parse_js::ast::stmt::decl::{ClassDecl, FuncDecl, ParamDecl, VarDecl};
use parse_js::ast::stmt::{
  CatchBlock, EmptyStmt, ForInOfLhs, ForTripleStmtInit, Stmt, SwitchBranch,
};
use parse_js::ast::stx::TopLevel;
use parse_js::loc::Loc;

const ERR_TS_ERASE_UNSUPPORTED: &str = "EMITTS0001";

struct Ctx {
  file: FileId,
  diagnostics: Vec<Diagnostic>,
}

fn push_unsupported(ctx: &mut Ctx, loc: Loc, message: impl Into<String>) {
  let (mut span, note) = loc.to_diagnostics_span_with_note(ctx.file);
  if span.range.is_empty() {
    span.range.end = span.range.start.saturating_add(1);
  }
  let mut diag = Diagnostic::error(ERR_TS_ERASE_UNSUPPORTED, message.into(), span);
  if let Some(note) = note {
    diag = diag.with_note(note);
  }
  ctx.diagnostics.push(diag);
}

fn empty_stmt(loc: Loc) -> Node<Stmt> {
  Node::new(loc, Stmt::Empty(Node::new(loc, EmptyStmt {})))
}

fn dummy_expr() -> Node<Expr> {
  Node::new(
    Loc(0, 0),
    Expr::LitNull(Node::new(Loc(0, 0), LitNullExpr {})),
  )
}

fn take_expr(expr: &mut Node<Expr>) -> Node<Expr> {
  std::mem::replace(expr, dummy_expr())
}

fn erase_top_level(ctx: &mut Ctx, top: &mut Node<TopLevel>) {
  erase_stmt_list(ctx, &mut top.stx.body);
}

fn erase_stmt_list(ctx: &mut Ctx, stmts: &mut Vec<Node<Stmt>>) {
  let mut out = Vec::with_capacity(stmts.len());
  for stmt in stmts.drain(..) {
    if let Some(stmt) = erase_stmt_optional(ctx, stmt) {
      out.push(stmt);
    }
  }
  *stmts = out;
}

fn erase_stmt_required(ctx: &mut Ctx, stmt: Node<Stmt>) -> Node<Stmt> {
  let loc = stmt.loc;
  erase_stmt_optional(ctx, stmt).unwrap_or_else(|| empty_stmt(loc))
}

fn erase_stmt_required_in_place(ctx: &mut Ctx, stmt: &mut Node<Stmt>) {
  let loc = stmt.loc;
  let taken = std::mem::replace(stmt, empty_stmt(loc));
  *stmt = erase_stmt_required(ctx, taken);
}

fn erase_stmt_optional(ctx: &mut Ctx, mut stmt: Node<Stmt>) -> Option<Node<Stmt>> {
  match stmt.stx.as_mut() {
    // --- Type-only statements: drop completely ---
    Stmt::InterfaceDecl(_)
    | Stmt::TypeAliasDecl(_)
    | Stmt::GlobalDecl(_)
    | Stmt::AmbientVarDecl(_)
    | Stmt::AmbientFunctionDecl(_)
    | Stmt::AmbientClassDecl(_)
    | Stmt::ImportTypeDecl(_)
    | Stmt::ExportTypeDecl(_) => return None,

    // --- TS-only runtime constructs (no lowering here) ---
    Stmt::EnumDecl(decl) => {
      if decl.stx.declare {
        return None;
      }
      push_unsupported(
        ctx,
        decl.loc,
        "unsupported TypeScript syntax in JS erasure: enum declarations are not supported",
      );
      return None;
    }
    Stmt::NamespaceDecl(decl) => {
      if decl.stx.declare {
        return None;
      }
      push_unsupported(
        ctx,
        decl.loc,
        "unsupported TypeScript syntax in JS erasure: namespace declarations are not supported",
      );
      return None;
    }
    Stmt::ModuleDecl(decl) => {
      if decl.stx.declare {
        return None;
      }
      push_unsupported(
        ctx,
        decl.loc,
        "unsupported TypeScript syntax in JS erasure: module declarations are not supported",
      );
      return None;
    }
    Stmt::ImportEqualsDecl(decl) => {
      if decl.stx.type_only {
        return None;
      }
      push_unsupported(
        ctx,
        decl.loc,
        "unsupported TypeScript syntax in JS erasure: `import =` declarations are not supported",
      );
      return None;
    }
    Stmt::ExportAssignmentDecl(decl) => {
      push_unsupported(
        ctx,
        decl.loc,
        "unsupported TypeScript syntax in JS erasure: `export =` assignments are not supported",
      );
      return None;
    }
    Stmt::ExportAsNamespaceDecl(decl) => {
      push_unsupported(
        ctx,
        decl.loc,
        "unsupported TypeScript syntax in JS erasure: `export as namespace` is not supported",
      );
      return None;
    }

    // --- Standard JS statements (with TS type metadata stripped) ---
    Stmt::Block(block) => {
      erase_stmt_list(ctx, &mut block.stx.body);
    }
    Stmt::Break(_) | Stmt::Continue(_) | Stmt::Debugger(_) | Stmt::Empty(_) => {}
    Stmt::DoWhile(do_stmt) => {
      erase_expr(ctx, &mut do_stmt.stx.condition);
      erase_stmt_required_in_place(ctx, &mut do_stmt.stx.body);
    }
    Stmt::Expr(expr_stmt) => {
      erase_expr(ctx, &mut expr_stmt.stx.expr);
    }
    Stmt::ForIn(for_in) => {
      erase_for_in_of_lhs(ctx, &mut for_in.stx.lhs);
      erase_expr(ctx, &mut for_in.stx.rhs);
      erase_stmt_list(ctx, &mut for_in.stx.body.stx.body);
    }
    Stmt::ForOf(for_of) => {
      erase_for_in_of_lhs(ctx, &mut for_of.stx.lhs);
      erase_expr(ctx, &mut for_of.stx.rhs);
      erase_stmt_list(ctx, &mut for_of.stx.body.stx.body);
    }
    Stmt::ForTriple(for_triple) => {
      match &mut for_triple.stx.init {
        ForTripleStmtInit::None => {}
        ForTripleStmtInit::Expr(expr) => erase_expr(ctx, expr),
        ForTripleStmtInit::Decl(decl) => erase_var_decl(ctx, decl),
      }
      if let Some(cond) = for_triple.stx.cond.as_mut() {
        erase_expr(ctx, cond);
      }
      if let Some(post) = for_triple.stx.post.as_mut() {
        erase_expr(ctx, post);
      }
      erase_stmt_list(ctx, &mut for_triple.stx.body.stx.body);
    }
    Stmt::If(if_stmt) => {
      erase_expr(ctx, &mut if_stmt.stx.test);
      erase_stmt_required_in_place(ctx, &mut if_stmt.stx.consequent);
      if let Some(mut alt) = if_stmt.stx.alternate.take() {
        alt = erase_stmt_required(ctx, alt);
        if_stmt.stx.alternate = Some(alt);
      }
    }
    Stmt::Import(import_stmt) => {
      if import_stmt.stx.type_only {
        return None;
      }

      if let Some(attrs) = import_stmt.stx.attributes.as_mut() {
        erase_expr(ctx, attrs);
      }

      if let Some(names) = import_stmt.stx.names.as_mut() {
        match names {
          ImportNames::All(_) => {}
          ImportNames::Specific(list) => {
            let original_len = list.len();
            list.retain(|name| !name.stx.type_only);
            // If we removed specifiers and nothing remains, this was a type-only import and
            // must be erased entirely (it must not preserve module side effects).
            if original_len > 0 && list.is_empty() && import_stmt.stx.default.is_none() {
              return None;
            }
          }
        }
      }
    }
    Stmt::Label(label) => {
      erase_stmt_required_in_place(ctx, &mut label.stx.statement);
    }
    Stmt::Return(ret) => {
      if let Some(value) = ret.stx.value.as_mut() {
        erase_expr(ctx, value);
      }
    }
    Stmt::Switch(switch_stmt) => {
      erase_expr(ctx, &mut switch_stmt.stx.test);
      for branch in &mut switch_stmt.stx.branches {
        erase_switch_branch(ctx, branch);
      }
    }
    Stmt::Throw(thr) => {
      erase_expr(ctx, &mut thr.stx.value);
    }
    Stmt::Try(try_stmt) => {
      erase_stmt_list(ctx, &mut try_stmt.stx.wrapped.stx.body);
      if let Some(catch) = try_stmt.stx.catch.as_mut() {
        erase_catch_block(ctx, catch);
      }
      if let Some(finally) = try_stmt.stx.finally.as_mut() {
        erase_stmt_list(ctx, &mut finally.stx.body);
      }
    }
    Stmt::While(while_stmt) => {
      erase_expr(ctx, &mut while_stmt.stx.condition);
      erase_stmt_required_in_place(ctx, &mut while_stmt.stx.body);
    }
    Stmt::With(with_stmt) => {
      erase_expr(ctx, &mut with_stmt.stx.object);
      erase_stmt_required_in_place(ctx, &mut with_stmt.stx.body);
    }
    Stmt::VarDecl(var_decl) => {
      // `const x: T;` only exists in TypeScript (typically as `declare const x: T;`) and is not
      // valid ECMAScript. Treat it as type-only and erase it to preserve JS parsability.
      if var_decl.stx.mode == parse_js::ast::stmt::decl::VarDeclMode::Const
        && var_decl
          .stx
          .declarators
          .iter()
          .any(|decl| decl.initializer.is_none())
      {
        return None;
      }
      erase_var_decl(ctx, var_decl);
    }
    Stmt::FunctionDecl(func_decl) => {
      if func_decl.stx.function.stx.body.is_none() {
        // TS overload signature: erase.
        return None;
      }
      erase_func_decl(ctx, func_decl);
    }
    Stmt::ClassDecl(class_decl) => {
      if class_decl.stx.declare {
        return None;
      }
      erase_class_decl(ctx, class_decl);
    }
    Stmt::ExportDefaultExpr(export) => {
      erase_expr(ctx, &mut export.stx.expression);
    }
    Stmt::ExportList(export) => {
      if export.stx.type_only {
        return None;
      }
      if let Some(attrs) = export.stx.attributes.as_mut() {
        erase_expr(ctx, attrs);
      }
      match &mut export.stx.names {
        ExportNames::All(_) => {}
        ExportNames::Specific(list) => {
          let original_len = list.len();
          list.retain(|name| !name.stx.type_only);
          // Preserve `export {} ...` when the clause was originally empty, but erase when it only
          // contained type-only specifiers.
          if original_len > 0 && list.is_empty() {
            return None;
          }
        }
      }
    }
  }
  Some(stmt)
}

fn erase_for_in_of_lhs(ctx: &mut Ctx, lhs: &mut ForInOfLhs) {
  match lhs {
    ForInOfLhs::Assign(pat) => erase_pat(ctx, pat),
    ForInOfLhs::Decl((_mode, pat)) => erase_pat(ctx, &mut pat.stx.pat),
  }
}

fn erase_switch_branch(ctx: &mut Ctx, branch: &mut Node<SwitchBranch>) {
  if let Some(case) = branch.stx.case.as_mut() {
    erase_expr(ctx, case);
  }
  erase_stmt_list(ctx, &mut branch.stx.body);
}

fn erase_catch_block(ctx: &mut Ctx, catch: &mut Node<CatchBlock>) {
  // Catch type annotations are TS-only.
  catch.stx.type_annotation = None;
  if let Some(param) = catch.stx.parameter.as_mut() {
    erase_pat(ctx, &mut param.stx.pat);
  }
  erase_stmt_list(ctx, &mut catch.stx.body);
}

fn erase_var_decl(ctx: &mut Ctx, decl: &mut Node<VarDecl>) {
  for declarator in &mut decl.stx.declarators {
    // TS definite assignment assertions and type annotations are erased.
    declarator.definite_assignment = false;
    declarator.type_annotation = None;
    erase_pat(ctx, &mut declarator.pattern.stx.pat);
    if let Some(init) = declarator.initializer.as_mut() {
      erase_expr(ctx, init);
    }
  }
}

fn erase_func_decl(ctx: &mut Ctx, decl: &mut Node<FuncDecl>) {
  erase_func(ctx, &mut decl.stx.function);
}

fn erase_class_decl(ctx: &mut Ctx, decl: &mut Node<ClassDecl>) {
  // `abstract` is TS-only; it erases to a normal class.
  decl.stx.abstract_ = false;
  decl.stx.type_parameters = None;
  decl.stx.implements.clear();
  for deco in &mut decl.stx.decorators {
    erase_expr(ctx, &mut deco.stx.expression);
  }
  if let Some(extends) = decl.stx.extends.as_mut() {
    erase_expr(ctx, extends);
  }
  erase_class_members(ctx, &mut decl.stx.members);
}

fn erase_class_expr(ctx: &mut Ctx, class: &mut Node<parse_js::ast::expr::ClassExpr>) {
  class.stx.type_parameters = None;
  class.stx.implements.clear();
  for deco in &mut class.stx.decorators {
    erase_expr(ctx, &mut deco.stx.expression);
  }
  if let Some(extends) = class.stx.extends.as_mut() {
    erase_expr(ctx, extends);
  }
  erase_class_members(ctx, &mut class.stx.members);
}

fn erase_class_members(ctx: &mut Ctx, members: &mut Vec<Node<ClassMember>>) {
  let mut out = Vec::with_capacity(members.len());
  for mut member in members.drain(..) {
    // Class index signatures are TS-only and have no runtime meaning.
    if matches!(member.stx.val, ClassOrObjVal::IndexSignature(_)) {
      continue;
    }

    // TS-only modifiers.
    member.stx.abstract_ = false;
    member.stx.readonly = false;
    member.stx.override_ = false;
    member.stx.optional = false;
    member.stx.definite_assignment = false;
    member.stx.accessibility = None;
    member.stx.type_annotation = None;

    for deco in &mut member.stx.decorators {
      erase_expr(ctx, &mut deco.stx.expression);
    }

    if let ClassOrObjKey::Computed(expr) = &mut member.stx.key {
      erase_expr(ctx, expr);
    }

    match &mut member.stx.val {
      ClassOrObjVal::Getter(get) => {
        erase_func(ctx, &mut get.stx.func);
        if get.stx.func.stx.body.is_none() {
          continue;
        }
      }
      ClassOrObjVal::Setter(set) => {
        erase_func(ctx, &mut set.stx.func);
        if set.stx.func.stx.body.is_none() {
          continue;
        }
      }
      ClassOrObjVal::Method(method) => {
        erase_func(ctx, &mut method.stx.func);
        if method.stx.func.stx.body.is_none() {
          continue;
        }
      }
      ClassOrObjVal::Prop(Some(expr)) => erase_expr(ctx, expr),
      ClassOrObjVal::Prop(None) => {}
      ClassOrObjVal::IndexSignature(_) => unreachable!("handled above"),
      ClassOrObjVal::StaticBlock(block) => {
        erase_stmt_list(ctx, &mut block.stx.body);
      }
    }

    out.push(member);
  }
  *members = out;
}

fn erase_obj_member(ctx: &mut Ctx, member: &mut Node<ObjMember>) {
  match &mut member.stx.typ {
    ObjMemberType::Valued { key, val } => {
      if let ClassOrObjKey::Computed(expr) = key {
        erase_expr(ctx, expr);
      }

      match val {
        ClassOrObjVal::Getter(get) => {
          erase_func(ctx, &mut get.stx.func);
          if get.stx.func.stx.body.is_none() {
            push_unsupported(
              ctx,
              get.loc,
              "unsupported TypeScript syntax in JS erasure: object literal getter signature without body",
            );
          }
        }
        ClassOrObjVal::Setter(set) => {
          erase_func(ctx, &mut set.stx.func);
          if set.stx.func.stx.body.is_none() {
            push_unsupported(
              ctx,
              set.loc,
              "unsupported TypeScript syntax in JS erasure: object literal setter signature without body",
            );
          }
        }
        ClassOrObjVal::Method(method) => {
          erase_func(ctx, &mut method.stx.func);
          if method.stx.func.stx.body.is_none() {
            push_unsupported(
              ctx,
              method.loc,
              "unsupported TypeScript syntax in JS erasure: object literal method signature without body",
            );
          }
        }
        ClassOrObjVal::Prop(Some(expr)) => erase_expr(ctx, expr),
        ClassOrObjVal::Prop(None) => {}
        ClassOrObjVal::IndexSignature(_) | ClassOrObjVal::StaticBlock(_) => {
          push_unsupported(
            ctx,
            member.loc,
            "unsupported TypeScript syntax in JS erasure: object literal member kind is not supported",
          );
        }
      }
    }
    ObjMemberType::Rest { val } => erase_expr(ctx, val),
    ObjMemberType::Shorthand { .. } => {}
  }
}

fn erase_func(ctx: &mut Ctx, func: &mut Node<Func>) {
  // Erase generics and return type annotations.
  func.stx.type_parameters = None;
  func.stx.return_type = None;

  for param in &mut func.stx.parameters {
    erase_param_decl(ctx, param);
  }

  match func.stx.body.as_mut() {
    Some(FuncBody::Block(stmts)) => erase_stmt_list(ctx, stmts),
    Some(FuncBody::Expression(expr)) => erase_expr(ctx, expr),
    None => {}
  }
}

fn erase_param_decl(ctx: &mut Ctx, param: &mut Node<ParamDecl>) {
  if !param.stx.decorators.is_empty() {
    push_unsupported(
      ctx,
      param.loc,
      "unsupported TypeScript syntax in JS erasure: parameter decorators are not supported",
    );
    param.stx.decorators.clear();
  }
  if param.stx.accessibility.is_some() || param.stx.readonly {
    push_unsupported(
      ctx,
      param.loc,
      "unsupported TypeScript syntax in JS erasure: parameter properties are not supported",
    );
    param.stx.accessibility = None;
    param.stx.readonly = false;
  }

  param.stx.optional = false;
  param.stx.type_annotation = None;

  erase_pat(ctx, &mut param.stx.pattern.stx.pat);
  if let Some(default) = param.stx.default_value.as_mut() {
    erase_expr(ctx, default);
  }
}

fn erase_pat(ctx: &mut Ctx, pat: &mut Node<Pat>) {
  match pat.stx.as_mut() {
    Pat::Arr(arr) => {
      for elem in arr.stx.elements.iter_mut().flatten() {
        erase_pat(ctx, &mut elem.target);
        if let Some(default) = elem.default_value.as_mut() {
          erase_expr(ctx, default);
        }
      }
      if let Some(rest) = arr.stx.rest.as_mut() {
        erase_pat(ctx, rest);
      }
    }
    Pat::Obj(obj) => {
      for prop in &mut obj.stx.properties {
        if let ClassOrObjKey::Computed(expr) = &mut prop.stx.key {
          erase_expr(ctx, expr);
        }
        erase_pat(ctx, &mut prop.stx.target);
        if let Some(default) = prop.stx.default_value.as_mut() {
          erase_expr(ctx, default);
        }
      }
      if let Some(rest) = obj.stx.rest.as_mut() {
        erase_pat(ctx, rest);
      }
    }
    Pat::AssignTarget(expr) => erase_expr(ctx, expr),
    Pat::Id(_) => {}
  }
}

fn erase_expr(ctx: &mut Ctx, expr: &mut Node<Expr>) {
  match expr.stx.as_mut() {
    Expr::ArrowFunc(arrow) => erase_func(ctx, &mut arrow.stx.func),
    Expr::Binary(binary) => {
      erase_expr(ctx, &mut binary.stx.left);
      erase_expr(ctx, &mut binary.stx.right);
    }
    Expr::Call(call) => {
      erase_expr(ctx, &mut call.stx.callee);
      for arg in &mut call.stx.arguments {
        erase_expr(ctx, &mut arg.stx.value);
      }
    }
    Expr::Class(class) => erase_class_expr(ctx, class),
    Expr::ComputedMember(member) => {
      erase_expr(ctx, &mut member.stx.object);
      erase_expr(ctx, &mut member.stx.member);
    }
    Expr::Cond(cond) => {
      erase_expr(ctx, &mut cond.stx.test);
      erase_expr(ctx, &mut cond.stx.consequent);
      erase_expr(ctx, &mut cond.stx.alternate);
    }
    Expr::Func(func) => {
      erase_func(ctx, &mut func.stx.func);
      if func.stx.func.stx.body.is_none() {
        push_unsupported(
          ctx,
          func.loc,
          "unsupported TypeScript syntax in JS erasure: function expressions without bodies are not supported",
        );
      }
    }
    Expr::Import(import_expr) => {
      erase_expr(ctx, &mut import_expr.stx.module);
      if let Some(attrs) = import_expr.stx.attributes.as_mut() {
        erase_expr(ctx, attrs);
      }
    }
    Expr::Member(member) => {
      erase_expr(ctx, &mut member.stx.left);
    }
    Expr::TaggedTemplate(tagged) => {
      erase_expr(ctx, &mut tagged.stx.function);
      for part in &mut tagged.stx.parts {
        if let LitTemplatePart::Substitution(expr) = part {
          erase_expr(ctx, expr);
        }
      }
    }
    Expr::Unary(unary) => erase_expr(ctx, &mut unary.stx.argument),
    Expr::UnaryPostfix(unary) => erase_expr(ctx, &mut unary.stx.argument),
    Expr::LitArr(arr) => {
      for elem in &mut arr.stx.elements {
        match elem {
          LitArrElem::Single(expr) | LitArrElem::Rest(expr) => erase_expr(ctx, expr),
          LitArrElem::Empty => {}
        }
      }
    }
    Expr::LitObj(obj) => {
      for member in &mut obj.stx.members {
        erase_obj_member(ctx, member);
      }
    }
    Expr::LitTemplate(tpl) => {
      for part in &mut tpl.stx.parts {
        if let LitTemplatePart::Substitution(expr) = part {
          erase_expr(ctx, expr);
        }
      }
    }
    Expr::ArrPat(arr) => {
      for elem in arr.stx.elements.iter_mut() {
        if let Some(elem) = elem {
          erase_pat(ctx, &mut elem.target);
          if let Some(default) = elem.default_value.as_mut() {
            erase_expr(ctx, default);
          }
        }
      }
      if let Some(rest) = arr.stx.rest.as_mut() {
        erase_pat(ctx, rest);
      }
    }
    Expr::ObjPat(obj) => {
      for prop in &mut obj.stx.properties {
        if let ClassOrObjKey::Computed(expr) = &mut prop.stx.key {
          erase_expr(ctx, expr);
        }
        erase_pat(ctx, &mut prop.stx.target);
        if let Some(default) = prop.stx.default_value.as_mut() {
          erase_expr(ctx, default);
        }
      }
      if let Some(rest) = obj.stx.rest.as_mut() {
        erase_pat(ctx, rest);
      }
    }
    Expr::Instantiation(inst) => {
      // TypeScript instantiation expressions erase to their underlying expression.
      erase_expr(ctx, inst.stx.expression.as_mut());
    }
    Expr::TypeAssertion(assert) => {
      // Erase both `as Type` and `as const`.
      erase_expr(ctx, assert.stx.expression.as_mut());
    }
    Expr::NonNullAssertion(assert) => {
      erase_expr(ctx, assert.stx.expression.as_mut());
    }
    Expr::SatisfiesExpr(assert) => {
      erase_expr(ctx, assert.stx.expression.as_mut());
    }

    // JSX is not valid ECMAScript; reject it in the erasure pipeline.
    Expr::JsxElem(_) | Expr::JsxExprContainer(_) | Expr::JsxMember(_) | Expr::JsxName(_)
    | Expr::JsxSpreadAttr(_) | Expr::JsxText(_) => {
      push_unsupported(
        ctx,
        expr.loc,
        "unsupported TypeScript syntax in JS erasure: JSX is not supported",
      );
    }

    // Leaf nodes: nothing to erase.
    Expr::Id(_)
    | Expr::ImportMeta(_)
    | Expr::NewTarget(_)
    | Expr::Super(_)
    | Expr::This(_)
    | Expr::LitBigInt(_)
    | Expr::LitBool(_)
    | Expr::LitNull(_)
    | Expr::LitNum(_)
    | Expr::LitRegex(_)
    | Expr::LitStr(_)
    | Expr::IdPat(_) => {}
  }

  // After traversing children, strip TS-only expression wrappers by replacing
  // the node with its underlying expression.
  loop {
    match expr.stx.as_mut() {
      Expr::Instantiation(inst) => {
        let inner = take_expr(inst.stx.expression.as_mut());
        *expr = inner;
      }
      Expr::TypeAssertion(assert) => {
        let inner = take_expr(assert.stx.expression.as_mut());
        *expr = inner;
      }
      Expr::NonNullAssertion(assert) => {
        let inner = take_expr(assert.stx.expression.as_mut());
        *expr = inner;
      }
      Expr::SatisfiesExpr(assert) => {
        let inner = take_expr(assert.stx.expression.as_mut());
        *expr = inner;
      }
      _ => break,
    }
  }
}

/// Emit a TypeScript `parse-js` AST as JavaScript by erasing type-only syntax.
///
/// The provided AST is **mutated in place** (type syntax removed and type-only
/// statements dropped) before emission.
///
/// Supported erasures:
/// - Type-only statements are removed (`interface`, `type`, `import type`,
///   `export type`, ambient `declare` statements, and overload signatures).
/// - Type-only expression wrappers are removed:
///   - `expr<T>` instantiations
///   - `expr as T` / `expr as const`
///   - `expr!` non-null assertions
///   - `expr satisfies T`
///
/// Unsupported TS runtime constructs (currently `enum`, `namespace`, `module`,
/// `import =`, and `export =`) produce diagnostics.
pub fn emit_ecma_from_ts_top_level(
  file: FileId,
  top: &mut Node<TopLevel>,
  options: crate::EmitOptions,
) -> Result<String, Vec<Diagnostic>> {
  let mut ctx = Ctx {
    file,
    diagnostics: Vec::new(),
  };
  erase_top_level(&mut ctx, top);
  if !ctx.diagnostics.is_empty() {
    return Err(ctx.diagnostics);
  }

  match crate::emit_top_level_diagnostic(file, top, options) {
    Ok(out) => Ok(out),
    Err(diag) => Err(vec![diag]),
  }
}
