use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::panic_any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bumpalo::Bump;
use diagnostics::{Diagnostic, FileId, Span, TextRange};
use hir_js::hir::SwitchCase;
use hir_js::{
  ArrayElement, AssignOp, BinaryOp, Body, BodyKind, ExprId, ExprKind, ForHead, ForInit, MemberExpr,
  NameId, NameInterner, ObjectKey, ObjectLiteral, ObjectProperty, PatId, PatKind, StmtId, StmtKind,
  UnaryOp, VarDecl as HirVarDecl,
};
use num_bigint::BigInt;
use ordered_float::OrderedFloat;
use parse_js::ast::class_or_object::{
  ClassMember, ClassOrObjKey, ClassOrObjVal, ClassStaticBlock, ObjMemberType,
};
use parse_js::ast::expr::jsx::{JsxAttr, JsxAttrVal, JsxElem, JsxElemChild, JsxElemName, JsxText};
use parse_js::ast::expr::pat::{ArrPat, ObjPat, Pat as AstPat};
use parse_js::ast::expr::Expr as AstExpr;
use parse_js::ast::func::{Func, FuncBody};
use parse_js::ast::node::Node;
use parse_js::ast::stmt::decl::{ParamDecl, VarDecl, VarDeclMode};
use parse_js::ast::stmt::Stmt;
use parse_js::ast::ts_stmt::{NamespaceBody, NamespaceDecl};
use parse_js::loc::Loc;
use parse_js::operator::OperatorName;
use semantic_js::ts::SymbolId;
use types_ts_interned::{
  Accessibility, EvaluatorCaches, ExpandedType, NameId as TsNameId, ObjectType, Param as SigParam,
  PredicateParam, PropData, PropKey, RelateCtx, Shape, Signature, SignatureId, TypeDisplay,
  TypeEvaluator, TypeExpander, TypeId, TypeKind, TypeParamDecl, TypeParamId, TypeParamVariance,
  TypeStore,
};

use super::cfg::{BlockId, BlockKind, ControlFlowGraph};
use super::flow::{BindingKey, Env, FlowKey, InitState, PathSegment};
use super::flow_bindings::{FlowBindingId, FlowBindings};
use super::flow_narrow::{
  and_facts, narrow_by_assignability, narrow_by_discriminant_path, narrow_by_in_check,
  narrow_by_instanceof_rhs, narrow_by_literal, narrow_by_nullish_equality, narrow_by_typeof,
  narrow_non_nullish, nullish_coalesce_facts, or_facts, truthy_falsy_types, Facts, LiteralValue,
};

use super::caches::BodyCaches;
use super::expr::{resolve_call, resolve_construct};
use super::infer::{infer_type_arguments_for_call, infer_type_arguments_from_contextual_signature};
use super::instantiate::{InstantiationCache, Substituter};
use super::overload::{
  callable_signatures, callable_signatures_with_expander, construct_signatures_with_expander,
  expected_arg_type_at, signature_allows_arg_count, signature_contains_literal_types, CallArgType,
};
use super::type_expr::{TypeLowerer, TypeResolver};
use crate::lib_support::{JsxMode, ScriptTarget};
pub use crate::BodyCheckResult;
use crate::{codes, BodyId, DefId};

#[derive(Default, Clone)]
struct Scope {
  bindings: HashMap<String, Binding>,
}

#[derive(Clone)]
struct Binding {
  ty: TypeId,
  type_params: Vec<TypeParamDecl>,
}

/// Simple resolver that maps single-segment type names to known definitions.
#[derive(Clone)]
pub struct BindingTypeResolver {
  map: HashMap<String, DefId>,
}

impl BindingTypeResolver {
  pub fn new(map: HashMap<String, DefId>) -> Self {
    Self { map }
  }
}

impl TypeResolver for BindingTypeResolver {
  fn resolve_type_name(&self, path: &[String]) -> Option<DefId> {
    match path {
      [name] => self.map.get(name).copied(),
      _ => None,
    }
  }
}

#[derive(Clone)]
struct BodyLocalTypeResolver {
  locals_type: HashMap<String, DefId>,
  locals_value: HashMap<String, DefId>,
  inner: Option<Arc<dyn TypeResolver>>,
}

impl TypeResolver for BodyLocalTypeResolver {
  fn resolve_type_name(&self, path: &[String]) -> Option<DefId> {
    match path {
      [name] => self
        .locals_type
        .get(name)
        .copied()
        .or_else(|| self.inner.as_ref()?.resolve_type_name(path)),
      _ => self.inner.as_ref()?.resolve_type_name(path),
    }
  }

  fn resolve_typeof(&self, path: &[String]) -> Option<DefId> {
    match path {
      [name] => self
        .locals_value
        .get(name)
        .copied()
        .or_else(|| self.inner.as_ref()?.resolve_typeof(path)),
      _ => self.inner.as_ref()?.resolve_typeof(path),
    }
  }

  fn resolve_import_type(&self, module: &str, qualifier: Option<&[String]>) -> Option<DefId> {
    self
      .inner
      .as_ref()
      .and_then(|inner| inner.resolve_import_type(module, qualifier))
  }

  fn resolve_import_typeof(&self, module: &str, qualifier: Option<&[String]>) -> Option<DefId> {
    self
      .inner
      .as_ref()
      .and_then(|inner| inner.resolve_import_typeof(module, qualifier))
  }
}

pub struct AstIndex {
  ast: Arc<Node<parse_js::ast::stx::TopLevel>>,
  stmts: HashMap<TextRange, *const Node<Stmt>>,
  exprs: HashMap<TextRange, *const Node<AstExpr>>,
  pats: HashMap<TextRange, *const Node<AstPat>>,
  params: HashMap<TextRange, *const Node<ParamDecl>>,
  vars: HashMap<TextRange, VarInfo>,
  class_field_initializers: HashMap<TextRange, ClassFieldInitializerInfo>,
  class_member_functions: HashMap<TextRange, ClassMemberFunctionInfo>,
  classes: Vec<ClassInfo>,
  classes_by_name: HashMap<String, usize>,
  functions: Vec<FunctionInfo>,
  class_static_blocks: Vec<ClassStaticBlockInfo>,
  class_field_param_props: HashMap<TextRange, Vec<String>>,
}

// Safety: `AstIndex` stores immutable pointers into an `Arc`-owned AST.
unsafe impl Send for AstIndex {}
unsafe impl Sync for AstIndex {}

#[derive(Clone, Copy)]
struct VarInfo {
  initializer: Option<*const Node<AstExpr>>,
  type_annotation: Option<*const Node<parse_js::ast::type_expr::TypeExpr>>,
  mode: VarDeclMode,
}

#[derive(Clone, Debug)]
struct ClassInfo {
  name: Option<String>,
  extends: Option<String>,
  type_params: Vec<String>,
  instance_fields: Vec<ClassFieldDecl>,
  static_fields: Vec<ClassFieldDecl>,
  instance_param_props: Vec<String>,
  instance_param_props_private: HashSet<String>,
  instance_member_names: HashSet<String>,
}

#[derive(Clone, Debug)]
struct ClassFieldDecl {
  name: String,
  member_index: usize,
  optional: bool,
  has_initializer: bool,
  is_private: bool,
  key_range: TextRange,
}

#[derive(Clone, Copy)]
struct ClassFieldInitializerInfo {
  class_index: usize,
  member_index: usize,
  is_static: bool,
}

#[derive(Clone, Copy, Debug)]
enum MemberKind {
  Constructor,
  Method,
  Getter,
  Setter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemberAccessReceiver {
  This,
  Super,
  Other,
}

#[derive(Clone, Copy, Debug)]
struct ClassMemberContext {
  class_index: usize,
  is_static: bool,
  kind: MemberKind,
}

#[derive(Clone, Copy)]
struct ClassMemberFunctionInfo {
  class_index: usize,
  is_static: bool,
  is_constructor: bool,
}

#[derive(Clone, Copy)]
struct FunctionInfo {
  func_span: TextRange,
  func: *const Node<Func>,
  is_arrow: bool,
  class_member: Option<ClassMemberContext>,
}

#[derive(Clone, Copy)]
struct ClassStaticBlockInfo {
  span: TextRange,
  block: *const Node<ClassStaticBlock>,
  class_index: usize,
  member_index: usize,
}

impl AstIndex {
  pub fn new(
    ast: Arc<Node<parse_js::ast::stx::TopLevel>>,
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) -> Self {
    let mut index = AstIndex {
      ast,
      stmts: HashMap::new(),
      exprs: HashMap::new(),
      pats: HashMap::new(),
      params: HashMap::new(),
      vars: HashMap::new(),
      class_field_initializers: HashMap::new(),
      class_member_functions: HashMap::new(),
      classes: Vec::new(),
      classes_by_name: HashMap::new(),
      functions: Vec::new(),
      class_static_blocks: Vec::new(),
      class_field_param_props: HashMap::new(),
    };
    index.index_top_level(file, cancelled);
    index
  }

  pub(crate) fn ast(&self) -> &Node<parse_js::ast::stx::TopLevel> {
    self.ast.as_ref()
  }

  fn check_cancelled(cancelled: Option<&Arc<AtomicBool>>) {
    if let Some(flag) = cancelled {
      if flag.load(Ordering::Relaxed) {
        panic_any(crate::FatalError::Cancelled);
      }
    }
  }

  fn index_top_level(&mut self, file: FileId, cancelled: Option<&Arc<AtomicBool>>) {
    let ast = Arc::clone(&self.ast);
    for (idx, stmt) in ast.stx.body.iter().enumerate() {
      if idx % 1024 == 0 {
        Self::check_cancelled(cancelled);
      }
      self.index_stmt(stmt, file, cancelled);
    }
  }

  fn index_stmt(&mut self, stmt: &Node<Stmt>, file: FileId, cancelled: Option<&Arc<AtomicBool>>) {
    let span = loc_to_range(file, stmt.loc);
    self.stmts.insert(span, stmt as *const _);
    match stmt.stx.as_ref() {
      Stmt::Expr(expr_stmt) => {
        self.index_expr(&expr_stmt.stx.expr, file, cancelled);
      }
      Stmt::Return(ret) => {
        if let Some(value) = &ret.stx.value {
          self.index_expr(value, file, cancelled);
        }
      }
      Stmt::Block(block) => self.index_stmt_list(&block.stx.body, file, cancelled),
      Stmt::If(if_stmt) => {
        self.index_expr(&if_stmt.stx.test, file, cancelled);
        self.index_stmt(&if_stmt.stx.consequent, file, cancelled);
        if let Some(alt) = &if_stmt.stx.alternate {
          self.index_stmt(alt, file, cancelled);
        }
      }
      Stmt::While(while_stmt) => {
        self.index_expr(&while_stmt.stx.condition, file, cancelled);
        self.index_stmt(&while_stmt.stx.body, file, cancelled);
      }
      Stmt::DoWhile(do_while) => {
        self.index_expr(&do_while.stx.condition, file, cancelled);
        self.index_stmt(&do_while.stx.body, file, cancelled);
      }
      Stmt::ForTriple(for_stmt) => {
        use parse_js::ast::stmt::ForTripleStmtInit;
        match &for_stmt.stx.init {
          ForTripleStmtInit::Expr(expr) => self.index_expr(expr, file, cancelled),
          ForTripleStmtInit::Decl(decl) => self.index_var_decl(decl, file, cancelled),
          ForTripleStmtInit::None => {}
        }
        if let Some(cond) = &for_stmt.stx.cond {
          self.index_expr(cond, file, cancelled);
        }
        if let Some(post) = &for_stmt.stx.post {
          self.index_expr(post, file, cancelled);
        }
        self.index_stmt_list(&for_stmt.stx.body.stx.body, file, cancelled);
      }
      Stmt::ForIn(for_in) => {
        use parse_js::ast::stmt::ForInOfLhs;
        match &for_in.stx.lhs {
          ForInOfLhs::Assign(pat) => self.index_pat(pat, file, cancelled),
          ForInOfLhs::Decl((_, pat_decl)) => self.index_pat(&pat_decl.stx.pat, file, cancelled),
        }
        self.index_expr(&for_in.stx.rhs, file, cancelled);
        self.index_stmt_list(&for_in.stx.body.stx.body, file, cancelled);
      }
      Stmt::ForOf(for_of) => {
        use parse_js::ast::stmt::ForInOfLhs;
        match &for_of.stx.lhs {
          ForInOfLhs::Assign(pat) => self.index_pat(pat, file, cancelled),
          ForInOfLhs::Decl((_, pat_decl)) => self.index_pat(&pat_decl.stx.pat, file, cancelled),
        }
        self.index_expr(&for_of.stx.rhs, file, cancelled);
        self.index_stmt_list(&for_of.stx.body.stx.body, file, cancelled);
      }
      Stmt::Switch(sw) => {
        self.index_expr(&sw.stx.test, file, cancelled);
        for branch in sw.stx.branches.iter() {
          if let Some(case) = &branch.stx.case {
            self.index_expr(case, file, cancelled);
          }
          for stmt in branch.stx.body.iter() {
            self.index_stmt(stmt, file, cancelled);
          }
        }
      }
      Stmt::Try(tr) => {
        self.index_stmt_list(&tr.stx.wrapped.stx.body, file, cancelled);
        if let Some(catch) = &tr.stx.catch {
          if let Some(param) = &catch.stx.parameter {
            self.index_pat(&param.stx.pat, file, cancelled);
          }
          self.index_stmt_list(&catch.stx.body, file, cancelled);
        }
        if let Some(finally) = &tr.stx.finally {
          self.index_stmt_list(&finally.stx.body, file, cancelled);
        }
      }
      Stmt::Throw(th) => self.index_expr(&th.stx.value, file, cancelled),
      Stmt::Label(label) => self.index_stmt(&label.stx.statement, file, cancelled),
      Stmt::With(w) => {
        self.index_expr(&w.stx.object, file, cancelled);
        self.index_stmt(&w.stx.body, file, cancelled);
      }
      Stmt::VarDecl(decl) => {
        self.index_var_decl(decl, file, cancelled);
      }
      Stmt::FunctionDecl(func) => {
        self.index_function(&func.stx.function, file, cancelled);
      }
      Stmt::ClassDecl(class_decl) => {
        let class_name = class_decl
          .stx
          .name
          .as_ref()
          .map(|name| name.stx.name.clone());
        let type_params: Vec<String> = class_decl
          .stx
          .type_parameters
          .as_ref()
          .map(|params| params.iter().map(|param| param.stx.name.clone()).collect())
          .unwrap_or_default();
        let extends_name =
          class_decl
            .stx
            .extends
            .as_ref()
            .and_then(|expr| match expr.stx.as_ref() {
              AstExpr::Id(id) => Some(id.stx.name.clone()),
              AstExpr::Instantiation(inst) => match inst.stx.expression.stx.as_ref() {
                AstExpr::Id(id) => Some(id.stx.name.clone()),
                _ => None,
              },
               _ => None,
             });
        let class_index = self.register_class(class_name, extends_name, type_params);
        for decorator in class_decl.stx.decorators.iter() {
          self.index_expr(&decorator.stx.expression, file, cancelled);
        }
        if let Some(extends) = class_decl.stx.extends.as_ref() {
          self.index_expr(extends, file, cancelled);
        }
        for implements in class_decl.stx.implements.iter() {
          self.index_expr(implements, file, cancelled);
        }
        self.index_class_members_in_class(&class_decl.stx.members, class_index, file, cancelled);
      }
      Stmt::NamespaceDecl(ns) => self.index_namespace(ns, file, cancelled),
      Stmt::ModuleDecl(module) => {
        if let Some(body) = &module.stx.body {
          self.index_stmt_list(body, file, cancelled);
        }
      }
      Stmt::GlobalDecl(global) => {
        self.index_stmt_list(&global.stx.body, file, cancelled);
      }
      _ => {}
    }
  }

  fn index_stmt_list(
    &mut self,
    stmts: &[Node<Stmt>],
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) {
    for (idx, stmt) in stmts.iter().enumerate() {
      if idx % 1024 == 0 {
        Self::check_cancelled(cancelled);
      }
      self.index_stmt(stmt, file, cancelled);
    }
  }

  fn index_namespace(
    &mut self,
    ns: &Node<NamespaceDecl>,
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) {
    match &ns.stx.body {
      NamespaceBody::Block(stmts) => self.index_stmt_list(stmts, file, cancelled),
      NamespaceBody::Namespace(inner) => self.index_namespace(inner, file, cancelled),
    }
  }

  fn index_var_decl(
    &mut self,
    decl: &Node<VarDecl>,
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) {
    for declarator in decl.stx.declarators.iter() {
      let pat_span = loc_to_range(file, declarator.pattern.loc);
      self
        .pats
        .insert(pat_span, &declarator.pattern.stx.pat as *const _);
      self.vars.insert(
        pat_span,
        VarInfo {
          initializer: declarator.initializer.as_ref().map(|n| n as *const _),
          type_annotation: declarator.type_annotation.as_ref().map(|n| n as *const _),
          mode: decl.stx.mode,
        },
      );
      self.index_pat(&declarator.pattern.stx.pat, file, cancelled);
      if let Some(init) = &declarator.initializer {
        self.index_expr(init, file, cancelled);
      }
    }
  }

  fn index_function(
    &mut self,
    func: &Node<Func>,
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) {
    self.index_function_with_context(func, file, cancelled, None);
  }

  fn index_function_with_context(
    &mut self,
    func: &Node<Func>,
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
    class_member: Option<ClassMemberContext>,
  ) {
    let func_span = loc_to_range(file, func.loc);
    if func.stx.body.is_some() {
      self.functions.push(FunctionInfo {
        func_span,
        func: func as *const _,
        is_arrow: func.stx.arrow,
        class_member,
      });
    }

    for param in func.stx.parameters.iter() {
      let pat_span = loc_to_range(file, param.stx.pattern.loc);
      self
        .pats
        .insert(pat_span, &param.stx.pattern.stx.pat as *const _);
      self.params.insert(pat_span, param as *const _);
      self.index_pat(&param.stx.pattern.stx.pat, file, cancelled);
      if let Some(default) = &param.stx.default_value {
        self.index_expr(default, file, cancelled);
      }
    }

    if let Some(body) = &func.stx.body {
      match body {
        FuncBody::Block(block) => self.index_stmt_list(block, file, cancelled),
        FuncBody::Expression(expr) => self.index_expr(expr, file, cancelled),
      }
    }
  }

  fn index_jsx_elem(
    &mut self,
    elem: &Node<JsxElem>,
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) {
    for (idx, attr) in elem.stx.attributes.iter().enumerate() {
      if idx % 256 == 0 {
        Self::check_cancelled(cancelled);
      }
      match attr {
        JsxAttr::Named { value, .. } => match value {
          Some(JsxAttrVal::Expression(container)) => {
            if !is_empty_jsx_expr_placeholder(&container.stx.value) {
              self.index_expr(&container.stx.value, file, cancelled);
            }
          }
          Some(JsxAttrVal::Element(child)) => self.index_jsx_elem(child, file, cancelled),
          Some(JsxAttrVal::Text(_)) | None => {}
        },
        JsxAttr::Spread { value } => {
          if !is_empty_jsx_expr_placeholder(&value.stx.value) {
            self.index_expr(&value.stx.value, file, cancelled);
          }
        }
      }
    }

    for (idx, child) in elem.stx.children.iter().enumerate() {
      if idx % 256 == 0 {
        Self::check_cancelled(cancelled);
      }
      match child {
        JsxElemChild::Element(child_elem) => self.index_jsx_elem(child_elem, file, cancelled),
        JsxElemChild::Expr(container) => {
          if !is_empty_jsx_expr_placeholder(&container.stx.value) {
            self.index_expr(&container.stx.value, file, cancelled);
          }
        }
        JsxElemChild::Text(_) => {}
      }
    }
  }

  fn index_expr(
    &mut self,
    expr: &Node<AstExpr>,
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) {
    let span = loc_to_range(file, expr.loc);
    self.exprs.insert(span, expr as *const _);
    match expr.stx.as_ref() {
      AstExpr::Binary(bin) => {
        self.index_expr(&bin.stx.left, file, cancelled);
        self.index_expr(&bin.stx.right, file, cancelled);
      }
      AstExpr::Call(call) => {
        self.index_expr(&call.stx.callee, file, cancelled);
        for arg in call.stx.arguments.iter() {
          self.index_expr(&arg.stx.value, file, cancelled);
        }
      }
      AstExpr::Instantiation(inst) => {
        self.index_expr(&inst.stx.expression, file, cancelled);
      }
      AstExpr::Member(mem) => {
        self.index_expr(&mem.stx.left, file, cancelled);
      }
      AstExpr::ComputedMember(mem) => {
        self.index_expr(&mem.stx.object, file, cancelled);
        self.index_expr(&mem.stx.member, file, cancelled);
      }
      AstExpr::Cond(cond) => {
        self.index_expr(&cond.stx.test, file, cancelled);
        self.index_expr(&cond.stx.consequent, file, cancelled);
        self.index_expr(&cond.stx.alternate, file, cancelled);
      }
      AstExpr::Import(import) => {
        self.index_expr(&import.stx.module, file, cancelled);
        if let Some(attributes) = import.stx.attributes.as_ref() {
          self.index_expr(attributes, file, cancelled);
        }
      }
      AstExpr::Unary(un) => {
        self.index_expr(&un.stx.argument, file, cancelled);
      }
      AstExpr::UnaryPostfix(post) => {
        self.index_expr(&post.stx.argument, file, cancelled);
      }
      AstExpr::TaggedTemplate(tagged) => {
        self.index_expr(&tagged.stx.function, file, cancelled);
        for part in tagged.stx.parts.iter() {
          match part {
            parse_js::ast::expr::lit::LitTemplatePart::Substitution(expr) => {
              self.index_expr(expr, file, cancelled)
            }
            parse_js::ast::expr::lit::LitTemplatePart::String(_) => {}
          }
        }
      }
      AstExpr::LitArr(arr) => {
        for elem in arr.stx.elements.iter() {
          match elem {
            parse_js::ast::expr::lit::LitArrElem::Single(v)
            | parse_js::ast::expr::lit::LitArrElem::Rest(v) => self.index_expr(v, file, cancelled),
            parse_js::ast::expr::lit::LitArrElem::Empty => {}
          }
        }
      }
      AstExpr::LitObj(obj) => {
        for member in obj.stx.members.iter() {
          match &member.stx.typ {
            ObjMemberType::Valued { key, val } => {
              if let ClassOrObjKey::Computed(expr) = key {
                self.index_expr(expr, file, cancelled);
              }
              match val {
                ClassOrObjVal::Getter(getter) => {
                  self.index_function(&getter.stx.func, file, cancelled)
                }
                ClassOrObjVal::Setter(setter) => {
                  self.index_function(&setter.stx.func, file, cancelled)
                }
                ClassOrObjVal::Method(method) => {
                  self.index_function(&method.stx.func, file, cancelled)
                }
                ClassOrObjVal::Prop(Some(expr)) => self.index_expr(expr, file, cancelled),
                ClassOrObjVal::StaticBlock(block) => {
                  self.index_stmt_list(&block.stx.body, file, cancelled)
                }
                ClassOrObjVal::Prop(None) | ClassOrObjVal::IndexSignature(_) => {}
              }
            }
            ObjMemberType::Rest { val } => self.index_expr(val, file, cancelled),
            ObjMemberType::Shorthand { .. } => {}
          }
        }
      }
      AstExpr::LitTemplate(template) => {
        for part in template.stx.parts.iter() {
          match part {
            parse_js::ast::expr::lit::LitTemplatePart::Substitution(expr) => {
              self.index_expr(expr, file, cancelled)
            }
            parse_js::ast::expr::lit::LitTemplatePart::String(_) => {}
          }
        }
      }
      AstExpr::Class(class_expr) => {
        let class_name = class_expr
          .stx
          .name
          .as_ref()
          .map(|name| name.stx.name.clone());
        let type_params: Vec<String> = class_expr
          .stx
          .type_parameters
          .as_ref()
          .map(|params| params.iter().map(|param| param.stx.name.clone()).collect())
          .unwrap_or_default();
        let extends_name =
          class_expr
            .stx
            .extends
            .as_ref()
            .and_then(|expr| match expr.stx.as_ref() {
              AstExpr::Id(id) => Some(id.stx.name.clone()),
              AstExpr::Instantiation(inst) => match inst.stx.expression.stx.as_ref() {
                AstExpr::Id(id) => Some(id.stx.name.clone()),
                _ => None,
              },
               _ => None,
             });
        let class_index = self.register_class(class_name, extends_name, type_params);
        for decorator in class_expr.stx.decorators.iter() {
          self.index_expr(&decorator.stx.expression, file, cancelled);
        }
        if let Some(extends) = class_expr.stx.extends.as_ref() {
          self.index_expr(extends, file, cancelled);
        }
        self.index_class_members_in_class(&class_expr.stx.members, class_index, file, cancelled);
      }
      AstExpr::Func(func) => self.index_function(&func.stx.func, file, cancelled),
      AstExpr::ArrowFunc(func) => self.index_function(&func.stx.func, file, cancelled),
      AstExpr::JsxElem(elem) => self.index_jsx_elem(elem, file, cancelled),
      AstExpr::JsxExprContainer(container) => {
        if !is_empty_jsx_expr_placeholder(&container.stx.value) {
          self.index_expr(&container.stx.value, file, cancelled);
        }
      }
      AstExpr::JsxSpreadAttr(spread) => {
        if !is_empty_jsx_expr_placeholder(&spread.stx.value) {
          self.index_expr(&spread.stx.value, file, cancelled);
        }
      }
      AstExpr::TypeAssertion(assert) => self.index_expr(&assert.stx.expression, file, cancelled),
      AstExpr::NonNullAssertion(assert) => self.index_expr(&assert.stx.expression, file, cancelled),
      AstExpr::SatisfiesExpr(expr) => self.index_expr(&expr.stx.expression, file, cancelled),
      AstExpr::Id(..)
      | AstExpr::LitNull(..)
      | AstExpr::LitStr(..)
      | AstExpr::LitNum(..)
      | AstExpr::LitBool(..)
      | AstExpr::LitBigInt(..)
      | AstExpr::This(..)
      | AstExpr::Super(..)
      | AstExpr::IdPat(..)
      | AstExpr::ArrPat(..)
      | AstExpr::ObjPat(..)
      | AstExpr::ImportMeta(..)
      | AstExpr::JsxMember(..)
      | AstExpr::JsxName(..)
      | AstExpr::JsxText(..)
      | AstExpr::LitRegex(..)
      | AstExpr::NewTarget(..) => {}
    }
  }

  fn register_class(&mut self, name: Option<String>, extends: Option<String>, type_params: Vec<String>) -> usize {
    let index = self.classes.len();
    if let Some(name) = name.as_ref() {
      self.classes_by_name.entry(name.clone()).or_insert(index);
    }
    self.classes.push(ClassInfo {
      name,
      extends,
      type_params,
      instance_fields: Vec::new(),
      static_fields: Vec::new(),
      instance_param_props: Vec::new(),
      instance_param_props_private: HashSet::new(),
      instance_member_names: HashSet::new(),
    });
    index
  }

  fn index_class_members_in_class(
    &mut self,
    members: &[Node<ClassMember>],
    class_index: usize,
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) {
    let mut param_props: Vec<String> = Vec::new();
    let mut param_props_private: HashSet<String> = HashSet::new();
    for (idx, member) in members.iter().enumerate() {
      if idx % 128 == 0 {
        Self::check_cancelled(cancelled);
      }
      if member.stx.static_ {
        continue;
      }
      let is_constructor = matches!(
        &member.stx.key,
        ClassOrObjKey::Direct(key) if key.stx.key == "constructor"
      );
      if !is_constructor {
        continue;
      }
      let ClassOrObjVal::Method(method) = &member.stx.val else {
        continue;
      };
      for param in method.stx.func.stx.parameters.iter() {
        if param.stx.accessibility.is_some() || param.stx.readonly {
          if let AstPat::Id(id) = param.stx.pattern.stx.pat.stx.as_ref() {
            param_props.push(id.stx.name.clone());
            if matches!(
              param.stx.accessibility,
              Some(parse_js::ast::stmt::decl::Accessibility::Private)
            ) {
              param_props_private.insert(id.stx.name.clone());
            }
          }
        }
      }
    }
    if !param_props.is_empty() {
      param_props.sort();
      param_props.dedup();
    }
    if let Some(info) = self.classes.get_mut(class_index) {
      info.instance_param_props = param_props.clone();
      info.instance_param_props_private = param_props_private;
      for prop in param_props.iter() {
        info.instance_member_names.insert(prop.clone());
      }
    }

    for (idx, member) in members.iter().enumerate() {
      if idx % 128 == 0 {
        Self::check_cancelled(cancelled);
      }
      self.index_class_member(member, class_index, idx, &param_props, file, cancelled);
    }
  }

  fn index_class_member(
    &mut self,
    member: &Node<ClassMember>,
    class_index: usize,
    member_index: usize,
    param_props: &[String],
    file: FileId,
    cancelled: Option<&Arc<AtomicBool>>,
  ) {
    for decorator in member.stx.decorators.iter() {
      self.index_expr(&decorator.stx.expression, file, cancelled);
    }
    match &member.stx.key {
      ClassOrObjKey::Computed(expr) => self.index_expr(expr, file, cancelled),
      ClassOrObjKey::Direct(_) => {}
    }
    if let ClassOrObjKey::Direct(key) = &member.stx.key {
      let name = key.stx.key.clone();
      let is_constructor_method =
        name == "constructor" && matches!(member.stx.val, ClassOrObjVal::Method(_));
      if !member.stx.static_ && !is_constructor_method {
        if let Some(info) = self.classes.get_mut(class_index) {
          info.instance_member_names.insert(name);
        }
      }
    }
    if let (ClassOrObjKey::Direct(key), ClassOrObjVal::Prop(_)) = (&member.stx.key, &member.stx.val)
    {
      if let Some(info) = self.classes.get_mut(class_index) {
        let field = ClassFieldDecl {
          name: key.stx.key.clone(),
          member_index,
          optional: member.stx.optional,
          has_initializer: matches!(member.stx.val, ClassOrObjVal::Prop(Some(_))),
          is_private: matches!(
            member.stx.accessibility,
            Some(parse_js::ast::stmt::decl::Accessibility::Private)
          ),
          key_range: {
            let range = loc_to_range(file, key.loc);
            let name = key.stx.key.as_str();
            // `parse-js` key locations can currently span beyond the identifier
            // token (e.g. include a trailing type annotation). TS2612 expects the
            // diagnostic to underline only the member name, so shrink
            // identifier-like keys to their textual length.
            let is_identifier_like = name.chars().enumerate().all(|(idx, ch)| match idx {
              0 => ch == '_' || ch == '$' || ch.is_ascii_alphabetic(),
              _ => ch == '_' || ch == '$' || ch.is_ascii_alphanumeric(),
            });
            if is_identifier_like {
              TextRange::new(range.start, range.start.saturating_add(name.len() as u32))
            } else {
              range
            }
          },
        };
        if member.stx.static_ {
          info.static_fields.push(field);
        } else {
          info.instance_fields.push(field);
        }
      }
    }
    match &member.stx.val {
      ClassOrObjVal::Getter(getter) => {
        self.class_member_functions.insert(
          loc_to_range(file, getter.stx.func.loc),
          ClassMemberFunctionInfo {
            class_index,
            is_static: member.stx.static_,
            is_constructor: false,
          },
        );
        self.index_function_with_context(
          &getter.stx.func,
          file,
          cancelled,
          Some(ClassMemberContext {
            class_index,
            is_static: member.stx.static_,
            kind: MemberKind::Getter,
          }),
        );
      }
      ClassOrObjVal::Setter(setter) => {
        self.class_member_functions.insert(
          loc_to_range(file, setter.stx.func.loc),
          ClassMemberFunctionInfo {
            class_index,
            is_static: member.stx.static_,
            is_constructor: false,
          },
        );
        self.index_function_with_context(
          &setter.stx.func,
          file,
          cancelled,
          Some(ClassMemberContext {
            class_index,
            is_static: member.stx.static_,
            kind: MemberKind::Setter,
          }),
        );
      }
      ClassOrObjVal::Method(method) => {
        let is_constructor = !member.stx.static_
          && matches!(
            &member.stx.key,
            ClassOrObjKey::Direct(key) if key.stx.key == "constructor"
          );
        self.class_member_functions.insert(
          loc_to_range(file, method.stx.func.loc),
          ClassMemberFunctionInfo {
            class_index,
            is_static: member.stx.static_,
            is_constructor,
          },
        );
        let is_constructor = !member.stx.static_
          && matches!(
            &member.stx.key,
            ClassOrObjKey::Direct(key) if key.stx.key == "constructor"
          );
        self.index_function_with_context(
          &method.stx.func,
          file,
          cancelled,
          Some(ClassMemberContext {
            class_index,
            is_static: member.stx.static_,
            kind: if is_constructor {
              MemberKind::Constructor
            } else {
              MemberKind::Method
            },
          }),
        );
      }
      ClassOrObjVal::Prop(Some(expr)) => {
        let init_range = loc_to_range(file, expr.loc);
        self.class_field_initializers.insert(
          init_range,
          ClassFieldInitializerInfo {
            class_index,
            member_index,
            is_static: member.stx.static_,
          },
        );
        if !param_props.is_empty() && !member.stx.static_ {
          self
            .class_field_param_props
            .insert(init_range, param_props.to_vec());
        }
        self.index_expr(expr, file, cancelled);
      }
      ClassOrObjVal::Prop(None) => {}
      ClassOrObjVal::IndexSignature(_) => {}
      ClassOrObjVal::StaticBlock(block) => {
        let span =
          span_for_stmt_list(&block.stx.body, file).unwrap_or(loc_to_range(file, block.loc));
        self.class_static_blocks.push(ClassStaticBlockInfo {
          span,
          block: block as *const _,
          class_index,
          member_index,
        });
        self.index_stmt_list(&block.stx.body, file, cancelled);
      }
    }
  }

  fn class_field_initializer(&self, span: TextRange) -> Option<ClassFieldInitializerInfo> {
    self.class_field_initializers.get(&span).copied()
  }

  fn enclosing_class_field_initializer(
    &self,
    span: TextRange,
  ) -> Option<(TextRange, ClassFieldInitializerInfo)> {
    let mut best: Option<(u32, TextRange, ClassFieldInitializerInfo)> = None;
    for (init_span, info) in self.class_field_initializers.iter() {
      let init_span = *init_span;
      if !contains_range(init_span, span) && !contains_range(span, init_span) {
        continue;
      }
      let len = init_span.end.saturating_sub(init_span.start);
      let replace = match best {
        Some((best_len, best_span, _)) => {
          len < best_len || (len == best_len && init_span.start < best_span.start)
        }
        None => true,
      };
      if replace {
        best = Some((len, init_span, *info));
      }
    }
    best.map(|(_, span, info)| (span, info))
  }

  fn class_member_function(&self, span: TextRange) -> Option<ClassMemberFunctionInfo> {
    self.class_member_functions.get(&span).copied()
  }

  fn enclosing_class_field_param_props(&self, span: TextRange) -> Option<&[String]> {
    let mut best: Option<(u32, &Vec<String>)> = None;
    for (init_span, props) in self.class_field_param_props.iter() {
      let init_span = *init_span;
      if !contains_range(init_span, span) && !contains_range(span, init_span) {
        continue;
      }
      let len = init_span.end.saturating_sub(init_span.start);
      let replace = match best {
        Some((best_len, _)) => len < best_len,
        None => true,
      };
      if replace {
        best = Some((len, props));
      }
    }
    best.map(|(_, props)| props.as_slice())
  }

  fn enclosing_class_static_block(&self, span: TextRange) -> Option<ClassStaticBlockInfo> {
    let mut best: Option<(u32, ClassStaticBlockInfo)> = None;
    for block in self.class_static_blocks.iter().copied() {
      if !contains_range(block.span, span) && !contains_range(span, block.span) {
        continue;
      }
      let len = block.span.end.saturating_sub(block.span.start);
      let replace = match best {
        Some((best_len, _)) => len < best_len,
        None => true,
      };
      if replace {
        best = Some((len, block));
      }
    }
    best.map(|(_, block)| block)
  }

  fn field_declared_not_before(
    &self,
    class_index: usize,
    member_index: usize,
    name: &str,
    is_static: bool,
  ) -> bool {
    let Some(info) = self.classes.get(class_index) else {
      return false;
    };
    let fields = if is_static {
      info.static_fields.as_slice()
    } else {
      info.instance_fields.as_slice()
    };
    fields
      .iter()
      .any(|field| !field.optional && field.name == name && field.member_index >= member_index)
  }

  fn instance_prop_declared_in_ancestor(&self, class_index: usize, name: &str) -> bool {
    let mut current = self
      .classes
      .get(class_index)
      .and_then(|info| info.extends.clone());
    let mut visited = HashSet::<String>::new();
    while let Some(base_name) = current.take() {
      if !visited.insert(base_name.clone()) {
        break;
      }
      let Some(base_index) = self.classes_by_name.get(&base_name).copied() else {
        break;
      };
      let Some(base) = self.classes.get(base_index) else {
        break;
      };
      if base.instance_member_names.contains(name) {
        return true;
      }
      current = base.extends.clone();
    }
    false
  }

  fn instance_data_prop_declared_in_base_chain(&self, base_name: &str, prop: &str) -> bool {
    let mut current = Some(base_name.to_string());
    let mut visited = HashSet::<String>::new();
    while let Some(base_name) = current.take() {
      if !visited.insert(base_name.clone()) {
        break;
      }
      let Some(base_index) = self.classes_by_name.get(&base_name).copied() else {
        break;
      };
      let Some(base) = self.classes.get(base_index) else {
        break;
      };

      if base
        .instance_fields
        .iter()
        .any(|field| field.name == prop && !field.name.starts_with('#') && !field.is_private)
      {
        return true;
      }
      if base
        .instance_param_props
        .iter()
        .any(|name| name == prop && !base.instance_param_props_private.contains(prop))
      {
        return true;
      }

      current = base.extends.clone();
    }
    false
  }

  fn index_pat(&mut self, pat: &Node<AstPat>, file: FileId, cancelled: Option<&Arc<AtomicBool>>) {
    let span = loc_to_range(file, pat.loc);
    self.pats.insert(span, pat as *const _);
    match pat.stx.as_ref() {
      AstPat::Arr(arr) => {
        for elem in arr.stx.elements.iter().flatten() {
          self.index_pat(&elem.target, file, cancelled);
          if let Some(default) = &elem.default_value {
            self.index_expr(default, file, cancelled);
          }
        }
        if let Some(rest) = &arr.stx.rest {
          self.index_pat(rest, file, cancelled);
        }
      }
      AstPat::Obj(obj) => {
        for prop in obj.stx.properties.iter() {
          if let ClassOrObjKey::Computed(expr) = &prop.stx.key {
            self.index_expr(expr, file, cancelled);
          }
          self.index_pat(&prop.stx.target, file, cancelled);
          if let Some(default) = &prop.stx.default_value {
            self.index_expr(default, file, cancelled);
          }
        }
        if let Some(rest) = &obj.stx.rest {
          self.index_pat(rest, file, cancelled);
        }
      }
      AstPat::Id(_) | AstPat::AssignTarget(_) => {}
    }
  }
}

/// Per-body context needed for correctly typing `this`/`super` expressions.
///
/// This is computed by `ProgramState`/DB callers and threaded into the base and
/// flow body checkers so syntax like `super()` can resolve against the base
/// class constructor signatures even when the `super` keyword itself is typed
/// as the base instance type for `super.prop`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BodyThisSuperContext {
  /// Type of the `this` keyword within the current body.
  pub this_ty: Option<TypeId>,
  /// Type of the `super` keyword within the current body (for `super.prop`).
  pub super_ty: Option<TypeId>,
  pub super_instance_ty: Option<TypeId>,
  pub super_value_ty: Option<TypeId>,
}

/// Type-check a lowered HIR body, producing per-expression and per-pattern type tables.
pub fn check_body(
  body_id: BodyId,
  body: &Body,
  names: &NameInterner,
  file: FileId,
  ast_index: &AstIndex,
  store: Arc<TypeStore>,
  target: ScriptTarget,
  use_define_for_class_fields: bool,
  caches: &BodyCaches,
  bindings: &HashMap<String, TypeId>,
  value_defs: &HashMap<DefId, DefId>,
  resolver: Option<Arc<dyn TypeResolver>>,
  expr_value_overrides: Option<&HashMap<TextRange, TypeId>>,
) -> BodyCheckResult {
  check_body_with_expander(
    body_id,
    body,
    names,
    file,
    ast_index,
    store,
    target,
    use_define_for_class_fields,
    caches,
    bindings,
    resolver,
    value_defs,
    None,
    None,
    None,
    None,
    BodyThisSuperContext::default(),
    expr_value_overrides,
    None,
    false,
    false,
    None,
    None,
    None,
  )
}

/// Type-check a lowered HIR body with an optional reference type expander for
/// relation checks. The expander is used to lazily resolve `TypeKind::Ref`
/// nodes during assignability comparisons.
pub fn check_body_with_expander(
  body_id: BodyId,
  body: &Body,
  names: &NameInterner,
  file: FileId,
  ast_index: &AstIndex,
  store: Arc<TypeStore>,
  target: ScriptTarget,
  use_define_for_class_fields: bool,
  caches: &BodyCaches,
  bindings: &HashMap<String, TypeId>,
  resolver: Option<Arc<dyn TypeResolver>>,
  value_defs: &HashMap<DefId, DefId>,
  def_spans: Option<&HashMap<(FileId, TextRange), DefId>>,
  relate_expander: Option<&dyn types_ts_interned::RelateTypeExpander>,
  type_param_decls: Option<&HashMap<DefId, Arc<[TypeParamDecl]>>>,
  contextual_fn_ty: Option<TypeId>,
  this_super_context: BodyThisSuperContext,
  expr_value_overrides: Option<&HashMap<TextRange, TypeId>>,
  current_class_def: Option<DefId>,
  strict_native: bool,
  no_implicit_any: bool,
  jsx_mode: Option<JsxMode>,
  jsx_import_source: Option<String>,
  cancelled: Option<&Arc<AtomicBool>>,
) -> BodyCheckResult {
  if let Some(flag) = cancelled {
    if flag.load(Ordering::Relaxed) {
      panic_any(crate::FatalError::Cancelled);
    }
  }
  let prim = store.primitive_ids();
  let expr_types = vec![prim.unknown; body.exprs.len()];
  let call_signatures = vec![None; body.exprs.len()];
  let pat_types = vec![prim.unknown; body.pats.len()];
  let expr_spans: Vec<TextRange> = body.exprs.iter().map(|e| e.span).collect();
  let pat_spans: Vec<TextRange> = body.pats.iter().map(|p| p.span).collect();
  let ast = ast_index.ast();

  let expr_map: HashMap<TextRange, ExprId> = body
    .exprs
    .iter()
    .enumerate()
    .map(|(idx, expr)| (expr.span, ExprId(idx as u32)))
    .collect();
  let pat_map: HashMap<TextRange, PatId> = body
    .pats
    .iter()
    .enumerate()
    .map(|(idx, pat)| (pat.span, PatId(idx as u32)))
    .collect();
  let mut class_expr_def_by_span: HashMap<TextRange, DefId> = HashMap::new();
  for expr in body.exprs.iter() {
    if let ExprKind::ClassExpr { def, .. } = &expr.kind {
      class_expr_def_by_span.insert(expr.span, *def);
    }
  }
  let mut decl_def_by_span: HashMap<TextRange, DefId> = HashMap::new();
  for stmt in body.stmts.iter() {
    if let StmtKind::Decl(def) = &stmt.kind {
      decl_def_by_span.insert(stmt.span, *def);
    }
  }

  // TypeLowerer normally resolves names via module/file bindings. However,
  // classes declared inside a body introduce both a value binding (constructor)
  // and a type binding (instance type), so we build a body-scoped resolver that
  // knows about local class declarations.
  //
  // Note: We approximate block scoping by picking a deterministic "winner" per
  // name across the whole body, since the AST body checker does not currently
  // model nested type scopes.
  let mut local_class_defs: HashMap<String, (TextRange, DefId, DefId)> = HashMap::new();
  for (span, def_id) in decl_def_by_span.iter() {
    let Some(stmt) = ast_index.stmts.get(span).copied() else {
      continue;
    };
    // Safety: `AstIndex` stores immutable pointers into an Arc-owned AST.
    let stmt = unsafe { &*stmt };
    let Stmt::ClassDecl(class_decl) = stmt.stx.as_ref() else {
      continue;
    };
    let Some(name) = class_decl.stx.name.as_ref() else {
      continue;
    };
    let name = name.stx.name.clone();
    let value_def = value_defs.get(def_id).copied().unwrap_or(*def_id);
    let replace = match local_class_defs.get(&name) {
      None => true,
      Some((existing_span, existing_def_id, _)) => {
        (span.start, span.end, def_id.0) < (existing_span.start, existing_span.end, existing_def_id.0)
      }
    };
    if replace {
      local_class_defs.insert(name, (*span, *def_id, value_def));
    }
  }
  let (local_type_defs, local_value_defs): (HashMap<String, DefId>, HashMap<String, DefId>) =
    local_class_defs
      .into_iter()
      .map(|(name, (_span, type_def, value_def))| ((name.clone(), type_def), (name, value_def)))
      .unzip();

  let body_range = body_range(body);
  let mut relate_hooks = super::relate_hooks();
  let check_cancelled = || {
    if let Some(flag) = cancelled {
      if flag.load(Ordering::Relaxed) {
        panic_any(crate::FatalError::Cancelled);
      }
    }
  };
  relate_hooks.check_cancelled = Some(&check_cancelled);
  if let Some(expander) = relate_expander {
    relate_hooks.expander = Some(expander);
  }
  let relate = RelateCtx::with_hooks_and_cache(
    Arc::clone(&store),
    store.options(),
    relate_hooks,
    caches.relation.clone(),
  );
  let resolver = if local_type_defs.is_empty() && local_value_defs.is_empty() {
    resolver
  } else {
    Some(Arc::new(BodyLocalTypeResolver {
      locals_type: local_type_defs,
      locals_value: local_value_defs,
      inner: resolver,
    }) as Arc<_>)
  };
  let type_resolver = resolver.clone();
  let mut lowerer = match resolver {
    Some(resolver) => TypeLowerer::with_resolver(Arc::clone(&store), resolver),
    None => TypeLowerer::new(Arc::clone(&store)),
  };
  lowerer.set_file(file);
  lowerer.set_strict_native(strict_native);
  let synthetic_top_level = matches!(body.kind, BodyKind::TopLevel)
    && body.exprs.is_empty()
    && body.stmts.is_empty()
    && body.pats.is_empty();
  let native_define_class_fields =
    use_define_for_class_fields && matches!(target, ScriptTarget::Es2022 | ScriptTarget::EsNext);
  let jsx_runtime_module = match jsx_mode {
    Some(JsxMode::ReactJsx) => {
      let base = jsx_import_source
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("react");
      Some(Arc::<str>::from(format!("{base}/jsx-runtime")))
    }
    Some(JsxMode::ReactJsxdev) => {
      let base = jsx_import_source
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("react");
      Some(Arc::<str>::from(format!("{base}/jsx-dev-runtime")))
    }
    _ => None,
  };
  let prim = store.primitive_ids();
  let mut checker = Checker {
    store,
    relate,
    eval_caches: caches.eval.clone(),
    instantiation_cache: caches.instantiation.clone(),
    lowerer,
    type_resolver,
    jsx_mode,
    jsx_runtime_module,
    jsx_runtime_module_exists: None,
    jsx_runtime_module_missing_reported: false,
    jsx_element_ty: None,
    jsx_element_type_constraint_ty: None,
    jsx_element_class_ty: None,
    jsx_intrinsic_elements_ty: None,
    jsx_intrinsic_attributes_ty: None,
    jsx_intrinsic_class_attributes_def: None,
    jsx_element_attributes_prop_name: None,
    jsx_library_managed_attributes_def: None,
    jsx_children_prop_name: None,
    jsx_namespace_missing_reported: false,
    expr_types,
    call_signatures,
    pat_types,
    expr_spans,
    pat_spans,
    expr_map,
    pat_map,
    class_expr_def_by_span,
    decl_def_by_span,
    diagnostics: Vec::new(),
    implicit_any_reported: HashSet::new(),
    return_types: Vec::new(),
    index: ast_index,
    value_defs,
    def_spans,
    scopes: vec![Scope::default()],
    var_scopes: vec![0],
    type_param_scopes: Vec::new(),
    namespace_scopes: HashMap::new(),
    expected_return: None,
    body_kind: body.kind,
    in_async_function: false,
    check_var_assignments: !synthetic_top_level,
    widen_object_literals: true,
    strict_native,
    no_implicit_any,
    use_define_for_class_fields,
    native_define_class_fields,
    current_this_ty: prim.unknown,
    current_super_ty: prim.unknown,
    current_super_ctor_ty: prim.unknown,
    current_class_field_param_props: None,
    class_field_initializer: None,
    current_class_def,
    file,
    ref_expander: relate_expander,
    def_type_param_decls: type_param_decls,
    contextual_fn_ty,
    this_super_context,
    expr_value_overrides,
    cancelled,
    _names: names,
    _bump: Bump::new(),
  };

  checker.seed_builtins();
  for (name, ty) in bindings {
    checker.insert_binding(name.clone(), *ty, Vec::new());
  }

  match body.kind {
    BodyKind::TopLevel => {
      checker.check_class_field_overwrites_base_properties();
      checker.hoist_function_decls_in_stmt_list(&ast.stx.body);
      checker.hoist_var_decls_in_stmt_tree(&ast.stx.body);
      checker.check_stmt_list(&ast.stx.body);
    }
    BodyKind::Function => {
      let pushed_class_type_params = checker.push_class_type_param_scope(body_range);
      let found = checker.check_enclosing_function(body_range);
      if pushed_class_type_params {
        checker.pop_class_type_param_scope();
      }
      if !found {
        checker.diagnostics.push(codes::MISSING_BODY.error(
          "missing function body for checker",
          Span::new(file, body_range),
        ));
      }
    }
    BodyKind::Initializer => {
      let pushed_class_type_params = checker.push_class_type_param_scope(body_range);
      let found = checker.check_matching_initializer(body_range);
      if pushed_class_type_params {
        checker.pop_class_type_param_scope();
      }
      if !found {
        checker.diagnostics.push(codes::MISSING_BODY.error(
          "missing initializer body for checker",
          Span::new(file, body_range),
        ));
      }
    }
    BodyKind::Class => {
      let found = checker.check_enclosing_class(body_range);
      if !found {
        checker.diagnostics.push(codes::MISSING_BODY.error(
          "missing class body for checker",
          Span::new(file, body_range),
        ));
      }
    }
    BodyKind::Unknown => {
      checker
        .diagnostics
        .push(codes::MISSING_BODY.error("missing body for checker", Span::new(file, body_range)));
    }
  }

  checker
    .diagnostics
    .extend(checker.lowerer.take_diagnostics());
  if checker.strict_native {
    checker.report_forbidden_any_types();
  }
  codes::normalize_diagnostics(&mut checker.diagnostics);
  BodyCheckResult {
    body: body_id,
    expr_types: checker.expr_types,
    call_signatures: checker.call_signatures,
    expr_spans: checker.expr_spans,
    pat_types: checker.pat_types,
    pat_spans: checker.pat_spans,
    diagnostics: checker.diagnostics,
    return_types: checker.return_types,
  }
}

#[derive(Clone, Debug)]
enum ArrayLiteralContext {
  Tuple(Vec<types_ts_interned::TupleElem>),
  Array(TypeId),
}

#[derive(Debug)]
struct JsxActualProps {
  ty: TypeId,
  props: HashSet<String>,
  named_props: Vec<(String, TextRange)>,
  explicit_attr_count: usize,
}

#[derive(Clone, Copy, Debug)]
enum JsxAttributesPropertyName {
  /// `JSX.ElementAttributesProperty` does not exist.
  Missing,
  /// `JSX.ElementAttributesProperty` exists but has no properties.
  Empty,
  /// `JSX.ElementAttributesProperty` has exactly one property.
  Name(TsNameId),
}

#[derive(Clone, Copy, Debug)]
struct ClassFieldInitializerContext {
  class_index: usize,
  member_index: usize,
  is_static: bool,
}

struct Checker<'a> {
  store: Arc<TypeStore>,
  relate: RelateCtx<'a>,
  eval_caches: EvaluatorCaches,
  instantiation_cache: InstantiationCache,
  lowerer: TypeLowerer,
  type_resolver: Option<Arc<dyn TypeResolver>>,
  jsx_mode: Option<JsxMode>,
  jsx_runtime_module: Option<Arc<str>>,
  jsx_runtime_module_exists: Option<bool>,
  jsx_runtime_module_missing_reported: bool,
  jsx_element_ty: Option<TypeId>,
  jsx_element_type_constraint_ty: Option<Option<TypeId>>,
  jsx_element_class_ty: Option<TypeId>,
  jsx_intrinsic_elements_ty: Option<TypeId>,
  jsx_intrinsic_attributes_ty: Option<TypeId>,
  jsx_intrinsic_class_attributes_def: Option<Option<DefId>>,
  jsx_element_attributes_prop_name: Option<JsxAttributesPropertyName>,
  jsx_library_managed_attributes_def: Option<Option<DefId>>,
  jsx_children_prop_name: Option<Option<TsNameId>>,
  jsx_namespace_missing_reported: bool,
  expr_types: Vec<TypeId>,
  call_signatures: Vec<Option<SignatureId>>,
  pat_types: Vec<TypeId>,
  expr_spans: Vec<TextRange>,
  pat_spans: Vec<TextRange>,
  expr_map: HashMap<TextRange, ExprId>,
  pat_map: HashMap<TextRange, PatId>,
  class_expr_def_by_span: HashMap<TextRange, DefId>,
  decl_def_by_span: HashMap<TextRange, DefId>,
  diagnostics: Vec<Diagnostic>,
  implicit_any_reported: HashSet<TextRange>,
  return_types: Vec<TypeId>,
  index: &'a AstIndex,
  value_defs: &'a HashMap<DefId, DefId>,
  def_spans: Option<&'a HashMap<(FileId, TextRange), DefId>>,
  scopes: Vec<Scope>,
  /// Index of the nearest "var scope" in `scopes`.
  ///
  /// `var` declarations are function-scoped (not block-scoped). We keep a stack
  /// because the base checker can type-check nested function expressions (e.g.
  /// contextual callback typing) using the same `Checker` instance.
  var_scopes: Vec<usize>,
  type_param_scopes: Vec<Vec<TypeParamDecl>>,
  namespace_scopes: HashMap<String, Scope>,
  expected_return: Option<TypeId>,
  body_kind: BodyKind,
  in_async_function: bool,
  check_var_assignments: bool,
  widen_object_literals: bool,
  strict_native: bool,
  no_implicit_any: bool,
  use_define_for_class_fields: bool,
  native_define_class_fields: bool,
  current_this_ty: TypeId,
  current_super_ty: TypeId,
  current_super_ctor_ty: TypeId,
  current_class_field_param_props: Option<&'a [String]>,
  class_field_initializer: Option<ClassFieldInitializerContext>,
  current_class_def: Option<DefId>,
  file: FileId,
  ref_expander: Option<&'a dyn types_ts_interned::RelateTypeExpander>,
  def_type_param_decls: Option<&'a HashMap<DefId, Arc<[TypeParamDecl]>>>,
  contextual_fn_ty: Option<TypeId>,
  this_super_context: BodyThisSuperContext,
  expr_value_overrides: Option<&'a HashMap<TextRange, TypeId>>,
  cancelled: Option<&'a Arc<AtomicBool>>,
  _names: &'a NameInterner,
  _bump: Bump,
}

impl<'a> Checker<'a> {
  fn type_param_constraint(&self, param: TypeParamId) -> Option<TypeId> {
    for scope in self.type_param_scopes.iter().rev() {
      if let Some(decl) = scope.iter().find(|decl| decl.id == param) {
        return decl.constraint;
      }
    }
    None
  }

  fn enclosing_class_context_for_type_params(&self, span: TextRange) -> Option<(TextRange, usize, bool)> {
    let mut best: Option<(u32, TextRange, usize, bool)> = None;

    if let Some(block) = self.index.enclosing_class_static_block(span) {
      let len = block.span.end.saturating_sub(block.span.start);
      best = Some((len, block.span, block.class_index, true));
    }

    if let Some((init_span, info)) = self.index.enclosing_class_field_initializer(span) {
      let len = init_span.end.saturating_sub(init_span.start);
      let replace = match best {
        Some((best_len, _, _, _)) => len < best_len,
        None => true,
      };
      if replace {
        best = Some((len, init_span, info.class_index, info.is_static));
      }
    }

    for func in self.index.functions.iter().copied() {
      let Some(ctx) = func.class_member else {
        continue;
      };
      let contains = contains_range(func.func_span, span) || contains_range(span, func.func_span);
      if !contains {
        continue;
      }
      let len = func.func_span.end.saturating_sub(func.func_span.start);
      let replace = match best {
        Some((best_len, _, _, _)) => len < best_len,
        None => true,
      };
      if replace {
        best = Some((len, func.func_span, ctx.class_index, ctx.is_static));
      }
    }

    best.map(|(_, span, class_index, is_static)| (span, class_index, is_static))
  }

  fn push_class_type_param_scope(&mut self, span: TextRange) -> bool {
    let Some((_ctx_span, class_index, is_static)) = self.enclosing_class_context_for_type_params(span)
    else {
      return false;
    };
    if is_static {
      return false;
    }
    let Some(class_def) = self.current_class_def else {
      return false;
    };
    let Some(type_param_decls) = self
      .def_type_param_decls
      .and_then(|decls| decls.get(&class_def))
      .cloned()
    else {
      return false;
    };
    if type_param_decls.is_empty() {
      return false;
    }
    let Some(class_info) = self.index.classes.get(class_index) else {
      return false;
    };
    if class_info.type_params.is_empty() {
      return false;
    }

    self.lowerer.push_type_param_scope();
    for (name, decl) in class_info.type_params.iter().zip(type_param_decls.iter()) {
      self.lowerer.declare_type_param(name.clone(), decl.id);
    }
    self
      .type_param_scopes
      .push(type_param_decls.iter().cloned().collect());
    true
  }

  fn pop_class_type_param_scope(&mut self) {
    self.type_param_scopes.pop();
    self.lowerer.pop_type_param_scope();
  }

  fn expand_callable_type(&self, ty: TypeId) -> TypeId {
    let mut current = self.expand_ref(ty);
    let mut seen = HashSet::new();
    while seen.insert(current) {
      match self.store.type_kind(current) {
        TypeKind::TypeParam(param) => {
          let Some(constraint) = self.type_param_constraint(param) else {
            break;
          };
          current = self.expand_ref(constraint);
        }
        _ => break,
      }
    }
    current
  }

  fn apply_explicit_type_args(&mut self, base: TypeId, type_args: &[TypeId], span: Span) -> TypeId {
    if type_args.is_empty() {
      return base;
    }
    let mut reported_arity = false;
    let mut reported_constraint = false;

    fn instantiate_signature(
      checker: &mut Checker<'_>,
      sig_id: SignatureId,
      type_args: &[TypeId],
      span: Span,
      reported_arity: &mut bool,
      reported_constraint: &mut bool,
    ) -> SignatureId {
      let sig = checker.store.signature(sig_id);

      let declared = sig.type_params.len();
      if type_args.len() > declared && !*reported_arity {
        checker
          .diagnostics
          .push(codes::WRONG_TYPE_ARGUMENT_COUNT.error(
            format!(
              "Expected {declared} type arguments, but got {}.",
              type_args.len()
            ),
            span,
          ));
        *reported_arity = true;
      }

      let mut subst = HashMap::new();
      let mut prefix_subst: HashMap<TypeParamId, TypeId> = HashMap::new();
      for (idx, decl) in sig.type_params.iter().enumerate() {
        let Some(arg) = type_args.get(idx).copied() else {
          break;
        };

        if let Some(constraint) = decl.constraint {
          let instantiated_constraint = if prefix_subst.is_empty() {
            constraint
          } else {
            let mut substituter =
              Substituter::new(Arc::clone(&checker.store), prefix_subst.clone());
            substituter.substitute_type(constraint)
          };

          if !checker.relate.is_assignable(arg, instantiated_constraint) && !*reported_constraint {
            checker
              .diagnostics
              .push(codes::TYPE_ARGUMENT_CONSTRAINT_VIOLATION.error(
                format!(
                  "Type '{}' does not satisfy the constraint '{}'.",
                  TypeDisplay::new(checker.store.as_ref(), arg),
                  TypeDisplay::new(checker.store.as_ref(), instantiated_constraint)
                ),
                span,
              ));
            *reported_constraint = true;
          }
        }

        subst.insert(decl.id, arg);
        prefix_subst.insert(decl.id, arg);
      }

      checker
        .instantiation_cache
        .instantiate_signature(&checker.store, sig_id, &sig, &subst)
    }

    fn inner(
      checker: &mut Checker<'_>,
      ty: TypeId,
      type_args: &[TypeId],
      span: Span,
      reported_arity: &mut bool,
      reported_constraint: &mut bool,
    ) -> TypeId {
      let ty = checker.store.canon(ty);
      if let TypeKind::Ref { def, .. } = checker.store.type_kind(ty) {
        if let Some(decls_map) = checker.def_type_param_decls {
          let decls = decls_map
            .get(&def)
            .cloned()
            .or_else(|| {
              checker
                .value_defs
                .iter()
                .find_map(|(type_def, value_def)| {
                  (*value_def == def).then(|| decls_map.get(type_def).cloned()).flatten()
                })
            });
          if let Some(decls) = decls {
            let declared = decls.len();
            if type_args.len() > declared && !*reported_arity {
              checker
                .diagnostics
                .push(codes::WRONG_TYPE_ARGUMENT_COUNT.error(
                  format!(
                    "Expected {declared} type arguments, but got {}.",
                    type_args.len()
                  ),
                  span,
                ));
              *reported_arity = true;
            }
            if declared == 0 {
              return checker.store.primitive_ids().unknown;
            }

            let mut prefix_subst: HashMap<TypeParamId, TypeId> = HashMap::new();
            for (idx, decl) in decls.iter().enumerate() {
              let Some(arg) = type_args.get(idx).copied() else {
                break;
              };
              if let Some(constraint) = decl.constraint {
                let instantiated_constraint = if prefix_subst.is_empty() {
                  constraint
                } else {
                  let mut substituter =
                    Substituter::new(Arc::clone(&checker.store), prefix_subst.clone());
                  substituter.substitute_type(constraint)
                };
                if !checker.relate.is_assignable(arg, instantiated_constraint) && !*reported_constraint
                {
                  checker
                    .diagnostics
                    .push(codes::TYPE_ARGUMENT_CONSTRAINT_VIOLATION.error(
                      format!(
                        "Type '{}' does not satisfy the constraint '{}'.",
                        TypeDisplay::new(checker.store.as_ref(), arg),
                        TypeDisplay::new(checker.store.as_ref(), instantiated_constraint)
                      ),
                      span,
                    ));
                  *reported_constraint = true;
                }
              }
              prefix_subst.insert(decl.id, arg);
            }

            let stored_args: Vec<_> = type_args.iter().take(declared).copied().collect();
            return checker.store.intern_type(TypeKind::Ref {
              def,
              args: stored_args,
            });
          }
        }
      }

      let ty = checker.expand_callable_type(ty);
      match checker.store.type_kind(ty) {
        TypeKind::Any | TypeKind::Unknown | TypeKind::Never => ty,
        TypeKind::Callable { overloads } => {
          let mut instantiated: Vec<_> = overloads
            .iter()
            .copied()
            .map(|sig_id| {
              instantiate_signature(
                checker,
                sig_id,
                type_args,
                span,
                reported_arity,
                reported_constraint,
              )
            })
            .collect();
          instantiated.sort();
          instantiated.dedup();
          checker.store.intern_type(TypeKind::Callable {
            overloads: instantiated,
          })
        }
        TypeKind::Object(obj_id) => {
          let obj = checker.store.object(obj_id);
          let mut shape = checker.store.shape(obj.shape);
          let mut changed = false;

          if !shape.call_signatures.is_empty() {
            let mut call_sigs: Vec<_> = shape
              .call_signatures
              .iter()
              .copied()
              .map(|sig_id| {
                instantiate_signature(
                  checker,
                  sig_id,
                  type_args,
                  span,
                  reported_arity,
                  reported_constraint,
                )
              })
              .collect();
            call_sigs.sort();
            call_sigs.dedup();
            if call_sigs != shape.call_signatures {
              shape.call_signatures = call_sigs;
              changed = true;
            }
          }

          if !shape.construct_signatures.is_empty() {
            let mut construct_sigs: Vec<_> = shape
              .construct_signatures
              .iter()
              .copied()
              .map(|sig_id| {
                instantiate_signature(
                  checker,
                  sig_id,
                  type_args,
                  span,
                  reported_arity,
                  reported_constraint,
                )
              })
              .collect();
            construct_sigs.sort();
            construct_sigs.dedup();
            if construct_sigs != shape.construct_signatures {
              shape.construct_signatures = construct_sigs;
              changed = true;
            }
          }

          if !changed {
            return ty;
          }

          let shape_id = checker.store.intern_shape(shape);
          let obj_id = checker.store.intern_object(ObjectType { shape: shape_id });
          checker.store.intern_type(TypeKind::Object(obj_id))
        }
        TypeKind::Union(members) => {
          let mapped: Vec<_> = members
            .iter()
            .copied()
            .map(|member| {
              inner(
                checker,
                member,
                type_args,
                span,
                reported_arity,
                reported_constraint,
              )
            })
            .collect();
          checker.store.union(mapped)
        }
        TypeKind::Intersection(members) => {
          let mapped: Vec<_> = members
            .iter()
            .copied()
            .map(|member| {
              inner(
                checker,
                member,
                type_args,
                span,
                reported_arity,
                reported_constraint,
              )
            })
            .collect();
          checker.store.intersection(mapped)
        }
        _ => {
          if !*reported_arity {
            checker
              .diagnostics
              .push(codes::WRONG_TYPE_ARGUMENT_COUNT.error(
                format!("Expected 0 type arguments, but got {}.", type_args.len()),
                span,
              ));
            *reported_arity = true;
          }
          checker.store.primitive_ids().unknown
        }
      }
    }

    inner(
      self,
      base,
      type_args,
      span,
      &mut reported_arity,
      &mut reported_constraint,
    )
  }

  fn binding_name_range(&self, pat: &Node<AstPat>) -> TextRange {
    match pat.stx.as_ref() {
      AstPat::Id(id) => {
        let range = loc_to_range(self.file, id.loc);
        let len = id.stx.name.len() as u32;
        TextRange::new(range.start, range.start.saturating_add(len))
      }
      _ => loc_to_range(self.file, pat.loc),
    }
  }

  fn report_implicit_any(&mut self, range: TextRange, name: Option<&str>) {
    if !self.no_implicit_any {
      return;
    }
    if !self.implicit_any_reported.insert(range) {
      return;
    }
    self.diagnostics.push(codes::IMPLICIT_ANY.error(
      codes::implicit_any_message(name),
      Span::new(self.file, range),
    ));
  }

  fn report_implicit_any_in_pat(&mut self, pat: &Node<AstPat>) {
    match pat.stx.as_ref() {
      AstPat::Id(id) => {
        let range = self.binding_name_range(pat);
        self.report_implicit_any(range, Some(&id.stx.name));
      }
      AstPat::Arr(arr) => {
        for elem in arr.stx.elements.iter().flatten() {
          self.report_implicit_any_in_pat(&elem.target);
        }
        if let Some(rest) = &arr.stx.rest {
          self.report_implicit_any_in_pat(rest);
        }
      }
      AstPat::Obj(obj) => {
        for prop in obj.stx.properties.iter() {
          self.report_implicit_any_in_pat(&prop.stx.target);
        }
        if let Some(rest) = &obj.stx.rest {
          self.report_implicit_any_in_pat(rest);
        }
      }
      AstPat::AssignTarget(_) => {}
    }
  }

  fn report_forbidden_any_types(&mut self) {
    let prim = self.store.primitive_ids();
    for (idx, ty) in self.expr_types.iter().enumerate() {
      let ty = self.store.canon(*ty);
      if ty != prim.any {
        continue;
      }
      if let Some(span) = self.expr_spans.get(idx).copied() {
        self.diagnostics.push(codes::FORBIDDEN_ANY.error(
          "`any` is forbidden when `native_strict` is enabled",
          Span::new(self.file, span),
        ));
      }
    }

    for (idx, ty) in self.pat_types.iter().enumerate() {
      let ty = self.store.canon(*ty);
      if ty != prim.any {
        continue;
      }
      if let Some(span) = self.pat_spans.get(idx).copied() {
        self.diagnostics.push(codes::FORBIDDEN_ANY.error(
          "`any` is forbidden when `native_strict` is enabled",
          Span::new(self.file, span),
        ));
      }
    }
  }

  fn check_class_field_overwrites_base_properties(&mut self) {
    if !self.use_define_for_class_fields {
      return;
    }
    for class in self.index.classes.iter() {
      let Some(base_name) = class.extends.as_deref() else {
        continue;
      };

      for field in class.instance_fields.iter() {
        if field.has_initializer || field.name.starts_with('#') || field.is_private {
          continue;
        }
        if !self
          .index
          .instance_data_prop_declared_in_base_chain(base_name, &field.name)
        {
          continue;
        }
        let end = field
          .key_range
          .start
          .saturating_add(field.name.len() as u32)
          .min(field.key_range.end);
        let range = TextRange::new(field.key_range.start, end);
        self
          .diagnostics
          .push(codes::PROPERTY_WILL_OVERWRITE_BASE_PROPERTY.error(
            format!(
              "Property '{}' will overwrite the base property in '{}'. If this is intentional, add an initializer. Otherwise, add a 'declare' modifier or remove the redundant declaration.",
              field.name, base_name
            ),
            Span::new(self.file, range),
          ));
      }
    }
  }

  fn check_property_not_used_before_initialization(
    &mut self,
    obj: &Node<AstExpr>,
    prop: &str,
    span: TextRange,
  ) {
    let Some(ctx) = self.class_field_initializer else {
      return;
    };
    enum CheckObject {
      This,
      ClassName,
    }

    let obj_kind = match obj.stx.as_ref() {
      AstExpr::This(_) => CheckObject::This,
      AstExpr::Id(id) => {
        // TypeScript also reports TS2729 for `C.x` reads in class field initializers when
        // `C` is the current class name and `x` is a later-declared *static* field.
        //
        // Note: For instance field initializers, this check is only active when targeting
        // native class fields (ES2022/ESNext) with `useDefineForClassFields` enabled.
        let Some(name) = self
          .index
          .classes
          .get(ctx.class_index)
          .and_then(|info| info.name.as_deref())
        else {
          return;
        };
        if id.stx.name != name {
          return;
        }
        if !ctx.is_static && !self.native_define_class_fields {
          return;
        }
        CheckObject::ClassName
      }
      _ => return,
    };

    let check_static = match obj_kind {
      CheckObject::This => ctx.is_static,
      CheckObject::ClassName => true,
    };

    if !self
      .index
      .field_declared_not_before(ctx.class_index, ctx.member_index, prop, check_static)
    {
      return;
    }

    if !ctx.is_static
      && matches!(obj_kind, CheckObject::This)
      && !self.use_define_for_class_fields
      && !prop.starts_with('#')
      && self
        .index
        .instance_prop_declared_in_ancestor(ctx.class_index, prop)
    {
      return;
    }

    let prop_len = prop.len() as u32;
    let start = span.end.saturating_sub(prop_len);
    let prop_span = TextRange::new(start, span.end);
    self
      .diagnostics
      .push(codes::PROPERTY_USED_BEFORE_INITIALIZATION.error(
        format!("Property '{prop}' is used before its initialization."),
        Span::new(self.file, prop_span),
      ));
  }

  fn check_cancelled(&self) {
    if let Some(flag) = self.cancelled {
      if flag.load(Ordering::Relaxed) {
        panic_any(crate::FatalError::Cancelled);
      }
    }
  }

  fn seed_builtins(&mut self) {
    let prim = self.store.primitive_ids();
    self.insert_binding("undefined".to_string(), prim.undefined, Vec::new());
    self.insert_binding("NaN".to_string(), prim.number, Vec::new());
  }

  fn insert_binding(&mut self, name: String, ty: TypeId, type_params: Vec<TypeParamDecl>) {
    if let Some(scope) = self.scopes.last_mut() {
      scope.bindings.insert(name, Binding { ty, type_params });
    }
  }

  fn insert_binding_in_scope(
    &mut self,
    scope_index: usize,
    name: String,
    ty: TypeId,
    type_params: Vec<TypeParamDecl>,
  ) {
    if let Some(scope) = self.scopes.get_mut(scope_index) {
      scope.bindings.insert(name, Binding { ty, type_params });
    }
  }

  fn current_var_scope_index(&self) -> usize {
    self.var_scopes.last().copied().unwrap_or(0)
  }

  fn lookup(&self, name: &str) -> Option<Binding> {
    for scope in self.scopes.iter().rev() {
      if let Some(binding) = scope.bindings.get(name) {
        return Some(binding.clone());
      }
    }
    None
  }

  fn lookup_with_scope(&self, name: &str) -> Option<(usize, Binding)> {
    for idx in (0..self.scopes.len()).rev() {
      if let Some(binding) = self.scopes[idx].bindings.get(name) {
        return Some((idx, binding.clone()));
      }
    }
    None
  }

  fn resolve_single_segment_ref(&self, name: &str, typeof_query: bool) -> Option<TypeId> {
    // `typeof` references point at value namespace definitions. These are not
    // always available for `TypeKind::Ref` expansion in the per-body checker, so
    // prefer a directly bound value type when present.
    if typeof_query {
      if let Some(binding) = self.lookup(name) {
        return Some(binding.ty);
      }
    }
    let resolver = self.type_resolver.as_ref()?;
    let path = [name.to_string()];
    let mut def = if typeof_query {
      resolver.resolve_typeof(&path)?
    } else {
      resolver.resolve_type_name(&path)?
    };
    if typeof_query {
      def = self.value_defs.get(&def).copied().unwrap_or(def);
    }
    Some(self.store.canon(self.store.intern_type(TypeKind::Ref {
      def,
      args: Vec::new(),
    })))
  }

  fn this_super_for_class(&self, class_index: usize, is_static: bool) -> (TypeId, TypeId) {
    let prim = self.store.primitive_ids();
    let Some(info) = self.index.classes.get(class_index) else {
      return (prim.unknown, prim.unknown);
    };
    let Some(name) = info.name.as_deref() else {
      return (prim.unknown, prim.unknown);
    };
    let this_ty = self
      .resolve_single_segment_ref(name, is_static)
      .unwrap_or(prim.unknown);
    let super_ty = info
      .extends
      .as_deref()
      .and_then(|base| self.resolve_single_segment_ref(base, is_static))
      .unwrap_or(prim.unknown);
    (this_ty, super_ty)
  }

  fn explicit_this_param_type(&mut self, func: &Node<Func>) -> Option<TypeId> {
    let first = func.stx.parameters.first()?;
    let AstPat::Id(id) = first.stx.pattern.stx.pat.stx.as_ref() else {
      return None;
    };
    if id.stx.name != "this" {
      return None;
    }
    first
      .stx
      .type_annotation
      .as_ref()
      .map(|ann| self.lowerer.lower_type_expr(ann))
  }

  fn enclosing_function(
    &self,
    span: TextRange,
    strict: bool,
    within: Option<TextRange>,
  ) -> Option<FunctionInfo> {
    let mut best: Option<FunctionInfo> = None;
    for (idx, func) in self.index.functions.iter().copied().enumerate() {
      if idx % 2048 == 0 {
        self.check_cancelled();
      }
      let contains = if strict {
        func.func_span.start < span.start && func.func_span.end > span.end
      } else {
        func.func_span.start <= span.start && func.func_span.end >= span.end
      };
      if !contains {
        continue;
      }
      if let Some(within) = within {
        if !contains_range(within, func.func_span) {
          continue;
        }
      }
      let len = func.func_span.end.saturating_sub(func.func_span.start);
      let replace = match best {
        Some(existing) => {
          let existing_len = existing
            .func_span
            .end
            .saturating_sub(existing.func_span.start);
          len < existing_len
        }
        None => true,
      };
      if replace {
        best = Some(func);
      }
    }
    best
  }

  fn this_super_for_span(&mut self, span: TextRange) -> (TypeId, TypeId) {
    let prim = self.store.primitive_ids();

    // If we're within a class evaluation context (static blocks / field
    // initializers), that context provides `this` unless shadowed by a nested
    // non-arrow function.
    let mut class_context: Option<(TextRange, usize, bool)> = None;
    if let Some(block) = self.index.enclosing_class_static_block(span) {
      class_context = Some((block.span, block.class_index, true));
    }
    if let Some((init_span, info)) = self.index.enclosing_class_field_initializer(span) {
      let cand = (init_span, info.class_index, info.is_static);
      class_context = match class_context {
        Some(existing) => {
          let existing_len = existing.0.end.saturating_sub(existing.0.start);
          let cand_len = init_span.end.saturating_sub(init_span.start);
          if cand_len < existing_len {
            Some(cand)
          } else {
            Some(existing)
          }
        }
        None => Some(cand),
      };
    }

    let within = class_context.as_ref().map(|(span, _, _)| *span);

    // Prefer the nearest non-arrow function's `this` binding. Arrow functions
    // lexically capture the nearest enclosing non-arrow `this` provider. When we
    // are inside a class evaluation context, ignore enclosing functions outside
    // that context; those do not provide `this` for the class body.
    let mut current = span;
    let mut strict = false;
    while let Some(func) = self.enclosing_function(current, strict, within) {
      let func_node = unsafe { &*func.func };
      if func.is_arrow {
        current = func.func_span;
        strict = true;
        continue;
      }

      let explicit_this = self.explicit_this_param_type(func_node);
      let (mut this_ty, super_ty) =
        if let Some(ctx) = func.class_member {
          // Constructors currently share the same `this`/`super` model as other
          // instance members.
          let _is_constructor = matches!(ctx.kind, MemberKind::Constructor);
          self.this_super_for_class(ctx.class_index, ctx.is_static)
        } else {
          (prim.unknown, prim.unknown)
        };
      if let Some(explicit) = explicit_this {
        this_ty = explicit;
      }
      return (this_ty, super_ty);
    }

    if let Some((_, class_index, is_static)) = class_context {
      return self.this_super_for_class(class_index, is_static);
    }

    (prim.unknown, prim.unknown)
  }

  fn super_ctor_for_span(&self, span: TextRange) -> TypeId {
    if self.index.enclosing_class_field_initializer(span).is_some() {
      return self.store.primitive_ids().unknown;
    }
    let prim = self.store.primitive_ids();
    let mut current = span;
    let mut strict = false;
    while let Some(func) = self.enclosing_function(current, strict, None) {
      if func.is_arrow {
        current = func.func_span;
        strict = true;
        continue;
      }
      let Some(ctx) = self.index.class_member_function(func.func_span) else {
        return prim.unknown;
      };
      if !ctx.is_constructor {
        return prim.unknown;
      }
      let Some(info) = self.index.classes.get(ctx.class_index) else {
        return prim.unknown;
      };
      let Some(base) = info.extends.as_deref() else {
        return prim.unknown;
      };
      return self
        .resolve_single_segment_ref(base, true)
        .unwrap_or(prim.unknown);
    }

    prim.unknown
  }

  fn check_enclosing_function(&mut self, body_range: TextRange) -> bool {
    let mut best: Option<FunctionInfo> = None;
    for (idx, func) in self.index.functions.iter().copied().enumerate() {
      if idx % 2048 == 0 {
        self.check_cancelled();
      }
      let contains =
        func.func_span.start <= body_range.start && func.func_span.end >= body_range.end;
      if !contains {
        continue;
      }
      let len = func.func_span.end.saturating_sub(func.func_span.start);
      let replace = match best {
        Some(existing) => {
          let existing_len = existing
            .func_span
            .end
            .saturating_sub(existing.func_span.start);
          len < existing_len
        }
        None => true,
      };
      if replace {
        best = Some(func);
      }
    }
    if let Some(func) = best {
      self.check_cancelled();
      let func_node = unsafe { &*func.func };
      let prev_return = self.expected_return;
      let prev_async = self.in_async_function;
      let (prev_this, prev_super, prev_super_ctor) = (
        self.current_this_ty,
        self.current_super_ty,
        self.current_super_ctor_ty,
      );
      let mut type_param_decls = Vec::new();
      let mut has_type_params = false;
      if let Some(params) = func_node.stx.type_parameters.as_ref() {
        self.lowerer.push_type_param_scope();
        has_type_params = true;
        type_param_decls = self.lower_type_params(params);
        self.type_param_scopes.push(type_param_decls.clone());
      }
      let (this_ty, super_ty) = self.this_super_for_span(body_range);
      self.current_this_ty = this_ty;
      self.current_super_ty = super_ty;
      self.current_super_ctor_ty = self.super_ctor_for_span(body_range);
      let prim = self.store.primitive_ids();

      let mut contextual_sig = self.contextual_signature();
      let explicit_this_ty = func_node
        .stx
        .parameters
        .first()
        .and_then(|param| {
          matches!(
            param.stx.pattern.stx.pat.stx.as_ref(),
            AstPat::Id(id) if id.stx.name == "this"
          )
          .then(|| {
            param
              .stx
              .type_annotation
              .as_ref()
              .map(|ann| self.lowerer.lower_type_expr(ann))
          })
          .flatten()
        });
      let contextual_this_ty = contextual_sig
        .as_ref()
        .and_then(|sig| sig.this_param)
        .filter(|ty| *ty != prim.unknown);
      if let Some(this_ty) = explicit_this_ty.or(contextual_this_ty) {
        self.current_this_ty = this_ty;
      }

      if let Some(instantiated) = contextual_sig
        .as_ref()
        .and_then(|sig| self.instantiate_generic_contextual_signature_for_function_expr(func_node, sig))
      {
        contextual_sig = Some(instantiated);
        // If a `this` type came from the contextual signature, refresh it after
        // instantiation so the body check sees the instantiated `this` type.
        if explicit_this_ty.is_none() {
          if let Some(this_ty) = contextual_sig
            .as_ref()
            .and_then(|sig| sig.this_param)
            .filter(|ty| *ty != prim.unknown)
          {
            self.current_this_ty = this_ty;
          }
        }
      }

      let pushed_contextual_type_params = if has_type_params {
        false
      } else if let Some(sig) = contextual_sig.as_ref() {
        if sig.type_params.is_empty() {
          false
        } else {
          self.type_param_scopes.push(sig.type_params.clone());
          true
        }
      } else {
        false
      };
      let annotated_return = func_node
        .stx
        .return_type
        .as_ref()
        .map(|ret| self.lowerer.lower_type_expr(ret));
      self.in_async_function = func_node.stx.async_;
      let mut expected = annotated_return.or_else(|| contextual_sig.as_ref().map(|sig| sig.ret));
      if func_node.stx.async_ {
        expected = expected.map(|ty| awaited_type(self.store.as_ref(), ty, self.ref_expander));
      }
      self.expected_return = expected;
      self.bind_params(func_node, &type_param_decls, contextual_sig.as_ref());
      self.check_function_body(func_node);
      if pushed_contextual_type_params {
        self.type_param_scopes.pop();
      }
      if has_type_params {
        self.type_param_scopes.pop();
        self.lowerer.pop_type_param_scope();
      }
      self.expected_return = prev_return;
      self.in_async_function = prev_async;
      self.current_this_ty = prev_this;
      self.current_super_ty = prev_super;
      self.current_super_ctor_ty = prev_super_ctor;
      return true;
    }
    false
  }

  fn check_enclosing_class(&mut self, body_range: TextRange) -> bool {
    if body_range.start == body_range.end {
      // Empty class bodies (no static blocks) still count as checked.
      return true;
    }
    let mut matched_blocks: Vec<ClassStaticBlockInfo> = Vec::new();
    for (idx, block) in self.index.class_static_blocks.iter().copied().enumerate() {
      if idx % 256 == 0 {
        self.check_cancelled();
      }
      if ranges_overlap(body_range, block.span) || contains_range(block.span, body_range) {
        matched_blocks.push(block);
      }
    }
    matched_blocks.sort_by_key(|block| (block.span.start, block.span.end));
    matched_blocks.dedup_by_key(|block| (block.span.start, block.span.end, block.block));
    let static_block_spans: Vec<TextRange> =
      matched_blocks.iter().map(|block| block.span).collect();

    for block in matched_blocks.iter() {
      self.check_cancelled();
      let block_node = unsafe { &*block.block };
      let prev_ctx = self.class_field_initializer;
      self.class_field_initializer = Some(ClassFieldInitializerContext {
        class_index: block.class_index,
        member_index: block.member_index,
        is_static: true,
      });
      let (prev_this, prev_super, prev_super_ctor) = (
        self.current_this_ty,
        self.current_super_ty,
        self.current_super_ctor_ty,
      );
      let (this_ty, super_ty) = self.this_super_for_class(block.class_index, true);
      self.current_this_ty = this_ty;
      self.current_super_ty = super_ty;
      self.current_super_ctor_ty = self.store.primitive_ids().unknown;
      self.check_block_body(&block_node.stx.body);
      self.current_this_ty = prev_this;
      self.current_super_ty = prev_super;
      self.current_super_ctor_ty = prev_super_ctor;
      self.class_field_initializer = prev_ctx;
    }

    // Class bodies can contain additional evaluation expressions (e.g. decorators,
    // `extends` expressions, computed property names). These expressions are
    // lowered into the class body HIR but are not part of any static block.
    // Type-check them directly, skipping any spans that are covered by a static
    // block so we preserve static block scoping rules.
    let mut checked_any = !matched_blocks.is_empty();
    let expr_spans = self.expr_spans.clone();
    for span in expr_spans {
      if static_block_spans
        .iter()
        .any(|block_span| contains_range(*block_span, span))
      {
        continue;
      }
      let Some(expr) = self.index.exprs.get(&span).copied() else {
        continue;
      };
      self.check_cancelled();
      let expr = unsafe { &*expr };
      let prev_props = self.current_class_field_param_props;
      self.current_class_field_param_props = self
        .index
        .class_field_param_props
        .get(&span)
        .map(|props| props.as_slice());
      let prev_field_init = self.class_field_initializer;
      self.class_field_initializer =
        self
          .index
          .class_field_initializer(span)
          .map(|info| ClassFieldInitializerContext {
            class_index: info.class_index,
            member_index: info.member_index,
            is_static: info.is_static,
          });
      let (prev_this, prev_super, prev_super_ctor) = (
        self.current_this_ty,
        self.current_super_ty,
        self.current_super_ctor_ty,
      );
      if let Some(ctx) = self.class_field_initializer {
        let (this_ty, super_ty) = self.this_super_for_class(ctx.class_index, ctx.is_static);
        self.current_this_ty = this_ty;
        self.current_super_ty = super_ty;
        self.current_super_ctor_ty = self.store.primitive_ids().unknown;
      }
      let _ = self.check_expr(expr);
      self.current_this_ty = prev_this;
      self.current_super_ty = prev_super;
      self.current_super_ctor_ty = prev_super_ctor;
      self.class_field_initializer = prev_field_init;
      self.current_class_field_param_props = prev_props;
      checked_any = true;
    }

    checked_any
  }

  fn check_matching_initializer(&mut self, body_range: TextRange) -> bool {
    let mut best: Option<(u32, TextRange, VarInfo)> = None;
    for (span, info) in self.index.vars.iter() {
      if let Some(init) = info.initializer {
        let init = unsafe { &*init };
        let init_range = loc_to_range(self.file, init.loc);
        if !ranges_overlap(init_range, body_range) && !contains_range(body_range, init_range) {
          continue;
        }
        let len = init_range.end.saturating_sub(init_range.start);
        let replace = match best {
          Some((best_len, best_span, _)) => {
            len < best_len || (len == best_len && span.start < best_span.start)
          }
          None => true,
        };
        if replace {
          best = Some((len, *span, *info));
        }
      }
    }
    if let Some((_len, pat_span, info)) = best {
      self.check_cancelled();
      let mut has_type_params = false;
      if let Some(init) = info.initializer {
        let init = unsafe { &*init };
        let init_range = loc_to_range(self.file, init.loc);
        // Initializer bodies can be nested inside functions. Bind any enclosing
        // function parameters so references like `const x = param;` do not emit
        // spurious `unknown identifier` diagnostics when the initializer is
        // type-checked in isolation.
        has_type_params = self.bind_enclosing_params(init_range);
        let annotation = info
          .type_annotation
          .map(|ann| unsafe { &*ann })
          .map(|ann| self.lowerer.lower_type_expr(ann));
        let (prev_this, prev_super, prev_super_ctor) = (
          self.current_this_ty,
          self.current_super_ty,
          self.current_super_ctor_ty,
        );
        let (this_ty, super_ty) = self.this_super_for_span(init_range);
        self.current_this_ty = this_ty;
        self.current_super_ty = super_ty;
        self.current_super_ctor_ty = self.super_ctor_for_span(init_range);
        let init_ty = match annotation {
          Some(expected) => self.check_expr_with_expected(init, expected),
          None => self.check_expr(init),
        };
        self.current_this_ty = prev_this;
        self.current_super_ty = prev_super;
        self.current_super_ctor_ty = prev_super_ctor;
        if let Some(annotation) = annotation {
          // Mirror `check_var_decl`: anchor assignment diagnostics on the binding
          // name/pattern to match tsc and dedupe with the top-level var check.
          let range_override = self
            .index
            .pats
            .get(&pat_span)
            .copied()
            .map(|pat| self.binding_name_range(unsafe { &*pat }))
            .unwrap_or(pat_span);
          self.check_assignable(init, init_ty, annotation, Some(range_override));
        }
        let prim = self.store.primitive_ids();
        let binding_ty = match info.mode {
          VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing => init_ty,
          _ => self.base_type(init_ty),
        };
        let mut ty = annotation.unwrap_or(binding_ty);
        if self.no_implicit_any && annotation.is_none() && ty == prim.unknown {
          if let Some(pat) = self.index.pats.get(&pat_span).copied() {
            let pat = unsafe { &*pat };
            self.report_implicit_any_in_pat(pat);
          } else {
            self.report_implicit_any(pat_span, None);
          }
          ty = prim.any;
        }
        if let Some(pat) = self.index.pats.get(&pat_span).copied() {
          let pat = unsafe { &*pat };
          self.bind_pattern(pat, ty);
        }
      }
      if has_type_params {
        self.type_param_scopes.pop();
        self.lowerer.pop_type_param_scope();
      }
      return true;
    }

    // Fallback for initializer bodies that are not var declarators (e.g. class
    // field initializers). Match the tightest expression containing the body
    // range and type-check it.
    let mut best_expr: Option<(u32, TextRange, *const Node<AstExpr>)> = None;
    for (span, expr) in self.index.exprs.iter() {
      let span = *span;
      if !contains_range(span, body_range) {
        continue;
      }
      let len = span.end.saturating_sub(span.start);
      let replace = match best_expr {
        Some((best_len, best_span, _)) => {
          len < best_len || (len == best_len && span.start < best_span.start)
        }
        None => true,
      };
      if replace {
        best_expr = Some((len, span, *expr));
      }
    }
    if let Some((_len, _span, expr)) = best_expr {
      self.check_cancelled();
      let expr = unsafe { &*expr };
      let prev_props = self.current_class_field_param_props;
      self.current_class_field_param_props = self.index.enclosing_class_field_param_props(body_range);
      let prev_field_init = self.class_field_initializer;
      self.class_field_initializer = self
        .index
        .enclosing_class_field_initializer(body_range)
        .map(|(_, info)| ClassFieldInitializerContext {
          class_index: info.class_index,
          member_index: info.member_index,
          is_static: info.is_static,
        });
      let (prev_this, prev_super, prev_super_ctor) = (
        self.current_this_ty,
        self.current_super_ty,
        self.current_super_ctor_ty,
      );
      let (this_ty, super_ty) = self.this_super_for_span(body_range);
      self.current_this_ty = this_ty;
      self.current_super_ty = super_ty;
      self.current_super_ctor_ty = self.super_ctor_for_span(body_range);
      let _ = self.check_expr(expr);
      self.current_this_ty = prev_this;
      self.current_super_ty = prev_super;
      self.current_super_ctor_ty = prev_super_ctor;
      self.class_field_initializer = prev_field_init;
      self.current_class_field_param_props = prev_props;
      return true;
    }
    false
  }

  /// Bind the closest enclosing function's parameters into the current scope.
  ///
  /// Returns `true` when a type parameter scope was pushed and must be popped by
  /// the caller.
  fn bind_enclosing_params(&mut self, span: TextRange) -> bool {
    let mut best: Option<FunctionInfo> = None;
    for (idx, func) in self.index.functions.iter().copied().enumerate() {
      if idx % 2048 == 0 {
        self.check_cancelled();
      }
      let strictly_contains = func.func_span.start < span.start && func.func_span.end > span.end;
      if !strictly_contains {
        continue;
      }
      let len = func.func_span.end.saturating_sub(func.func_span.start);
      let replace = match best {
        Some(existing) => {
          let existing_len = existing
            .func_span
            .end
            .saturating_sub(existing.func_span.start);
          len < existing_len
        }
        None => true,
      };
      if replace {
        best = Some(func);
      }
    }

    let Some(func) = best else {
      return false;
    };
    let func_node = unsafe { &*func.func };
    let prim = self.store.primitive_ids();

    let mut type_param_decls = Vec::new();
    let mut has_type_params = false;
    if let Some(params) = func_node.stx.type_parameters.as_ref() {
      self.lowerer.push_type_param_scope();
      has_type_params = true;
      type_param_decls = self.lower_type_params(params);
      self.type_param_scopes.push(type_param_decls.clone());
    }
    fn bind_missing(
      checker: &mut Checker<'_>,
      pat: &Node<AstPat>,
      ty: TypeId,
      type_params: &[TypeParamDecl],
    ) {
      match pat.stx.as_ref() {
        AstPat::Id(id) => {
          if checker.lookup(&id.stx.name).is_none() {
            checker.insert_binding(id.stx.name.clone(), ty, type_params.to_vec());
          }
        }
        AstPat::Arr(arr) => {
          for elem in arr.stx.elements.iter().flatten() {
            bind_missing(checker, &elem.target, ty, type_params);
          }
          if let Some(rest) = &arr.stx.rest {
            bind_missing(checker, rest, ty, type_params);
          }
        }
        AstPat::Obj(obj) => {
          for prop in obj.stx.properties.iter() {
            bind_missing(checker, &prop.stx.target, ty, type_params);
          }
          if let Some(rest) = &obj.stx.rest {
            bind_missing(checker, rest, ty, type_params);
          }
        }
        AstPat::AssignTarget(_) => {}
      }
    }
    for param in func_node.stx.parameters.iter() {
      bind_missing(
        self,
        &param.stx.pattern.stx.pat,
        prim.unknown,
        &type_param_decls,
      );
    }
    has_type_params
  }

  fn bind_params(
    &mut self,
    func: &Node<Func>,
    type_param_decls: &[TypeParamDecl],
    contextual_sig: Option<&Signature>,
  ) {
    let prim = self.store.primitive_ids();
    let has_explicit_this = func.stx.parameters.first().is_some_and(|param| {
      matches!(
        param.stx.pattern.stx.pat.stx.as_ref(),
        AstPat::Id(id) if id.stx.name == "this"
      )
    });
    for (idx, param) in func.stx.parameters.iter().enumerate() {
      if idx % 64 == 0 {
        self.check_cancelled();
      }
      let annotation = param
        .stx
        .type_annotation
        .as_ref()
        .map(|ann| self.lowerer.lower_type_expr(ann));
      let default_ty = param.stx.default_value.as_ref().map(|d| self.check_expr(d));
      let contextual_param_ty = contextual_sig
        .and_then(|sig| {
          let sig_idx = if has_explicit_this {
            if idx == 0 {
              return None;
            }
            idx - 1
          } else {
            idx
          };
          if let Some(param_sig) = sig.params.get(sig_idx) {
            if param_sig.rest && !param.stx.rest {
              let elem_ty = self.spread_element_type(param_sig.ty);
              return Some(elem_ty);
            }
            return Some(param_sig.ty);
          }

          let rest = sig
            .params
            .iter()
            .enumerate()
            .find(|(_, param)| param.rest)
            .filter(|(rest_idx, _)| sig_idx >= *rest_idx);
          rest.map(|(_, rest_param)| {
            if param.stx.rest {
              rest_param.ty
            } else {
              self.spread_element_type(rest_param.ty)
            }
          })
        })
        // Treat `unknown` contextual parameter types as absent so `--noImplicitAny`
        // still reports implicit `any` for untyped parameters when the surrounding
        // context doesn't provide a real type (e.g. uncontextualized arrow
        // functions).
        .filter(|ty| *ty != prim.unknown);
      let is_this = idx == 0
        && matches!(
          param.stx.pattern.stx.pat.stx.as_ref(),
          AstPat::Id(id) if id.stx.name == "this"
        );
      let implicit_any =
        self.no_implicit_any && !is_this && annotation.is_none() && contextual_param_ty.is_none();
      let mut ty = annotation
        .or(contextual_param_ty)
        .unwrap_or(if implicit_any { prim.any } else { prim.unknown });
      if implicit_any {
        let range = loc_to_range(self.file, param.stx.pattern.stx.pat.loc);
        let name = match param.stx.pattern.stx.pat.stx.as_ref() {
          AstPat::Id(id) => Some(id.stx.name.as_str()),
          _ => None,
        };
        self.report_implicit_any(range, name);
      }
      if let Some(default) = default_ty {
        ty = self.store.union(vec![ty, default]);
      }
      // Bind parameters directly from the AST node. Function parameters must be
      // in scope for identifier resolution while checking the body; relying on
      // `AstIndex` span lookups here can be brittle when spans differ between
      // HIR lowering and parse-js nodes.
      self.bind_pattern_with_type_params(
        &param.stx.pattern.stx.pat,
        ty,
        type_param_decls.to_vec(),
      );
    }
  }

  fn lower_type_params(
    &mut self,
    params: &[Node<parse_js::ast::type_expr::TypeParameter>],
  ) -> Vec<TypeParamDecl> {
    self.lowerer.register_type_params(params)
  }

  fn check_function_body(&mut self, func: &Node<Func>) {
    match &func.stx.body {
      Some(FuncBody::Block(block)) => {
        self.hoist_function_decls_in_stmt_list(block);
        self.hoist_var_decls_in_stmt_tree(block);
        self.check_stmt_list(block);
      }
      Some(FuncBody::Expression(expr)) => {
        let expr_ty = match self.expected_return {
          Some(expected) => self.check_expr_with_expected(expr, expected),
          None => self.check_expr(expr),
        };
        let ty = if self.in_async_function {
          awaited_type(self.store.as_ref(), expr_ty, self.ref_expander)
        } else {
          expr_ty
        };
        if let Some(expected) = self.expected_return {
          self.check_assignable(expr, ty, expected, None);
        }
        self.return_types.push(ty);
      }
      None => {}
    }
  }

  fn contextual_signature(&self) -> Option<Signature> {
    let ty = self.contextual_fn_ty?;
    self.first_callable_signature(ty)
  }

  fn bind_function_decl_name(&mut self, name_str: String, fn_ty: TypeId) {
    if let Some(existing) = self.lookup(&name_str) {
      let has_callables = !callable_signatures(self.store.as_ref(), existing.ty).is_empty();
      let ty = if has_callables {
        existing.ty
      } else {
        self.store.intersection(vec![existing.ty, fn_ty])
      };
      self.insert_binding(name_str, ty, Vec::new());
    } else {
      self.insert_binding(name_str, fn_ty, Vec::new());
    }
  }

  fn hoist_function_decls_in_stmt_list(&mut self, stmts: &[Node<Stmt>]) {
    for stmt in stmts {
      let Stmt::FunctionDecl(func) = stmt.stx.as_ref() else {
        continue;
      };
      let Some(name) = func.stx.name.as_ref() else {
        continue;
      };
      let name_str = name.stx.name.clone();
      let fn_ty = self.function_type(&func.stx.function);
      self.bind_function_decl_name(name_str, fn_ty);
    }
  }

  fn hoist_var_decls_in_stmt_tree(&mut self, stmts: &[Node<Stmt>]) {
    let mut names: HashSet<String> = HashSet::new();
    for (idx, stmt) in stmts.iter().enumerate() {
      if idx % 128 == 0 {
        self.check_cancelled();
      }
      self.collect_var_decl_names_in_stmt(stmt, &mut names);
    }

    let prim = self.store.primitive_ids();
    let var_scope = self.current_var_scope_index();
    for name in names {
      if self
        .scopes
        .get(var_scope)
        .is_some_and(|scope| scope.bindings.contains_key(&name))
      {
        continue;
      }
      self.insert_binding_in_scope(var_scope, name, prim.unknown, Vec::new());
    }
  }

  fn collect_var_decl_names_in_stmt(&self, stmt: &Node<Stmt>, names: &mut HashSet<String>) {
    match stmt.stx.as_ref() {
      Stmt::Block(block) => {
        for stmt in block.stx.body.iter() {
          self.collect_var_decl_names_in_stmt(stmt, names);
        }
      }
      Stmt::If(if_stmt) => {
        self.collect_var_decl_names_in_stmt(&if_stmt.stx.consequent, names);
        if let Some(alt) = if_stmt.stx.alternate.as_ref() {
          self.collect_var_decl_names_in_stmt(alt, names);
        }
      }
      Stmt::While(while_stmt) => {
        self.collect_var_decl_names_in_stmt(&while_stmt.stx.body, names);
      }
      Stmt::DoWhile(do_while) => {
        self.collect_var_decl_names_in_stmt(&do_while.stx.body, names);
      }
      Stmt::ForTriple(for_stmt) => {
        use parse_js::ast::stmt::ForTripleStmtInit;
        if let ForTripleStmtInit::Decl(decl) = &for_stmt.stx.init {
          if matches!(decl.stx.mode, VarDeclMode::Var) {
            for declarator in decl.stx.declarators.iter() {
              self.collect_var_decl_names_in_pat(&declarator.pattern.stx.pat, names);
            }
          }
        }
        for stmt in for_stmt.stx.body.stx.body.iter() {
          self.collect_var_decl_names_in_stmt(stmt, names);
        }
      }
      Stmt::ForIn(for_in) => {
        use parse_js::ast::stmt::ForInOfLhs;
        if let ForInOfLhs::Decl((mode, decl)) = &for_in.stx.lhs {
          if matches!(*mode, VarDeclMode::Var) {
            self.collect_var_decl_names_in_pat(&decl.stx.pat, names);
          }
        }
        for stmt in for_in.stx.body.stx.body.iter() {
          self.collect_var_decl_names_in_stmt(stmt, names);
        }
      }
      Stmt::ForOf(for_of) => {
        use parse_js::ast::stmt::ForInOfLhs;
        if let ForInOfLhs::Decl((mode, decl)) = &for_of.stx.lhs {
          if matches!(*mode, VarDeclMode::Var) {
            self.collect_var_decl_names_in_pat(&decl.stx.pat, names);
          }
        }
        for stmt in for_of.stx.body.stx.body.iter() {
          self.collect_var_decl_names_in_stmt(stmt, names);
        }
      }
      Stmt::Switch(sw) => {
        for branch in sw.stx.branches.iter() {
          for stmt in branch.stx.body.iter() {
            self.collect_var_decl_names_in_stmt(stmt, names);
          }
        }
      }
      Stmt::Try(tr) => {
        for stmt in tr.stx.wrapped.stx.body.iter() {
          self.collect_var_decl_names_in_stmt(stmt, names);
        }
        if let Some(catch) = tr.stx.catch.as_ref() {
          for stmt in catch.stx.body.iter() {
            self.collect_var_decl_names_in_stmt(stmt, names);
          }
        }
        if let Some(finally) = tr.stx.finally.as_ref() {
          for stmt in finally.stx.body.iter() {
            self.collect_var_decl_names_in_stmt(stmt, names);
          }
        }
      }
      Stmt::Label(label) => self.collect_var_decl_names_in_stmt(&label.stx.statement, names),
      Stmt::With(w) => {
        self.collect_var_decl_names_in_stmt(&w.stx.body, names);
      }
      Stmt::VarDecl(decl) => {
        if matches!(decl.stx.mode, VarDeclMode::Var) {
          for declarator in decl.stx.declarators.iter() {
            self.collect_var_decl_names_in_pat(&declarator.pattern.stx.pat, names);
          }
        }
      }

      // `var`-hoisting stops at function/class bodies and namespace blocks.
      Stmt::FunctionDecl(_) | Stmt::ClassDecl(_) | Stmt::NamespaceDecl(_) => {}

      Stmt::ModuleDecl(module) => {
        if let Some(body) = module.stx.body.as_ref() {
          for stmt in body.iter() {
            self.collect_var_decl_names_in_stmt(stmt, names);
          }
        }
      }
      Stmt::GlobalDecl(global) => {
        for stmt in global.stx.body.iter() {
          self.collect_var_decl_names_in_stmt(stmt, names);
        }
      }

      _ => {}
    }
  }

  fn collect_var_decl_names_in_pat(&self, pat: &Node<AstPat>, names: &mut HashSet<String>) {
    match pat.stx.as_ref() {
      AstPat::Id(id) => {
        names.insert(id.stx.name.clone());
      }
      AstPat::Arr(arr) => {
        for elem in arr.stx.elements.iter().flatten() {
          self.collect_var_decl_names_in_pat(&elem.target, names);
        }
        if let Some(rest) = &arr.stx.rest {
          self.collect_var_decl_names_in_pat(rest, names);
        }
      }
      AstPat::Obj(obj) => {
        for prop in obj.stx.properties.iter() {
          self.collect_var_decl_names_in_pat(&prop.stx.target, names);
        }
        if let Some(rest) = &obj.stx.rest {
          self.collect_var_decl_names_in_pat(rest, names);
        }
      }
      AstPat::AssignTarget(_) => {}
    }
  }

  fn check_stmt_list(&mut self, stmts: &[Node<Stmt>]) {
    self.hoist_function_decls_in_stmt_list(stmts);
    for (idx, stmt) in stmts.iter().enumerate() {
      if idx % 128 == 0 {
        self.check_cancelled();
      }
      self.check_stmt(stmt);
    }
  }

  fn check_stmt(&mut self, stmt: &Node<Stmt>) {
    match stmt.stx.as_ref() {
      Stmt::Expr(expr_stmt) => {
        self.check_expr(&expr_stmt.stx.expr);
      }
      Stmt::ExportDefaultExpr(default_expr) => {
        self.check_expr(&default_expr.stx.expression);
      }
      Stmt::Return(ret) => {
        let prim = self.store.primitive_ids();
        let expr_ty = match (self.expected_return, ret.stx.value.as_ref()) {
          (Some(expected), Some(value)) => self.check_expr_with_expected(value, expected),
          (None, Some(value)) => self.check_expr(value),
          (_, None) => prim.undefined,
        };
        let ty = if self.in_async_function {
          awaited_type(self.store.as_ref(), expr_ty, self.ref_expander)
        } else {
          expr_ty
        };
        if let (Some(expected), Some(value)) = (self.expected_return, ret.stx.value.as_ref()) {
          self.check_assignable(value, ty, expected, None);
        }
        self.return_types.push(ty);
      }
      Stmt::Block(block) => {
        self.scopes.push(Scope::default());
        self.check_stmt_list(&block.stx.body);
        self.scopes.pop();
      }
      Stmt::If(if_stmt) => {
        self.check_expr(&if_stmt.stx.test);
        self.scopes.push(Scope::default());
        self.check_stmt(&if_stmt.stx.consequent);
        self.scopes.pop();
        if let Some(alt) = &if_stmt.stx.alternate {
          self.scopes.push(Scope::default());
          self.check_stmt(alt);
          self.scopes.pop();
        }
      }
      Stmt::While(while_stmt) => {
        self.check_expr(&while_stmt.stx.condition);
        self.scopes.push(Scope::default());
        self.check_stmt(&while_stmt.stx.body);
        self.scopes.pop();
      }
      Stmt::DoWhile(do_while) => {
        self.scopes.push(Scope::default());
        self.check_stmt(&do_while.stx.body);
        self.scopes.pop();
        self.check_expr(&do_while.stx.condition);
      }
      Stmt::ForTriple(for_stmt) => {
        use parse_js::ast::stmt::ForTripleStmtInit;
        self.scopes.push(Scope::default());
        match &for_stmt.stx.init {
          ForTripleStmtInit::Expr(expr) => {
            self.check_expr(expr);
          }
          ForTripleStmtInit::Decl(decl) => {
            self.check_var_decl(decl);
          }
          ForTripleStmtInit::None => {}
        }
        if let Some(cond) = &for_stmt.stx.cond {
          self.check_expr(cond);
        }
        if let Some(post) = &for_stmt.stx.post {
          self.check_expr(post);
        }
        self.check_block_body(&for_stmt.stx.body.stx.body);
        self.scopes.pop();
      }
      Stmt::ForIn(for_in) => {
        use parse_js::ast::stmt::ForInOfLhs;
        self.scopes.push(Scope::default());
        match &for_in.stx.lhs {
          ForInOfLhs::Assign(pat) => {
            self.check_pat(pat, self.store.primitive_ids().unknown);
          }
          ForInOfLhs::Decl((mode, pat_decl)) => {
            let ty = self.store.primitive_ids().unknown;
            if matches!(mode, VarDeclMode::Var) {
              let var_scope = self.current_var_scope_index();
              self.bind_pattern_in_scope(&pat_decl.stx.pat, ty, var_scope);
            } else {
              self.check_pat(&pat_decl.stx.pat, ty);
            }
          }
        }
        self.check_expr(&for_in.stx.rhs);
        self.check_block_body(&for_in.stx.body.stx.body);
        self.scopes.pop();
      }
      Stmt::ForOf(for_of) => {
        use parse_js::ast::stmt::ForInOfLhs;
        self.scopes.push(Scope::default());
        match &for_of.stx.lhs {
          ForInOfLhs::Assign(pat) => {
            self.check_pat(pat, self.store.primitive_ids().unknown);
          }
          ForInOfLhs::Decl((mode, pat_decl)) => {
            let ty = self.store.primitive_ids().unknown;
            if matches!(mode, VarDeclMode::Var) {
              let var_scope = self.current_var_scope_index();
              self.bind_pattern_in_scope(&pat_decl.stx.pat, ty, var_scope);
            } else {
              self.check_pat(&pat_decl.stx.pat, ty);
            }
          }
        }
        self.check_expr(&for_of.stx.rhs);
        self.check_block_body(&for_of.stx.body.stx.body);
        self.scopes.pop();
      }
      Stmt::Switch(sw) => {
        let _ = self.check_expr(&sw.stx.test);
        for branch in sw.stx.branches.iter() {
          if let Some(case) = &branch.stx.case {
            self.check_expr(case);
          }
          self.scopes.push(Scope::default());
          self.check_stmt_list(&branch.stx.body);
          self.scopes.pop();
        }
      }
      Stmt::Try(tr) => {
        self.check_block_body(&tr.stx.wrapped.stx.body);
        if let Some(catch) = &tr.stx.catch {
          self.scopes.push(Scope::default());
          if let Some(param) = &catch.stx.parameter {
            self.check_pat(&param.stx.pat, self.store.primitive_ids().unknown);
          }
          self.check_block_body(&catch.stx.body);
          self.scopes.pop();
        }
        if let Some(finally) = &tr.stx.finally {
          self.check_block_body(&finally.stx.body);
        }
      }
      Stmt::Throw(th) => {
        self.check_expr(&th.stx.value);
      }
      Stmt::Label(label) => {
        self.check_stmt(&label.stx.statement);
      }
      Stmt::With(w) => {
        self.check_expr(&w.stx.object);
        self.scopes.push(Scope::default());
        self.check_stmt(&w.stx.body);
        self.scopes.pop();
      }
      Stmt::VarDecl(decl) => self.check_var_decl(decl),
      Stmt::FunctionDecl(func) => {
        // Function declarations are handled by separate bodies; bind the name for call sites in
        // this body, but preserve any pre-seeded binding (e.g. merged namespace + value types)
        // by intersecting rather than overwriting.
        if let Some(name) = func.stx.name.as_ref() {
          let name_str = name.stx.name.clone();
          if let Some(existing) = self.lookup(&name_str) {
            let has_callables = !callable_signatures(self.store.as_ref(), existing.ty).is_empty();
            if has_callables {
              // Avoid calling `function_type` here when the declaration surface already provided
              // callable signatures for this symbol. `function_type` can (transitively) check the
              // function body for inference, which can produce spurious errors during top-level
              // checking.
              self.insert_binding(name_str, existing.ty, Vec::new());
            } else {
              let fn_ty = self.function_type(&func.stx.function);
              self.bind_function_decl_name(name_str, fn_ty);
            }
          } else {
            let fn_ty = self.function_type(&func.stx.function);
            self.bind_function_decl_name(name_str, fn_ty);
          }
        }
      }
      Stmt::ClassDecl(class_decl) => {
        if let Some(name) = class_decl.stx.name.as_ref() {
          let name_str = name.stx.name.clone();
          let stmt_span = loc_to_range(self.file, stmt.loc);
          let type_def = self
            .decl_def_by_span
            .get(&stmt_span)
            .copied()
            .or_else(|| self.def_spans.and_then(|spans| spans.get(&(self.file, stmt_span)).copied()));
          if let Some(type_def) = type_def {
            let value_def = self.value_defs.get(&type_def).copied().unwrap_or(type_def);
            let class_ty = self.store.intern_type(TypeKind::Ref {
              def: value_def,
              args: Vec::new(),
            });
            if let Some(existing) = self.lookup(&name_str) {
              let has_constructables = !construct_signatures_with_expander(
                self.store.as_ref(),
                existing.ty,
                self.ref_expander,
              )
              .is_empty();
              let ty = if has_constructables {
                existing.ty
              } else {
                self.store.intersection(vec![existing.ty, class_ty])
              };
              self.insert_binding(name_str, ty, Vec::new());
            } else {
              self.insert_binding(name_str, class_ty, Vec::new());
            }
          }
        }
      }
      Stmt::NamespaceDecl(ns) => self.check_namespace(ns),
      Stmt::ModuleDecl(module) => {
        if let Some(body) = &module.stx.body {
          self.check_stmt_list(body);
        }
      }
      Stmt::GlobalDecl(global) => self.check_stmt_list(&global.stx.body),
      _ => {}
    }
  }

  fn check_block_body(&mut self, stmts: &[Node<Stmt>]) {
    self.scopes.push(Scope::default());
    self.check_stmt_list(stmts);
    self.scopes.pop();
  }

  fn check_var_decl(&mut self, decl: &Node<VarDecl>) {
    let prim = self.store.primitive_ids();
    if self.check_var_assignments
      && matches!(decl.stx.mode, VarDeclMode::AwaitUsing)
      && !self.in_async_function
      && !matches!(self.body_kind, BodyKind::TopLevel)
    {
      let range = loc_to_range(self.file, decl.loc);
      let start = range.start;
      let end = start.saturating_add("await".len() as u32);
      self.diagnostics.push(codes::AWAIT_USING_REQUIRES_ASYNC_CONTEXT.error(
        "'await using' statements are only allowed within async functions and at the top levels of modules.",
        Span::new(self.file, TextRange::new(start, end)),
      ));
    }
    for declarator in decl.stx.declarators.iter() {
      let annot_ty = declarator.type_annotation.as_ref().map(|ann| {
        self
          .lookup_typeof_query_binding(ann)
          .unwrap_or_else(|| self.lowerer.lower_type_expr(ann))
      });
      let init_ty = if self.check_var_assignments {
        declarator
          .initializer
          .as_ref()
          .map(|init| match annot_ty {
            Some(expected) => self.check_expr_with_expected(init, expected),
            None => self.check_expr(init),
          })
          .unwrap_or(prim.unknown)
      } else {
        prim.unknown
      };

      if matches!(decl.stx.mode, VarDeclMode::Using | VarDeclMode::AwaitUsing)
        && !matches!(declarator.pattern.stx.pat.stx.as_ref(), AstPat::Id(_))
      {
        self.diagnostics.push(codes::USING_BINDING_PATTERN.error(
          "'using' declarations may not have binding patterns.",
          Span::new(
            self.file,
            loc_to_range(self.file, declarator.pattern.stx.pat.loc),
          ),
        ));
      }

      if self.check_var_assignments {
        if let Some(init) = declarator.initializer.as_ref() {
          match decl.stx.mode {
            VarDeclMode::Using => self.check_using_initializer(init, init_ty, "Disposable"),
            VarDeclMode::AwaitUsing => {
              self.check_using_initializer(init, init_ty, "AsyncDisposable")
            }
            _ => {}
          }
        }
      }
      let binding_ty = match decl.stx.mode {
        VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing => init_ty,
        _ => self.base_type(init_ty),
      };
      let mut final_ty = annot_ty.unwrap_or(binding_ty);
      if self.no_implicit_any && annot_ty.is_none() && final_ty == prim.unknown {
        // Like TypeScript `--noImplicitAny`, report untyped bindings that
        // would otherwise become `any`. Use `any` for recovery to keep
        // type checking resilient.
        self.report_implicit_any_in_pat(&declarator.pattern.stx.pat);
        final_ty = prim.any;
      }
      if self.check_var_assignments {
        if let (Some(ann), Some(init)) = (annot_ty, declarator.initializer.as_ref()) {
          // TypeScript anchors assignment diagnostics for variable initializers
          // on the binding name/pattern (e.g. `const x: T = ...` points at `x`,
          // `const [x]: T = ...` points at `[x]`).
          self.check_assignable(
            init,
            init_ty,
            ann,
            Some(self.binding_name_range(&declarator.pattern.stx.pat)),
          );
        }
      }
      if matches!(decl.stx.mode, VarDeclMode::Var) {
        let var_scope = self.current_var_scope_index();
        self.bind_pattern_in_scope(&declarator.pattern.stx.pat, final_ty, var_scope);
      } else {
        self.check_pat(&declarator.pattern.stx.pat, final_ty);
      }
    }
  }

  fn lookup_typeof_query_binding(
    &self,
    ty: &Node<parse_js::ast::type_expr::TypeExpr>,
  ) -> Option<TypeId> {
    let parse_js::ast::type_expr::TypeExpr::TypeQuery(query) = ty.stx.as_ref() else {
      return None;
    };
    match &query.stx.expr_name {
      parse_js::ast::type_expr::TypeEntityName::Identifier(name) => {
        self.lookup(name.as_str()).map(|binding| binding.ty)
      }
      _ => None,
    }
  }

  fn check_using_initializer(&mut self, init: &Node<AstExpr>, init_ty: TypeId, required: &str) {
    let prim = self.store.primitive_ids();
    if matches!(
      self.store.type_kind(init_ty),
      TypeKind::Any | TypeKind::Unknown
    ) {
      return;
    }
    let Some(resolver) = self.type_resolver.as_ref() else {
      return;
    };
    let Some(def) = resolver.resolve_type_name(&[required.to_string()]) else {
      return;
    };
    let required_ty = self.store.intern_type(TypeKind::Ref {
      def,
      args: Vec::new(),
    });
    // `using`/`await using` declarations ignore nullish values at runtime.
    let required_ty = self
      .store
      .union(vec![required_ty, prim.null, prim.undefined]);
    if self.relate.is_assignable(init_ty, required_ty) {
      return;
    }
    let mut range = loc_to_range(self.file, init.loc);
    if let AstExpr::Unary(unary) = init.stx.as_ref() {
      if matches!(unary.stx.operator, OperatorName::New) {
        let arg_end = unary.stx.argument.loc.end_u32();
        range.end = range.end.min(arg_end);
      }
    }
    self
      .diagnostics
      .push(codes::INVALID_USING_INITIALIZER.error(
        format!("initializer must be assignable to `{required}`"),
        Span::new(self.file, range),
      ));
  }

  fn check_namespace(&mut self, ns: &Node<NamespaceDecl>) {
    fn namespace_key(path: &[String]) -> String {
      path.join(".")
    }

    fn check_ns(checker: &mut Checker<'_>, ns: &Node<NamespaceDecl>, path: &mut Vec<String>) {
      let name = ns.stx.name.clone();
      path.push(name.clone());
      let key = namespace_key(path);

      let mut scope = checker.namespace_scopes.remove(&key).unwrap_or_default();
      if let Some(binding) = checker.lookup(&name) {
        scope.bindings.insert(name.clone(), binding);
      }

      checker.scopes.push(scope);
      checker.var_scopes.push(checker.scopes.len().saturating_sub(1));
      match &ns.stx.body {
        NamespaceBody::Block(stmts) => {
          checker.hoist_function_decls_in_stmt_list(stmts);
          checker.hoist_var_decls_in_stmt_tree(stmts);
          checker.check_stmt_list(stmts)
        }
        NamespaceBody::Namespace(inner) => check_ns(checker, inner, path),
      }
      checker.var_scopes.pop();
      let scope = checker.scopes.pop().unwrap_or_default();
      checker.namespace_scopes.insert(key, scope);
      path.pop();
    }

    let mut path = Vec::new();
    check_ns(self, ns, &mut path);
  }

  fn check_pat(&mut self, pat: &Node<AstPat>, value_ty: TypeId) {
    self.bind_pattern(pat, value_ty);
  }

  fn check_expr(&mut self, expr: &Node<AstExpr>) -> TypeId {
    let ty = match expr.stx.as_ref() {
      AstExpr::Id(id) => self.resolve_ident(&id.stx.name, expr),
      AstExpr::LitNum(num) => {
        let value = num.stx.value.0;
        self
          .store
          .intern_type(TypeKind::NumberLiteral(OrderedFloat::from(value)))
      }
      AstExpr::LitStr(str_lit) => {
        let name = self.store.intern_name_ref(&str_lit.stx.value);
        self.store.intern_type(TypeKind::StringLiteral(name))
      }
      AstExpr::LitTemplate(tpl) => {
        for part in tpl.stx.parts.iter() {
          match part {
            parse_js::ast::expr::lit::LitTemplatePart::Substitution(expr) => {
              self.check_expr(expr);
            }
            parse_js::ast::expr::lit::LitTemplatePart::String(_) => {}
          }
        }
        self.store.primitive_ids().string
      }
      AstExpr::LitBool(b) => self
        .store
        .intern_type(TypeKind::BooleanLiteral(b.stx.value)),
      AstExpr::LitNull(_) => self.store.primitive_ids().null,
      AstExpr::LitBigInt(value) => {
        let trimmed = value.stx.value.trim_end_matches('n');
        let parsed = trimmed.parse::<i128>().unwrap_or(0);
        self
          .store
          .intern_type(TypeKind::BigIntLiteral(parsed.into()))
      }
      AstExpr::LitRegex(_) => {
        // Prefer `RegExp` when available in the type environment; otherwise fall back
        // to `unknown` (regex literals evaluate to objects, not strings).
        let prim = self.store.primitive_ids();
        self.resolve_type_ref(&["RegExp"]).unwrap_or(prim.unknown)
      }
      AstExpr::This(_) => {
        let prim = self.store.primitive_ids();
        let base_this = if self.current_this_ty != prim.unknown {
          self.current_this_ty
        } else {
          self.this_super_context.this_ty.unwrap_or(prim.unknown)
        };
        if base_this == prim.unknown {
          prim.unknown
        } else {
          // Model TypeScript's polymorphic `this` as an intersection between the
          // containing `this` type (class instance, explicit `this` parameter,
          // etc) and the `this` marker type. This preserves fluent `this` return
          // types while still allowing the receiver to satisfy signatures that
          // expect the concrete containing type.
          self
            .store
            .intersection(vec![base_this, self.store.intern_type(TypeKind::This)])
        }
      }
      AstExpr::Super(_) => self
        .this_super_context
        .super_ty
        .unwrap_or(self.current_super_ty),
      AstExpr::ImportMeta(_) => self
        .resolve_type_ref(&["ImportMeta"])
        .unwrap_or(self.store.primitive_ids().unknown),
      AstExpr::NewTarget(_) => {
        let prim = self.store.primitive_ids();
        self.resolve_type_ref(&["Function"]).unwrap_or(prim.unknown)
      }
      AstExpr::Unary(un) => {
        if matches!(un.stx.operator, OperatorName::New) {
          self.check_new_expr(un, expr.loc, None)
        } else {
          self.check_unary(un.stx.operator, &un.stx.argument)
        }
      }
      AstExpr::UnaryPostfix(post) => {
        let operand_ty = self.check_expr(&post.stx.argument);
        match post.stx.operator {
          OperatorName::PostfixIncrement | OperatorName::PostfixDecrement => {
            let prim = self.store.primitive_ids();
            if self.is_bigint_like(self.base_type(operand_ty)) {
              prim.bigint
            } else {
              prim.number
            }
          }
          _ => self.store.primitive_ids().unknown,
        }
      }
      AstExpr::Binary(bin) => self.check_binary(bin.stx.operator, &bin.stx.left, &bin.stx.right),
      AstExpr::Cond(cond) => {
        let _ = self.check_expr(&cond.stx.test);
        let cons = self.check_expr(&cond.stx.consequent);
        let alt = self.check_expr(&cond.stx.alternate);
        self.store.union(vec![cons, alt])
      }
      AstExpr::Call(call) => self.check_call_expr(call, None),
      AstExpr::Import(import) => {
        let prim = self.store.primitive_ids();
        let arg_ty = self.check_expr(&import.stx.module);
        if let Some(attributes) = import.stx.attributes.as_ref() {
          let _ = self.check_expr(attributes);
        }

        self.check_assignable_with_code(
          &import.stx.module,
          arg_ty,
          prim.string,
          None,
          &codes::ARGUMENT_TYPE_MISMATCH,
        );

        let inner_ty = if let AstExpr::LitStr(str_lit) = import.stx.module.stx.as_ref() {
          if let Some(resolver) = self.type_resolver.as_ref() {
            let specifier = str_lit.stx.value.as_str();
            match resolver.resolve_import_typeof(specifier, None) {
              Some(def) => self.store.canon(self.store.intern_type(TypeKind::Ref {
                def,
                args: Vec::new(),
              })),
              None => {
                let span = Span::new(self.file, loc_to_range(self.file, import.stx.module.loc));
                let mut diag = codes::UNRESOLVED_MODULE
                  .error(format!("unresolved module specifier \"{specifier}\""), span);
                diag.push_note(format!("module specifier: \"{specifier}\""));
                self.diagnostics.push(diag);
                prim.unknown
              }
            }
          } else {
            prim.unknown
          }
        } else {
          prim.unknown
        };

        self.promise_type(inner_ty).unwrap_or(prim.unknown)
      }
      AstExpr::TaggedTemplate(tagged) => self.check_tagged_template_expr(tagged, expr.loc, None),
      AstExpr::Member(mem) => {
        let full_range = loc_to_range(self.file, mem.loc);
        self.check_property_not_used_before_initialization(
          &mem.stx.left,
          &mem.stx.right,
          full_range,
        );
        let name_len = mem.stx.right.len() as u32;
        let start = full_range.end.saturating_sub(name_len);
        let prop_range = TextRange::new(start, full_range.end);
        let prim = self.store.primitive_ids();
        let receiver_kind = match mem.stx.left.stx.as_ref() {
          AstExpr::This(_) => MemberAccessReceiver::This,
          AstExpr::Super(_) => MemberAccessReceiver::Super,
          _ => MemberAccessReceiver::Other,
        };
        let obj_ty = self.check_expr(&mem.stx.left);
        let chain_optional =
          mem.stx.optional_chaining || self.is_optional_chain_expr(&mem.stx.left);
        let base_obj_ty = if chain_optional {
          let (non_nullish, _) = narrow_non_nullish(obj_ty, &self.store);
          non_nullish
        } else {
          obj_ty
        };
        let prop_ty = if chain_optional && base_obj_ty == prim.never {
          prim.undefined
        } else {
          match self.member_type_opt(base_obj_ty, &mem.stx.right) {
            Some(ty) => {
              self.check_member_access_for_type(
                base_obj_ty,
                &mem.stx.right,
                prop_range,
                receiver_kind,
                true,
              );
              substitute_this_type(&self.store, ty, base_obj_ty)
            }
            None => {
              if !matches!(
                self.store.type_kind(base_obj_ty),
                TypeKind::Any | TypeKind::Unknown | TypeKind::Never
              ) {
                self.diagnostics.push(codes::PROPERTY_DOES_NOT_EXIST.error(
                  format!(
                    "Property '{}' does not exist on type '{}'.",
                    mem.stx.right,
                    TypeDisplay::new(self.store.as_ref(), base_obj_ty)
                  ),
                  Span::new(self.file, prop_range),
                ));
              }
              prim.any
            }
          }
        };
        if self.native_define_class_fields {
          if let Some(props) = self.current_class_field_param_props {
            if matches!(mem.stx.left.stx.as_ref(), AstExpr::This(_))
              && props.iter().any(|name| name == &mem.stx.right)
            {
              let name_len = mem.stx.right.len() as u32;
              let start = full_range.end.saturating_sub(name_len);
              let range = TextRange::new(start, full_range.end);
              self
                .diagnostics
                .push(codes::PROPERTY_USED_BEFORE_INITIALIZATION.error(
                  format!(
                    "Property '{}' is used before its initialization.",
                    mem.stx.right
                  ),
                  Span::new(self.file, range),
                ));
            }
          }
        }
        if chain_optional {
          self.store.union(vec![prop_ty, prim.undefined])
        } else {
          prop_ty
        }
      }
      AstExpr::ComputedMember(mem) => {
        let prim = self.store.primitive_ids();
        let obj_ty = self.check_expr(&mem.stx.object);
        let chain_optional =
          mem.stx.optional_chaining || self.is_optional_chain_expr(&mem.stx.object);
        let base_obj_ty = if chain_optional {
          let (non_nullish, _) = narrow_non_nullish(obj_ty, &self.store);
          non_nullish
        } else {
          obj_ty
        };
        let key_ty = self.check_expr(&mem.stx.member);

        let literal_key = match mem.stx.member.stx.as_ref() {
          AstExpr::LitStr(s) => Some(s.stx.value.clone()),
          AstExpr::LitNum(n) => Some(n.stx.value.0.to_string()),
          AstExpr::LitBigInt(v) => Some(v.stx.value.clone()),
          _ => match self.store.type_kind(key_ty) {
            TypeKind::StringLiteral(id) => Some(self.store.name(id).to_string()),
            TypeKind::NumberLiteral(num) => Some(num.0.to_string()),
            _ => None,
          },
        };

        let key_is_string_or_number = matches!(
          mem.stx.member.stx.as_ref(),
          AstExpr::LitStr(_) | AstExpr::LitNum(_)
        ) || matches!(
          self.store.type_kind(key_ty),
          TypeKind::StringLiteral(_) | TypeKind::NumberLiteral(_)
        );

        let receiver_kind = match mem.stx.object.stx.as_ref() {
          AstExpr::This(_) => MemberAccessReceiver::This,
          AstExpr::Super(_) => MemberAccessReceiver::Super,
          _ => MemberAccessReceiver::Other,
        };

        let mut ty = if chain_optional && base_obj_ty == prim.never {
          prim.undefined
        } else if let Some(key) = literal_key {
          match self.member_type_opt(base_obj_ty, &key) {
            Some(ty) => {
              let key_range = loc_to_range(self.file, mem.stx.member.loc);
              self.check_member_access_for_type(base_obj_ty, &key, key_range, receiver_kind, false);
              substitute_this_type(&self.store, ty, base_obj_ty)
            }
            None => {
              if key_is_string_or_number
                && !matches!(
                  self.store.type_kind(base_obj_ty),
                  TypeKind::Any | TypeKind::Unknown | TypeKind::Never
                )
              {
                let range = loc_to_range(self.file, mem.stx.member.loc);
                self.diagnostics.push(codes::PROPERTY_DOES_NOT_EXIST.error(
                  format!(
                    "Property '{}' does not exist on type '{}'.",
                    key,
                    TypeDisplay::new(self.store.as_ref(), base_obj_ty)
                  ),
                  Span::new(self.file, range),
                ));
              }
              prim.any
            }
          }
        } else {
          self.member_type_for_index_key(base_obj_ty, key_ty)
        };

        if self.relate.options.no_unchecked_indexed_access {
          ty = self.store.union(vec![ty, prim.undefined]);
        }
        if chain_optional {
          ty = self.store.union(vec![ty, prim.undefined]);
        }
        ty
      }
      AstExpr::LitArr(arr) => self.array_literal_type(arr),
      AstExpr::LitObj(obj) => self.object_literal_type(obj),
      AstExpr::Func(func) => self
        .expr_value_overrides
        .and_then(|overrides| overrides.get(&loc_to_range(self.file, expr.loc)).copied())
        .unwrap_or_else(|| self.function_type(&func.stx.func)),
      AstExpr::ArrowFunc(func) => self
        .expr_value_overrides
        .and_then(|overrides| overrides.get(&loc_to_range(self.file, expr.loc)).copied())
        .unwrap_or_else(|| self.function_type(&func.stx.func)),
      AstExpr::Class(class_expr) => {
        // Class evaluation expressions (`extends`, decorators) need to be checked
        // in the current lexical scope. The class body itself is checked via its
        // own `BodyKind::Class` body check.
        for decorator in class_expr.stx.decorators.iter() {
          let _ = self.check_expr(&decorator.stx.expression);
        }
        if let Some(extends) = class_expr.stx.extends.as_ref() {
          let _ = self.check_expr(extends);
        }

        let range = loc_to_range(self.file, expr.loc);
        let prim = self.store.primitive_ids();
        let override_ty = (|| {
          let overrides = self.expr_value_overrides?;
          if let Some(ty) = overrides.get(&range).copied().filter(|ty| *ty != prim.unknown) {
            return Some(ty);
          }

          // HIR and AST spans can diverge slightly in some nested contexts (e.g.
          // truncated/adjusted loc ranges). When an exact span match fails,
          // conservatively choose the tightest override span that overlaps the
          // class expression.
          let mut best: Option<(u32, TextRange, TypeId)> = None;
          for (span, ty) in overrides.iter() {
            let span = *span;
            let ty = *ty;
            if ty == prim.unknown || !ranges_overlap(span, range) {
              continue;
            }
            let len = span.end.saturating_sub(span.start);
            let replace = match best {
              Some((best_len, best_span, _)) => len < best_len || (len == best_len && span.start < best_span.start),
              None => true,
            };
            if replace {
              best = Some((len, span, ty));
            }
          }
          best.map(|(_, _, ty)| ty)
        })();

        if let Some(ty) = override_ty {
          ty
        } else {
          let class_def = self
            .class_expr_def_by_span
            .get(&range)
            .copied()
            .or_else(|| {
              let mut best: Option<(u32, TextRange, DefId)> = None;
              for (span, def) in self.class_expr_def_by_span.iter() {
                let span = *span;
                if !ranges_overlap(span, range) {
                  continue;
                }
                let len = span.end.saturating_sub(span.start);
                let replace = match best {
                  Some((best_len, best_span, _)) => {
                    len < best_len || (len == best_len && span.start < best_span.start)
                  }
                  None => true,
                };
                if replace {
                  best = Some((len, span, *def));
                }
              }
              best.map(|(_, _, def)| def)
            });

          match class_def {
            Some(type_def) => {
              let value_def = self.value_defs.get(&type_def).copied().unwrap_or(type_def);
              self.store.intern_type(TypeKind::Ref {
                def: value_def,
                args: Vec::new(),
              })
            }
            None => prim.unknown,
          }
        }
      }
      AstExpr::JsxElem(elem) => self.check_jsx_elem(elem),
      AstExpr::IdPat(_) | AstExpr::ArrPat(_) | AstExpr::ObjPat(_) => {
        self.store.primitive_ids().unknown
      }
      AstExpr::Instantiation(inst) => {
        let base_ty = self.check_expr(&inst.stx.expression);
        let args: Vec<_> = inst
          .stx
          .type_arguments
          .iter()
          .map(|arg| self.lowerer.lower_type_expr(arg))
          .collect();
        let span = Span::new(self.file, loc_to_range(self.file, expr.loc));
        self.apply_explicit_type_args(base_ty, &args, span)
      }
      AstExpr::TypeAssertion(assert) => {
        if assert.stx.const_assertion {
          self.const_assertion_type(&assert.stx.expression)
        } else {
          let inner = self.check_expr(&assert.stx.expression);
          if let Some(annotation) = assert.stx.type_annotation.as_ref() {
            let target = self.lowerer.lower_type_expr(annotation);
            target
          } else {
            inner
          }
        }
      }
      AstExpr::NonNullAssertion(assert) => {
        let inner_ty = self.check_expr(&assert.stx.expression);
        let (non_nullish, _) = narrow_non_nullish(inner_ty, &self.store);
        non_nullish
      }
      AstExpr::SatisfiesExpr(expr) => {
        let target_ty = self.lowerer.lower_type_expr(&expr.stx.type_annotation);
        let value_ty = self.check_expr_with_expected(&expr.stx.expression, target_ty);
        if !matches!(
          self.store.type_kind(target_ty),
          TypeKind::Any | TypeKind::Unknown
        ) {
          if let AstExpr::LitObj(obj) = expr.stx.expression.stx.as_ref() {
            if let Some(range) = self.excess_property_range(obj, target_ty) {
              self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                "excess property",
                Span { file: self.file, range },
              ));
              return value_ty;
            }
          }
        }
        if !self.relate.is_assignable(value_ty, target_ty) {
          self.diagnostics.push(codes::TYPE_MISMATCH.error(
            "expression does not satisfy target type",
            Span {
              file: self.file,
              range: loc_to_range(self.file, expr.loc),
            },
          ));
        }
        value_ty
      }
      _ => self.store.primitive_ids().unknown,
    };
    self.record_expr_type(expr.loc, ty);
    ty
  }

  fn is_optional_chain_expr(&self, expr: &Node<AstExpr>) -> bool {
    match expr.stx.as_ref() {
      AstExpr::Member(mem) => {
        mem.stx.optional_chaining || self.is_optional_chain_expr(&mem.stx.left)
      }
      AstExpr::ComputedMember(mem) => {
        mem.stx.optional_chaining || self.is_optional_chain_expr(&mem.stx.object)
      }
      AstExpr::Call(call) => {
        call.stx.optional_chaining || self.is_optional_chain_expr(&call.stx.callee)
      }
      AstExpr::Instantiation(inst) => self.is_optional_chain_expr(&inst.stx.expression),
      AstExpr::TypeAssertion(assert) => self.is_optional_chain_expr(&assert.stx.expression),
      AstExpr::NonNullAssertion(assert) => self.is_optional_chain_expr(&assert.stx.expression),
      AstExpr::SatisfiesExpr(expr) => self.is_optional_chain_expr(&expr.stx.expression),
      _ => false,
    }
  }

  fn recorded_expr_type(&self, loc: Loc) -> Option<TypeId> {
    let range = loc_to_range(self.file, loc);
    self
      .expr_map
      .get(&range)
      .and_then(|id| self.expr_types.get(id.0 as usize))
      .copied()
  }

  fn check_tagged_template_expr(
    &mut self,
    tagged: &Node<parse_js::ast::expr::TaggedTemplateExpr>,
    expr_loc: Loc,
    contextual_return: Option<TypeId>,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    let callee_ty = self.check_expr(&tagged.stx.function);

    let template_obj_ty = self
      .resolve_type_ref(&["TemplateStringsArray"])
      .unwrap_or_else(|| {
        self.store.intern_type(TypeKind::Array {
          ty: prim.string,
          readonly: true,
        })
      });

    let mut arg_types = Vec::with_capacity(1 + tagged.stx.parts.len());
    let mut const_arg_types = Vec::with_capacity(1 + tagged.stx.parts.len());
    arg_types.push(CallArgType::new(template_obj_ty));
    const_arg_types.push(template_obj_ty);

    let mut substitution_exprs = Vec::new();
    for part in tagged.stx.parts.iter() {
      match part {
        parse_js::ast::expr::lit::LitTemplatePart::Substitution(expr) => {
          let ty = self.check_expr(expr);
          substitution_exprs.push(expr);
          arg_types.push(CallArgType::new(ty));
          const_arg_types.push(self.const_inference_type(expr));
        }
        parse_js::ast::expr::lit::LitTemplatePart::String(_) => {}
      }
    }

    let callee_base = self.expand_callable_type(callee_ty);
    let candidate_sigs =
      callable_signatures_with_expander(self.store.as_ref(), callee_base, self.ref_expander);

    let this_arg = match tagged.stx.function.stx.as_ref() {
      AstExpr::Member(mem) => self.recorded_expr_type(mem.stx.left.loc),
      AstExpr::ComputedMember(mem) => self.recorded_expr_type(mem.stx.object.loc),
      _ => None,
    };

    let span = Span {
      file: self.file,
      range: loc_to_range(self.file, expr_loc),
    };

    let resolution = resolve_call(
      &self.store,
      &self.relate,
      &self.instantiation_cache,
      callee_base,
      &arg_types,
      Some(&const_arg_types),
      this_arg,
      contextual_return,
      span,
      self.ref_expander,
    );

    let mut reported_assignability = false;
    let allow_assignable_fallback = resolution.diagnostics.len() == 1
      && resolution.diagnostics[0].code.as_str() == codes::NO_OVERLOAD.as_str()
      && candidate_sigs.len() == 1;
    if allow_assignable_fallback {
      if let Some(sig_id) = resolution
        .contextual_signature
        .or_else(|| candidate_sigs.first().copied())
      {
        let sig = self.store.signature(sig_id);
        let before = self.diagnostics.len();
        for (idx, expr) in substitution_exprs.iter().enumerate() {
          let param_index = idx.saturating_add(1);
          let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
            continue;
          };
          let arg_index = param_index;
          let arg_ty = arg_types
            .get(arg_index)
            .map(|arg| arg.ty)
            .unwrap_or(prim.unknown);
          let expected = match self.store.type_kind(param_ty) {
            TypeKind::TypeParam(id) => sig
              .type_params
              .iter()
              .find(|tp| tp.id == id)
              .and_then(|tp| tp.constraint)
              .unwrap_or(param_ty),
            _ => param_ty,
          };
          self.check_assignable_with_code(
            expr,
            arg_ty,
            expected,
            None,
            &codes::ARGUMENT_TYPE_MISMATCH,
          );
        }
        reported_assignability = self.diagnostics.len() > before;
      }
    }

    if !reported_assignability {
      for diag in &resolution.diagnostics {
        self.diagnostics.push(diag.clone());
      }
    }
    self.record_call_signature(expr_loc, resolution.signature.or(resolution.contextual_signature));

    if resolution.diagnostics.is_empty() {
      resolution.return_type
    } else {
      prim.unknown
    }
  }

  fn check_call_expr(
    &mut self,
    call: &Node<parse_js::ast::expr::CallExpr>,
    contextual_return: Option<TypeId>,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    // `super(...)` is a special-case: even though `super` expressions in instance
    // members are typed as the base instance type (for `super.prop`), `super()`
    // calls must resolve against the base class constructor signatures.
    if matches!(call.stx.callee.stx.as_ref(), AstExpr::Super(_)) {
      return self.check_super_call_expr(call, contextual_return);
    }
    let callee_ty = self.check_expr(&call.stx.callee);

    let call_optional = call.stx.optional_chaining || self.is_optional_chain_expr(&call.stx.callee);

    let callee_base = if call_optional {
      narrow_non_nullish(callee_ty, &self.store).0
    } else {
      callee_ty
    };
    let callee_base = self.expand_callable_type(callee_base);

    let all_candidate_sigs =
      callable_signatures_with_expander(self.store.as_ref(), callee_base, self.ref_expander);
    let mut candidate_sigs = all_candidate_sigs.clone();
    let has_spread = call.stx.arguments.iter().any(|arg| arg.stx.spread);
    let mut callee_for_resolution = callee_base;
    let sigs_for_context = {
      let base_for_context = if has_spread {
        let sigs_without_excess_props: Vec<_> = all_candidate_sigs
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            call
              .stx
              .arguments
              .iter()
              .enumerate()
              .take_while(|(_, arg)| !arg.stx.spread)
              .all(|(idx, arg)| {
                let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, idx) else {
                  return false;
                };
                !self.has_contextual_excess_properties(&arg.stx.value, param_ty)
              })
          })
          .collect();

        if sigs_without_excess_props.is_empty() {
          candidate_sigs.clone()
        } else {
          candidate_sigs = sigs_without_excess_props;
          callee_for_resolution = self.store.intern_type(TypeKind::Callable {
            overloads: candidate_sigs.clone(),
          });
          candidate_sigs.clone()
        }
      } else {
        let sigs_by_arity: Vec<_> = all_candidate_sigs
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            signature_allows_arg_count(self.store.as_ref(), &sig, call.stx.arguments.len())
          })
          .collect();

        let sigs_without_excess_props: Vec<_> = sigs_by_arity
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            call.stx.arguments.iter().enumerate().all(|(idx, arg)| {
              let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, idx) else {
                return false;
              };
              !self.has_contextual_excess_properties(&arg.stx.value, param_ty)
            })
          })
          .collect();

        if sigs_without_excess_props.is_empty() {
          if sigs_by_arity.is_empty() {
            all_candidate_sigs.clone()
          } else {
            sigs_by_arity
          }
        } else {
          candidate_sigs = sigs_without_excess_props;
          callee_for_resolution = self.store.intern_type(TypeKind::Callable {
            overloads: candidate_sigs.clone(),
          });
          candidate_sigs.clone()
        }
      };

      let specialized: Vec<_> = base_for_context
        .iter()
        .copied()
        .filter(|sig_id| {
          let sig = self.store.signature(*sig_id);
          signature_contains_literal_types(self.store.as_ref(), &sig)
        })
        .collect();

      if specialized.is_empty() {
        base_for_context
      } else {
        specialized
      }
    };

    let mut arg_types = Vec::with_capacity(call.stx.arguments.len());
    let mut const_arg_types = Vec::with_capacity(call.stx.arguments.len());
    // For each argument expression, record which parameter index it corresponds
    // to. Spread arguments (and arguments after an unknown-length spread) have no
    // stable mapping.
    let mut param_index_map = Vec::with_capacity(call.stx.arguments.len());
    let mut spread_param_index_map = Vec::with_capacity(call.stx.arguments.len());
    let mut mapping_known = true;
    let mut next_param_index = 0usize;

    for arg in call.stx.arguments.iter() {
      if arg.stx.spread {
        spread_param_index_map.push(mapping_known.then_some(next_param_index));
        let ty = if mapping_known {
          match arg.stx.value.stx.as_ref() {
            AstExpr::LitArr(arr) => {
              use parse_js::ast::expr::lit::LitArrElem;

              if arr
                .stx
                .elements
                .iter()
                .all(|elem| matches!(elem, LitArrElem::Single(_)))
              {
                let arity = arr.stx.elements.len();
                let mut expected_elems = Vec::with_capacity(arity);
                for offset in 0..arity {
                  let param_index = next_param_index.saturating_add(offset);
                  let mut expected_tys = Vec::new();
                  for sig_id in sigs_for_context.iter().copied() {
                    let sig = self.store.signature(sig_id);
                    if let Some(param_ty) =
                      expected_arg_type_at(self.store.as_ref(), &sig, param_index)
                    {
                      expected_tys.push(param_ty);
                    }
                  }
                  let expected = if expected_tys.is_empty() {
                    prim.unknown
                  } else {
                    self.store.union(expected_tys)
                  };
                  expected_elems.push(types_ts_interned::TupleElem {
                    ty: expected,
                    optional: false,
                    rest: false,
                    readonly: false,
                  });
                }
                let expected_tuple = self.store.intern_type(TypeKind::Tuple(expected_elems));
                self.check_expr_with_expected(&arg.stx.value, expected_tuple)
              } else {
                self.check_expr(&arg.stx.value)
              }
            }
            _ => self.check_expr(&arg.stx.value),
          }
        } else {
          self.check_expr(&arg.stx.value)
        };
        arg_types.push(CallArgType::spread(ty));
        const_arg_types.push(self.const_inference_type(&arg.stx.value));
        param_index_map.push(None);

        if mapping_known {
          let mut seen = HashSet::new();
          let fixed_len = fixed_spread_len(self.store.as_ref(), ty, self.ref_expander, &mut seen);
          if let Some(fixed_len) = fixed_len {
            next_param_index = next_param_index.saturating_add(fixed_len);
          } else {
            mapping_known = false;
          }
        }
        continue;
      }

      let param_index = mapping_known.then_some(next_param_index);
      param_index_map.push(param_index);
      spread_param_index_map.push(None);

      let ty = if let Some(param_index) = param_index {
        let mut expected_tys = Vec::new();
        for sig_id in sigs_for_context.iter().copied() {
          let sig = self.store.signature(sig_id);
          if let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) {
            expected_tys.push(param_ty);
          }
        }
        let expected = if expected_tys.is_empty() {
          prim.unknown
        } else {
          self.store.union(expected_tys)
        };
        self.check_expr_with_expected(&arg.stx.value, expected)
      } else {
        self.check_expr(&arg.stx.value)
      };

      arg_types.push(CallArgType::new(ty));
      const_arg_types.push(self.const_inference_type(&arg.stx.value));
      if mapping_known {
        next_param_index = next_param_index.saturating_add(1);
      }
    }

    let mut this_arg = match call.stx.callee.stx.as_ref() {
      AstExpr::Member(mem) => {
        if matches!(mem.stx.left.stx.as_ref(), AstExpr::Super(_)) {
          Some(self.current_this_ty)
        } else {
          self.recorded_expr_type(mem.stx.left.loc)
        }
      }
      AstExpr::ComputedMember(mem) => {
        if matches!(mem.stx.object.stx.as_ref(), AstExpr::Super(_)) {
          Some(self.current_this_ty)
        } else {
          self.recorded_expr_type(mem.stx.object.loc)
        }
      }
      _ => None,
    };
    if call_optional {
      if let Some(this_arg_ty) = this_arg.as_mut() {
        *this_arg_ty = narrow_non_nullish(*this_arg_ty, &self.store).0;
      }
    }

    let span = Span {
      file: self.file,
      range: loc_to_range(self.file, call.loc),
    };
    let overload_error_range =
      call.stx.arguments.first().map(|arg| loc_to_range(self.file, arg.stx.value.loc));

    let mut ty = if call_optional && callee_base == prim.never {
      self.record_call_signature(call.loc, None);
      prim.undefined
    } else {
      let mut resolution = resolve_call(
        &self.store,
        &self.relate,
        &self.instantiation_cache,
        callee_for_resolution,
        &arg_types,
        Some(&const_arg_types),
        this_arg,
        contextual_return,
        span,
        self.ref_expander,
      );
      if let Some(sig_id) = resolution.signature.or(resolution.contextual_signature) {
        let sig = self.store.signature(sig_id);
        let mut refined = false;
        for (idx, arg) in call.stx.arguments.iter().enumerate() {
          if arg.stx.spread {
            let Some(param_index) = spread_param_index_map.get(idx).and_then(|idx| *idx) else {
              continue;
            };
            let AstExpr::LitArr(arr) = arg.stx.value.stx.as_ref() else {
              continue;
            };
            use parse_js::ast::expr::lit::LitArrElem;
            if arr
              .stx
              .elements
              .iter()
              .any(|elem| !matches!(elem, LitArrElem::Single(_)))
            {
              continue;
            }

            let arity = arr.stx.elements.len();
            let mut expected_elems = Vec::with_capacity(arity);
            for offset in 0..arity {
              let Some(param_ty) = expected_arg_type_at(
                self.store.as_ref(),
                &sig,
                param_index.saturating_add(offset),
              ) else {
                expected_elems.clear();
                break;
              };
              expected_elems.push(types_ts_interned::TupleElem {
                ty: param_ty,
                optional: false,
                rest: false,
                readonly: false,
              });
            }
            if expected_elems.len() != arity {
              continue;
            }

            let expected_tuple = self.store.intern_type(TypeKind::Tuple(expected_elems));
            let spread_ty = self.check_expr_with_expected(&arg.stx.value, expected_tuple);
            if let Some(slot) = arg_types.get_mut(idx) {
              slot.ty = spread_ty;
              refined = true;
            }
            if let Some(slot) = const_arg_types.get_mut(idx) {
              *slot = spread_ty;
            }
            continue;
          }

          let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
            continue;
          };
          let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
            continue;
          };
          let Some(func) = (match arg.stx.value.stx.as_ref() {
            AstExpr::ArrowFunc(arrow) => Some(&arrow.stx.func),
            AstExpr::Func(func) => Some(&func.stx.func),
            _ => None,
          }) else {
            continue;
          };

          let Some(refined_ty) = self.refine_function_expr_with_expected(func, param_ty) else {
            continue;
          };
          if let Some(slot) = arg_types.get_mut(idx) {
            slot.ty = refined_ty;
            refined = true;
          }
          if let Some(slot) = const_arg_types.get_mut(idx) {
            *slot = refined_ty;
          }
          self.record_expr_type(arg.stx.value.loc, refined_ty);
        }

        if refined {
          let next = resolve_call(
            &self.store,
            &self.relate,
            &self.instantiation_cache,
            callee_for_resolution,
            &arg_types,
            Some(&const_arg_types),
            this_arg,
            contextual_return,
            span,
            self.ref_expander,
          );
          if next.diagnostics.is_empty() && next.signature.is_some() {
            resolution = next;
          }
        }
      }

      let allow_assignable_fallback = resolution.diagnostics.len() == 1
        && resolution.diagnostics[0].code.as_str() == codes::NO_OVERLOAD.as_str()
        && candidate_sigs.len() == 1;
      let mut reported_assignability = false;
      if allow_assignable_fallback {
        if let Some(sig_id) = resolution
          .contextual_signature
          .or_else(|| candidate_sigs.first().copied())
        {
          let sig = self.store.signature(sig_id);
          let before = self.diagnostics.len();
          for (idx, arg) in call.stx.arguments.iter().enumerate() {
            let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
              continue;
            };
            let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index)
            else {
              continue;
            };
            let arg_ty = arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown);
            let expected = match self.store.type_kind(param_ty) {
              TypeKind::TypeParam(id) => sig
                .type_params
                .iter()
                .find(|tp| tp.id == id)
                .and_then(|tp| tp.constraint)
                .unwrap_or(param_ty),
              _ => param_ty,
            };
            self.check_assignable_with_code(
              &arg.stx.value,
              arg_ty,
              expected,
              None,
              &codes::ARGUMENT_TYPE_MISMATCH,
            );
          }
          reported_assignability = self.diagnostics.len() > before;
        }
      }

      if !reported_assignability {
        for diag in &resolution.diagnostics {
          let mut diag = diag.clone();
          if diag.code.as_str() == codes::NO_OVERLOAD.as_str()
            && diag.message == "no overload matches this call"
          {
            if let Some(range) = overload_error_range {
              diag.primary = Span { file: self.file, range };
            }
          }
          self.diagnostics.push(diag);
        }
      }
      if resolution.diagnostics.is_empty() {
        if let Some(sig_id) = resolution.signature {
          let sig = self.store.signature(sig_id);
          for (idx, arg) in call.stx.arguments.iter().enumerate() {
            let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
              continue;
            };
            let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index)
            else {
              continue;
            };
            let arg_expr = &arg.stx.value;
            let arg_ty = match arg_expr.stx.as_ref() {
              AstExpr::LitObj(_) | AstExpr::LitArr(_) => {
                let contextual = self.check_expr_with_expected(arg_expr, param_ty);
                if let Some(slot) = arg_types.get_mut(idx) {
                  slot.ty = contextual;
                }
                contextual
              }
              _ => arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown),
            };
            let assignable_ty = match arg_expr.stx.as_ref() {
              AstExpr::LitObj(_) | AstExpr::LitArr(_) => {
                let const_ty = const_arg_types.get(idx).copied().unwrap_or(arg_ty);
                if !self.relate.is_assignable(arg_ty, param_ty)
                  && self.relate.is_assignable(const_ty, param_ty)
                {
                  const_ty
                } else {
                  arg_ty
                }
              }
              _ => arg_ty,
            };
            self.check_assignable_with_code(
              arg_expr,
              assignable_ty,
              param_ty,
              None,
              &codes::ARGUMENT_TYPE_MISMATCH,
            );
          }
          if let TypeKind::Predicate {
            parameter,
            asserted: Some(asserted),
            asserts: true,
          } = self.store.type_kind(sig.ret)
          {
            if let PredicateParam::Param(param_idx) = parameter.unwrap_or(PredicateParam::Param(0))
            {
              let param_idx = param_idx as usize;
              if let Some(arg_idx) = param_index_map
                .iter()
                .position(|idx| *idx == Some(param_idx))
              {
                if let Some(arg) = call.stx.arguments.get(arg_idx) {
                  if let AstExpr::Id(id) = arg.stx.value.stx.as_ref() {
                    self.insert_binding(id.stx.name.clone(), asserted, Vec::new());
                  }
                }
              }
            }
          }
          if !has_spread && !signature_allows_arg_count(self.store.as_ref(), &sig, arg_types.len())
          {
            self
              .diagnostics
              .push(codes::ARGUMENT_COUNT_MISMATCH.error("argument count mismatch", span));
          }
        }
      }
      let contextual_sig = resolution
        .signature
        .or(resolution.contextual_signature)
        .or_else(|| candidate_sigs.first().copied());
      if let Some(sig_id) = contextual_sig {
        let sig = self.store.signature(sig_id);
        for (idx, arg) in call.stx.arguments.iter().enumerate() {
          let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
            continue;
          };
          let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
            continue;
          };
          let arg_ty = arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown);
          let contextual = match arg.stx.value.stx.as_ref() {
            AstExpr::ArrowFunc(_) | AstExpr::Func(_)
              if self.first_callable_signature(param_ty).is_some() =>
            {
              // Record the contextual callable type for function expressions so nested
              // body checks can recover the contextual signature (including `this`
              // parameters) from the parent expression table.
              self.contextual_callable_type(param_ty).unwrap_or(arg_ty)
            }
            _ => self.contextual_arg_type(arg_ty, param_ty),
          };
          self.record_expr_type(arg.stx.value.loc, contextual);
         }
       }

      self.record_call_signature(
        call.loc,
        resolution.signature.or(resolution.contextual_signature),
      );
      resolution.return_type
    };

    if call_optional {
      ty = self.store.union(vec![ty, prim.undefined]);
    }
    ty
  }

  fn check_super_call_expr(
    &mut self,
    call: &Node<parse_js::ast::expr::CallExpr>,
    contextual_return: Option<TypeId>,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    // `super()` uses the base constructor value, which can differ from the
    // instance type used for `super.prop`. Record the callee type explicitly
    // because `check_call_expr` skips `check_expr` for the `super` callee.
    //
    // Prefer the span-derived constructor type, but fall back to the per-body
    // context when the AST index cannot determine it (e.g. missing enclosing
    // class/extends info). When both are available, prefer the per-body context
    // if it provides a more-specific instantiation of the same base constructor
    // (e.g. `class C extends Base<number> { constructor() { super(1); } }`).
    let ctx_super_ctor_ty = self.this_super_context.super_value_ty.unwrap_or(prim.unknown);
    let ctx_super_ctor_canon = self.store.canon(ctx_super_ctor_ty);
    let current_super_ctor_canon = self.store.canon(self.current_super_ctor_ty);
    let prefer_ctx = match (
      self.store.type_kind(current_super_ctor_canon),
      self.store.type_kind(ctx_super_ctor_canon),
    ) {
      (
        TypeKind::Ref {
          def: current_def,
          args: current_args,
        },
        TypeKind::Ref {
          def: ctx_def,
          args: ctx_args,
        },
      ) => current_def == ctx_def && ctx_args.len() > current_args.len(),
      _ => false,
    };
    let super_ctor_ty = if ctx_super_ctor_canon != prim.unknown
      && (current_super_ctor_canon == prim.unknown || prefer_ctx)
    {
      ctx_super_ctor_ty
    } else {
      self.current_super_ctor_ty
    };
    self.record_expr_type(call.stx.callee.loc, super_ctor_ty);
    let callee_ty = self.expand_callable_type(super_ctor_ty);
    let arg_exprs = call.stx.arguments.as_slice();
    let all_candidate_sigs =
      construct_signatures_with_expander(self.store.as_ref(), callee_ty, self.ref_expander);
    let mut candidate_sigs = all_candidate_sigs.clone();
    let has_spread = arg_exprs.iter().any(|arg| arg.stx.spread);
    let mut callee_for_resolution = callee_ty;

    let sigs_for_context = {
      let base_for_context = if has_spread {
        let sigs_without_excess_props: Vec<_> = all_candidate_sigs
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            arg_exprs
              .iter()
              .enumerate()
              .take_while(|(_, arg)| !arg.stx.spread)
              .all(|(idx, arg)| {
                let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, idx) else {
                  return false;
                };
                !self.has_contextual_excess_properties(&arg.stx.value, param_ty)
              })
          })
          .collect();

        if sigs_without_excess_props.is_empty() {
          candidate_sigs.clone()
        } else {
          candidate_sigs = sigs_without_excess_props;

          let mut shape = Shape::new();
          shape.construct_signatures = candidate_sigs.clone();
          let shape_id = self.store.intern_shape(shape);
          let obj_id = self.store.intern_object(ObjectType { shape: shape_id });
          callee_for_resolution = self.store.intern_type(TypeKind::Object(obj_id));

          candidate_sigs.clone()
        }
      } else {
        let sigs_by_arity: Vec<_> = all_candidate_sigs
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            signature_allows_arg_count(self.store.as_ref(), &sig, arg_exprs.len())
          })
          .collect();

        let sigs_without_excess_props: Vec<_> = sigs_by_arity
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            arg_exprs.iter().enumerate().all(|(idx, arg)| {
              let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, idx) else {
                return false;
              };
              !self.has_contextual_excess_properties(&arg.stx.value, param_ty)
            })
          })
          .collect();

        if sigs_without_excess_props.is_empty() {
          if sigs_by_arity.is_empty() {
            all_candidate_sigs.clone()
          } else {
            sigs_by_arity
          }
        } else {
          candidate_sigs = sigs_without_excess_props;

          let mut shape = Shape::new();
          shape.construct_signatures = candidate_sigs.clone();
          let shape_id = self.store.intern_shape(shape);
          let obj_id = self.store.intern_object(ObjectType { shape: shape_id });
          callee_for_resolution = self.store.intern_type(TypeKind::Object(obj_id));

          candidate_sigs.clone()
        }
      };

      let specialized: Vec<_> = base_for_context
        .iter()
        .copied()
        .filter(|sig_id| {
          let sig = self.store.signature(*sig_id);
          signature_contains_literal_types(self.store.as_ref(), &sig)
        })
        .collect();

      if specialized.is_empty() {
        base_for_context
      } else {
        specialized
      }
    };

    let mut arg_types = Vec::with_capacity(arg_exprs.len());
    let mut const_arg_types = Vec::with_capacity(arg_exprs.len());
    // For each argument expression, record which parameter index it corresponds
    // to. Spread arguments (and arguments after an unknown-length spread) have no
    // stable mapping.
    let mut param_index_map = Vec::with_capacity(arg_exprs.len());
    let mut spread_param_index_map = Vec::with_capacity(arg_exprs.len());
    let mut mapping_known = true;
    let mut next_param_index = 0usize;

    for arg in arg_exprs.iter() {
      if arg.stx.spread {
        spread_param_index_map.push(mapping_known.then_some(next_param_index));
        let ty = if mapping_known {
          match arg.stx.value.stx.as_ref() {
            AstExpr::LitArr(arr) => {
              use parse_js::ast::expr::lit::LitArrElem;

              if arr
                .stx
                .elements
                .iter()
                .all(|elem| matches!(elem, LitArrElem::Single(_)))
              {
                let arity = arr.stx.elements.len();
                let mut expected_elems = Vec::with_capacity(arity);
                for offset in 0..arity {
                  let param_index = next_param_index.saturating_add(offset);
                  let mut expected_tys = Vec::new();
                  for sig_id in sigs_for_context.iter().copied() {
                    let sig = self.store.signature(sig_id);
                    if let Some(param_ty) =
                      expected_arg_type_at(self.store.as_ref(), &sig, param_index)
                    {
                      expected_tys.push(param_ty);
                    }
                  }
                  let expected = if expected_tys.is_empty() {
                    prim.unknown
                  } else {
                    self.store.union(expected_tys)
                  };
                  expected_elems.push(types_ts_interned::TupleElem {
                    ty: expected,
                    optional: false,
                    rest: false,
                    readonly: false,
                  });
                }
                let expected_tuple = self.store.intern_type(TypeKind::Tuple(expected_elems));
                self.check_expr_with_expected(&arg.stx.value, expected_tuple)
              } else {
                self.check_expr(&arg.stx.value)
              }
            }
            _ => self.check_expr(&arg.stx.value),
          }
        } else {
          self.check_expr(&arg.stx.value)
        };
        arg_types.push(CallArgType::spread(ty));
        const_arg_types.push(self.const_inference_type(&arg.stx.value));
        param_index_map.push(None);

        if mapping_known {
          let mut seen = HashSet::new();
          let fixed_len = fixed_spread_len(self.store.as_ref(), ty, self.ref_expander, &mut seen);
          if let Some(fixed_len) = fixed_len {
            next_param_index = next_param_index.saturating_add(fixed_len);
          } else {
            mapping_known = false;
          }
        }
        continue;
      }

      let param_index = mapping_known.then_some(next_param_index);
      param_index_map.push(param_index);
      spread_param_index_map.push(None);

      let ty = if let Some(param_index) = param_index {
        let mut expected_tys = Vec::new();
        for sig_id in sigs_for_context.iter().copied() {
          let sig = self.store.signature(sig_id);
          if let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) {
            expected_tys.push(param_ty);
          }
        }
        let expected = if expected_tys.is_empty() {
          prim.unknown
        } else {
          self.store.union(expected_tys)
        };
        self.check_expr_with_expected(&arg.stx.value, expected)
      } else {
        self.check_expr(&arg.stx.value)
      };

      arg_types.push(CallArgType::new(ty));
      const_arg_types.push(self.const_inference_type(&arg.stx.value));
      if mapping_known {
        next_param_index = next_param_index.saturating_add(1);
      }
    }

    let span = Span {
      file: self.file,
      range: loc_to_range(self.file, call.loc),
    };
    let overload_error_range =
      call.stx.arguments.first().map(|arg| loc_to_range(self.file, arg.stx.value.loc));
    let mut resolution = resolve_construct(
      &self.store,
      &self.relate,
      &self.instantiation_cache,
      callee_for_resolution,
      &arg_types,
      Some(&const_arg_types),
      None,
      contextual_return,
      span,
      self.ref_expander,
    );

    // Refine inline function arguments with the chosen signature to improve
    // generic inference and avoid spurious `NO_OVERLOAD` errors.
    if let Some(sig_id) = resolution.signature.or(resolution.contextual_signature) {
      let sig = self.store.signature(sig_id);
      let mut refined = false;
      for (idx, arg) in arg_exprs.iter().enumerate() {
        if arg.stx.spread {
          let Some(param_index) = spread_param_index_map.get(idx).and_then(|idx| *idx) else {
            continue;
          };
          let AstExpr::LitArr(arr) = arg.stx.value.stx.as_ref() else {
            continue;
          };
          use parse_js::ast::expr::lit::LitArrElem;
          if arr
            .stx
            .elements
            .iter()
            .any(|elem| !matches!(elem, LitArrElem::Single(_)))
          {
            continue;
          }

          let arity = arr.stx.elements.len();
          let mut expected_elems = Vec::with_capacity(arity);
          for offset in 0..arity {
            let Some(param_ty) = expected_arg_type_at(
              self.store.as_ref(),
              &sig,
              param_index.saturating_add(offset),
            ) else {
              expected_elems.clear();
              break;
            };
            expected_elems.push(types_ts_interned::TupleElem {
              ty: param_ty,
              optional: false,
              rest: false,
              readonly: false,
            });
          }
          if expected_elems.is_empty() {
            continue;
          }

          let expected_tuple = self.store.intern_type(TypeKind::Tuple(expected_elems));
          let spread_ty = self.check_expr_with_expected(&arg.stx.value, expected_tuple);
          if let Some(slot) = arg_types.get_mut(idx) {
            slot.ty = spread_ty;
            refined = true;
          }
          if let Some(slot) = const_arg_types.get_mut(idx) {
            *slot = spread_ty;
          }
          continue;
        }

        let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
          continue;
        };
        let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
          continue;
        };
        let Some(func) = (match arg.stx.value.stx.as_ref() {
          AstExpr::ArrowFunc(arrow) => Some(&arrow.stx.func),
          AstExpr::Func(func) => Some(&func.stx.func),
          _ => None,
        }) else {
          continue;
        };

        let Some(refined_ty) = self.refine_function_expr_with_expected(func, param_ty) else {
          continue;
        };
        if let Some(slot) = arg_types.get_mut(idx) {
          slot.ty = refined_ty;
          refined = true;
        }
        if let Some(slot) = const_arg_types.get_mut(idx) {
          *slot = refined_ty;
        }
        self.record_expr_type(arg.stx.value.loc, refined_ty);
      }

      if refined {
        let next = resolve_construct(
          &self.store,
          &self.relate,
          &self.instantiation_cache,
          callee_for_resolution,
          &arg_types,
          Some(&const_arg_types),
          None,
          contextual_return,
          span,
          self.ref_expander,
        );
        if next.diagnostics.is_empty() && next.signature.is_some() {
          resolution = next;
        }
      }
    }

    let allow_assignable_fallback = resolution.diagnostics.len() == 1
      && resolution.diagnostics[0].code.as_str() == codes::NO_OVERLOAD.as_str()
      && candidate_sigs.len() == 1;
    let mut reported_assignability = false;
    if allow_assignable_fallback {
      if let Some(sig_id) = resolution
        .contextual_signature
        .or_else(|| candidate_sigs.first().copied())
      {
        let sig = self.store.signature(sig_id);
        let before = self.diagnostics.len();
        for (idx, arg) in arg_exprs.iter().enumerate() {
          let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
            continue;
          };
          let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
            continue;
          };
          let arg_ty = arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown);
          let expected = match self.store.type_kind(param_ty) {
            TypeKind::TypeParam(id) => sig
              .type_params
              .iter()
              .find(|tp| tp.id == id)
              .and_then(|tp| tp.constraint)
              .unwrap_or(param_ty),
            _ => param_ty,
          };
          self.check_assignable_with_code(
            &arg.stx.value,
            arg_ty,
            expected,
            None,
            &codes::ARGUMENT_TYPE_MISMATCH,
          );
        }
        reported_assignability = self.diagnostics.len() > before;
      }
    }

    if !reported_assignability {
      for diag in &resolution.diagnostics {
        let mut diag = diag.clone();
        if diag.code.as_str() == codes::NO_OVERLOAD.as_str()
          && diag.message == "no overload matches this call"
        {
          if let Some(range) = overload_error_range {
            diag.primary = Span { file: self.file, range };
          }
        }
        self.diagnostics.push(diag);
      }
    }
    if resolution.diagnostics.is_empty() {
      if let Some(sig_id) = resolution.signature {
        let sig = self.store.signature(sig_id);
        for (idx, arg) in arg_exprs.iter().enumerate() {
          let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
            continue;
          };
          let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
            continue;
          };
          let arg_expr = &arg.stx.value;
          let arg_ty = match arg_expr.stx.as_ref() {
            AstExpr::LitObj(_) | AstExpr::LitArr(_) => {
              let contextual = self.check_expr_with_expected(arg_expr, param_ty);
              if let Some(slot) = arg_types.get_mut(idx) {
                slot.ty = contextual;
              }
              contextual
            }
            _ => arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown),
          };
          self.check_assignable_with_code(
            arg_expr,
            arg_ty,
            param_ty,
            None,
            &codes::ARGUMENT_TYPE_MISMATCH,
          );
        }
      }
    }
    let contextual_sig = resolution
      .signature
      .or(resolution.contextual_signature)
      .or_else(|| candidate_sigs.first().copied());
    if let Some(sig_id) = contextual_sig {
      let sig = self.store.signature(sig_id);
      for (idx, arg) in arg_exprs.iter().enumerate() {
        let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
          continue;
        };
        let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
          continue;
        };
        let arg_ty = arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown);
        let contextual = match arg.stx.value.stx.as_ref() {
          AstExpr::ArrowFunc(_) | AstExpr::Func(_)
            if self.first_callable_signature(param_ty).is_some() =>
          {
            arg_ty
          }
          _ => self.contextual_arg_type(arg_ty, param_ty),
        };
        self.record_expr_type(arg.stx.value.loc, contextual);
      }
    }
    self.record_call_signature(call.loc, resolution.signature.or(resolution.contextual_signature));
    // Match `tsc`: `super(...)` is treated as `void` (the value is not usable).
    prim.void
  }

  fn check_new_expr(
    &mut self,
    un: &Node<parse_js::ast::expr::UnaryExpr>,
    expr_loc: Loc,
    contextual_return: Option<TypeId>,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    let (callee_expr, arg_exprs, span_loc) = match un.stx.argument.stx.as_ref() {
      AstExpr::Call(call) => (
        &call.stx.callee,
        Some(call.stx.arguments.as_slice()),
        call.loc,
      ),
      _ => (&un.stx.argument, None, expr_loc),
    };
    let callee_ty = self.check_expr(callee_expr);
    let callee_ty = self.expand_callable_type(callee_ty);
    let arg_exprs = arg_exprs.unwrap_or(&[]);
    let all_candidate_sigs =
      construct_signatures_with_expander(self.store.as_ref(), callee_ty, self.ref_expander);
    let mut candidate_sigs = all_candidate_sigs.clone();
    let has_spread = arg_exprs.iter().any(|arg| arg.stx.spread);
    let mut callee_for_resolution = callee_ty;
    let sigs_for_context = {
      let base_for_context = if has_spread {
        let sigs_without_excess_props: Vec<_> = all_candidate_sigs
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            arg_exprs
              .iter()
              .enumerate()
              .take_while(|(_, arg)| !arg.stx.spread)
              .all(|(idx, arg)| {
                let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, idx) else {
                  return false;
                };
                !self.has_contextual_excess_properties(&arg.stx.value, param_ty)
              })
          })
          .collect();

        if sigs_without_excess_props.is_empty() {
          candidate_sigs.clone()
        } else {
          candidate_sigs = sigs_without_excess_props;

          let mut shape = Shape::new();
          shape.construct_signatures = candidate_sigs.clone();
          let shape_id = self.store.intern_shape(shape);
          let obj_id = self.store.intern_object(ObjectType { shape: shape_id });
          callee_for_resolution = self.store.intern_type(TypeKind::Object(obj_id));

          candidate_sigs.clone()
        }
      } else {
        let sigs_by_arity: Vec<_> = all_candidate_sigs
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            signature_allows_arg_count(self.store.as_ref(), &sig, arg_exprs.len())
          })
          .collect();

        let sigs_without_excess_props: Vec<_> = sigs_by_arity
          .iter()
          .copied()
          .filter(|sig_id| {
            let sig = self.store.signature(*sig_id);
            arg_exprs.iter().enumerate().all(|(idx, arg)| {
              let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, idx) else {
                return false;
              };
              !self.has_contextual_excess_properties(&arg.stx.value, param_ty)
            })
          })
          .collect();

        if sigs_without_excess_props.is_empty() {
          if sigs_by_arity.is_empty() {
            all_candidate_sigs.clone()
          } else {
            sigs_by_arity
          }
        } else {
          candidate_sigs = sigs_without_excess_props;

          let mut shape = Shape::new();
          shape.construct_signatures = candidate_sigs.clone();
          let shape_id = self.store.intern_shape(shape);
          let obj_id = self.store.intern_object(ObjectType { shape: shape_id });
          callee_for_resolution = self.store.intern_type(TypeKind::Object(obj_id));

          candidate_sigs.clone()
        }
      };

      let specialized: Vec<_> = base_for_context
        .iter()
        .copied()
        .filter(|sig_id| {
          let sig = self.store.signature(*sig_id);
          signature_contains_literal_types(self.store.as_ref(), &sig)
        })
        .collect();

      if specialized.is_empty() {
        base_for_context
      } else {
        specialized
      }
    };

    let mut arg_types = Vec::with_capacity(arg_exprs.len());
    let mut const_arg_types = Vec::with_capacity(arg_exprs.len());
    // For each argument expression, record which parameter index it corresponds
    // to. Spread arguments (and arguments after an unknown-length spread) have no
    // stable mapping.
    let mut param_index_map = Vec::with_capacity(arg_exprs.len());
    let mut spread_param_index_map = Vec::with_capacity(arg_exprs.len());
    let mut mapping_known = true;
    let mut next_param_index = 0usize;

    for arg in arg_exprs.iter() {
      if arg.stx.spread {
        spread_param_index_map.push(mapping_known.then_some(next_param_index));
        let ty = self.check_expr(&arg.stx.value);
        arg_types.push(CallArgType::spread(ty));
        const_arg_types.push(self.const_inference_type(&arg.stx.value));
        param_index_map.push(None);

        if mapping_known {
          let fixed_len = match arg.stx.value.stx.as_ref() {
            AstExpr::LitArr(arr) => {
              use parse_js::ast::expr::lit::LitArrElem;

              if arr
                .stx
                .elements
                .iter()
                .all(|elem| matches!(elem, LitArrElem::Single(_)))
              {
                Some(arr.stx.elements.len())
              } else {
                None
              }
            }
            _ => {
              let mut seen = HashSet::new();
              fixed_spread_len(self.store.as_ref(), ty, self.ref_expander, &mut seen)
            }
          };
          if let Some(fixed_len) = fixed_len {
            next_param_index = next_param_index.saturating_add(fixed_len);
          } else {
            mapping_known = false;
          }
        }
        continue;
      }

      let param_index = mapping_known.then_some(next_param_index);
      param_index_map.push(param_index);
      spread_param_index_map.push(None);

      let ty = if let Some(param_index) = param_index {
        let mut expected_tys = Vec::new();
        for sig_id in sigs_for_context.iter().copied() {
          let sig = self.store.signature(sig_id);
          if let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) {
            expected_tys.push(param_ty);
          }
        }
        let expected = if expected_tys.is_empty() {
          prim.unknown
        } else {
          self.store.union(expected_tys)
        };
        self.check_expr_with_expected(&arg.stx.value, expected)
      } else {
        self.check_expr(&arg.stx.value)
      };

      arg_types.push(CallArgType::new(ty));
      const_arg_types.push(self.const_inference_type(&arg.stx.value));
      if mapping_known {
        next_param_index = next_param_index.saturating_add(1);
      }
    }
    let span = Span {
      file: self.file,
      range: loc_to_range(self.file, span_loc),
    };
    let overload_error_range =
      arg_exprs.first().map(|arg| loc_to_range(self.file, arg.stx.value.loc));
    let mut resolution = resolve_construct(
      &self.store,
      &self.relate,
      &self.instantiation_cache,
      callee_for_resolution,
      &arg_types,
      Some(&const_arg_types),
      None,
      contextual_return,
      span,
      self.ref_expander,
    );

    if let Some(sig_id) = resolution.signature.or(resolution.contextual_signature) {
      let sig = self.store.signature(sig_id);
      let mut refined = false;
      for (idx, arg) in arg_exprs.iter().enumerate() {
        if arg.stx.spread {
          let Some(param_index) = spread_param_index_map.get(idx).and_then(|idx| *idx) else {
            continue;
          };
          let AstExpr::LitArr(arr) = arg.stx.value.stx.as_ref() else {
            continue;
          };
          use parse_js::ast::expr::lit::LitArrElem;
          if arr
            .stx
            .elements
            .iter()
            .any(|elem| !matches!(elem, LitArrElem::Single(_)))
          {
            continue;
          }

          let arity = arr.stx.elements.len();
          let mut expected_elems = Vec::with_capacity(arity);
          for offset in 0..arity {
            let Some(param_ty) = expected_arg_type_at(
              self.store.as_ref(),
              &sig,
              param_index.saturating_add(offset),
            ) else {
              expected_elems.clear();
              break;
            };
            expected_elems.push(types_ts_interned::TupleElem {
              ty: param_ty,
              optional: false,
              rest: false,
              readonly: false,
            });
          }
          if expected_elems.is_empty() {
            continue;
          }

          let expected_tuple = self.store.intern_type(TypeKind::Tuple(expected_elems));
          let spread_ty = self.check_expr_with_expected(&arg.stx.value, expected_tuple);
          if let Some(slot) = arg_types.get_mut(idx) {
            slot.ty = spread_ty;
            refined = true;
          }
          if let Some(slot) = const_arg_types.get_mut(idx) {
            *slot = spread_ty;
          }
          continue;
        }

        let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
          continue;
        };
        let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
          continue;
        };
        let Some(func) = (match arg.stx.value.stx.as_ref() {
          AstExpr::ArrowFunc(arrow) => Some(&arrow.stx.func),
          AstExpr::Func(func) => Some(&func.stx.func),
          _ => None,
        }) else {
          continue;
        };

        let Some(refined_ty) = self.refine_function_expr_with_expected(func, param_ty) else {
          continue;
        };
        if let Some(slot) = arg_types.get_mut(idx) {
          slot.ty = refined_ty;
          refined = true;
        }
        if let Some(slot) = const_arg_types.get_mut(idx) {
          *slot = refined_ty;
        }
        self.record_expr_type(arg.stx.value.loc, refined_ty);
      }

      if refined {
        let next = resolve_construct(
          &self.store,
          &self.relate,
          &self.instantiation_cache,
          callee_for_resolution,
          &arg_types,
          Some(&const_arg_types),
          None,
          contextual_return,
          span,
          self.ref_expander,
        );
        if next.diagnostics.is_empty() && next.signature.is_some() {
          resolution = next;
        }
      }
    }

    let allow_assignable_fallback = resolution.diagnostics.len() == 1
      && resolution.diagnostics[0].code.as_str() == codes::NO_OVERLOAD.as_str()
      && candidate_sigs.len() == 1;
    let mut reported_assignability = false;
    if allow_assignable_fallback {
      if let Some(sig_id) = resolution
        .contextual_signature
        .or_else(|| candidate_sigs.first().copied())
      {
        let sig = self.store.signature(sig_id);
        let before = self.diagnostics.len();
        for (idx, arg) in arg_exprs.iter().enumerate() {
          let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
            continue;
          };
          let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
            continue;
          };
          let arg_ty = arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown);
          let expected = match self.store.type_kind(param_ty) {
            TypeKind::TypeParam(id) => sig
              .type_params
              .iter()
              .find(|tp| tp.id == id)
              .and_then(|tp| tp.constraint)
              .unwrap_or(param_ty),
            _ => param_ty,
          };
          self.check_assignable_with_code(
            &arg.stx.value,
            arg_ty,
            expected,
            None,
            &codes::ARGUMENT_TYPE_MISMATCH,
          );
        }
        reported_assignability = self.diagnostics.len() > before;
      }
    }

    if !reported_assignability {
      for diag in &resolution.diagnostics {
        let mut diag = diag.clone();
        if diag.code.as_str() == codes::NO_OVERLOAD.as_str()
          && diag.message == "no overload matches this call"
        {
          if let Some(range) = overload_error_range {
            diag.primary = Span { file: self.file, range };
          }
        }
        self.diagnostics.push(diag);
      }
    }
    if resolution.diagnostics.is_empty() {
      if let Some(sig_id) = resolution.signature {
        let sig = self.store.signature(sig_id);
        for (idx, arg) in arg_exprs.iter().enumerate() {
          let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
            continue;
          };
          let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
            continue;
          };
          let arg_expr = &arg.stx.value;
          let arg_ty = match arg_expr.stx.as_ref() {
            AstExpr::LitObj(_) | AstExpr::LitArr(_) => {
              let contextual = self.check_expr_with_expected(arg_expr, param_ty);
              if let Some(slot) = arg_types.get_mut(idx) {
                slot.ty = contextual;
              }
              contextual
            }
            _ => arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown),
          };
          self.check_assignable_with_code(
            arg_expr,
            arg_ty,
            param_ty,
            None,
            &codes::ARGUMENT_TYPE_MISMATCH,
          );
        }
      }
    }
    let contextual_sig = resolution
      .signature
      .or(resolution.contextual_signature)
      .or_else(|| candidate_sigs.first().copied());
    if let Some(sig_id) = contextual_sig {
      let sig = self.store.signature(sig_id);
      for (idx, arg) in arg_exprs.iter().enumerate() {
        let Some(param_index) = param_index_map.get(idx).and_then(|idx| *idx) else {
          continue;
        };
        let Some(param_ty) = expected_arg_type_at(self.store.as_ref(), &sig, param_index) else {
          continue;
        };
        let arg_ty = arg_types.get(idx).map(|arg| arg.ty).unwrap_or(prim.unknown);
        let contextual = match arg.stx.value.stx.as_ref() {
          AstExpr::ArrowFunc(_) | AstExpr::Func(_)
            if self.first_callable_signature(param_ty).is_some() =>
          {
            arg_ty
          }
          _ => self.contextual_arg_type(arg_ty, param_ty),
        };
        self.record_expr_type(arg.stx.value.loc, contextual);
      }
    }
    self.record_call_signature(
      expr_loc,
      resolution.signature.or(resolution.contextual_signature),
    );
    resolution.return_type
  }

  fn check_jsx_elem(&mut self, elem: &Node<JsxElem>) -> TypeId {
    let prim = self.store.primitive_ids();
    if self.jsx_mode.is_none() {
      self.diagnostics.push(codes::JSX_DISABLED.error(
        "jsx is disabled",
        Span::new(self.file, loc_to_range(self.file, elem.loc)),
      ));
      self.record_expr_type(elem.loc, prim.unknown);
      return prim.unknown;
    }

    self.check_jsx_runtime_module(elem.loc);

    let element_ty = self.jsx_element_type(elem.loc);
    let element_type_constraint = self.jsx_element_type_constraint_type();

    match &elem.stx.name {
      None => {
        match self.jsx_mode {
          Some(JsxMode::React) | Some(JsxMode::Preserve) => {
            let Some(react_binding) = self.lookup("React") else {
              let elem_range = loc_to_range(self.file, elem.loc);
              let opening_range =
                TextRange::new(elem_range.start, (elem_range.start + 2).min(elem_range.end));
              self.diagnostics.push(codes::JSX_FACTORY_MISSING.error(
                "This JSX tag requires 'React' to be in scope, but it could not be found.",
                Span::new(self.file, opening_range),
              ));
              self
                .diagnostics
                .push(codes::JSX_FRAGMENT_FACTORY_MISSING.error(
                  "Using JSX fragments requires fragment factory 'React' to be in scope, but it could not be found.",
                  Span::new(self.file, opening_range),
                ));
              let _ = self.jsx_actual_props(elem.loc, &elem.stx.attributes, &elem.stx.children, None);
              self.record_expr_type(elem.loc, element_ty);
              return element_ty;
            };

            let fragment_ty = self.member_type(react_binding.ty, "Fragment");
            if matches!(
              self.store.type_kind(fragment_ty),
              TypeKind::Any | TypeKind::Unknown
            ) {
              let _ =
                self.jsx_actual_props(elem.loc, &elem.stx.attributes, &elem.stx.children, None);
            } else {
              let expected_props_ty = self
                .jsx_expected_props_for_value_tag(fragment_ty, elem.loc)
                .map(|expected| self.jsx_apply_intrinsic_attributes(expected));
              let actual_props = self.jsx_actual_props(
                elem.loc,
                &elem.stx.attributes,
                &elem.stx.children,
                expected_props_ty,
              );
              if let Some(expected_props_ty) = expected_props_ty {
                self.check_jsx_props(elem.loc, &actual_props, expected_props_ty);
              }
            }
          }
          _ => {
            let _ = self.jsx_actual_props(elem.loc, &elem.stx.attributes, &elem.stx.children, None);
          }
        }
      }
      Some(JsxElemName::Name(name)) => {
        let tag_buf = name
          .stx
          .namespace
          .as_ref()
          .map(|ns| format!("{ns}:{}", name.stx.name));
        let tag = tag_buf.as_deref().unwrap_or_else(|| name.stx.name.as_str());
        if let Some(constraint) = element_type_constraint {
          let tag_ty = self.store.intern_type(TypeKind::StringLiteral(
            self.store.intern_name_ref(tag),
          ));
          if !self.relate.is_assignable(tag_ty, constraint) {
            self.diagnostics.push(codes::JSX_INVALID_ELEMENT_TYPE.error(
              format!(
                "Its type '{}' is not a valid JSX element type.",
                TypeDisplay::new(self.store.as_ref(), tag_ty)
              ),
              Span::new(self.file, loc_to_range(self.file, name.loc)),
            ));
          }
        }
        let intrinsic_elements = self.jsx_intrinsic_elements_type(elem.loc);
        let expected_props_ty = if intrinsic_elements != prim.unknown {
          self.member_type(intrinsic_elements, tag)
        } else {
          prim.unknown
        };
        if expected_props_ty == prim.unknown {
          let _ = self.jsx_actual_props(elem.loc, &elem.stx.attributes, &elem.stx.children, None);
          if intrinsic_elements != prim.unknown {
            self
              .diagnostics
              .push(codes::JSX_UNKNOWN_INTRINSIC_ELEMENT.error(
                format!("unknown JSX intrinsic element `{tag}`"),
                Span::new(self.file, loc_to_range(self.file, name.loc)),
              ));
          }
        } else {
          let expected_props_ty_for_context = self.refine_jsx_expected_props_by_discriminants(
            expected_props_ty,
            &elem.stx.attributes,
            &elem.stx.children,
          );
          let expected_props_ty_for_context =
            self.jsx_apply_intrinsic_attributes(expected_props_ty_for_context);
          let expected_props_ty = self.jsx_apply_intrinsic_attributes(expected_props_ty);
          let actual_props = self.jsx_actual_props(
            elem.loc,
            &elem.stx.attributes,
            &elem.stx.children,
            Some(expected_props_ty_for_context),
          );
          self.check_jsx_props(name.loc, &actual_props, expected_props_ty);
        }
      }
      Some(JsxElemName::Id(id)) => {
        let name = id.stx.name.as_str();
        if name.contains(':') || name.contains('-') {
          if let Some(constraint) = element_type_constraint {
            let tag_ty = self.store.intern_type(TypeKind::StringLiteral(
              self.store.intern_name_ref(name),
            ));
            if !self.relate.is_assignable(tag_ty, constraint) {
              self.diagnostics.push(codes::JSX_INVALID_ELEMENT_TYPE.error(
                format!(
                  "Its type '{}' is not a valid JSX element type.",
                  TypeDisplay::new(self.store.as_ref(), tag_ty)
                ),
                Span::new(self.file, loc_to_range(self.file, id.loc)),
              ));
            }
          }
          let intrinsic_elements = self.jsx_intrinsic_elements_type(elem.loc);
          let expected_props_ty = if intrinsic_elements != prim.unknown {
            self.member_type(intrinsic_elements, name)
          } else {
            prim.unknown
          };
          if expected_props_ty == prim.unknown {
            let _ = self.jsx_actual_props(elem.loc, &elem.stx.attributes, &elem.stx.children, None);
            if intrinsic_elements != prim.unknown {
              self
                .diagnostics
                .push(codes::JSX_UNKNOWN_INTRINSIC_ELEMENT.error(
                  format!("unknown JSX intrinsic element `{name}`"),
                  Span::new(self.file, loc_to_range(self.file, id.loc)),
                ));
            }
          } else {
            let expected_props_ty_for_context = self.refine_jsx_expected_props_by_discriminants(
              expected_props_ty,
              &elem.stx.attributes,
              &elem.stx.children,
            );
            let expected_props_ty_for_context =
              self.jsx_apply_intrinsic_attributes(expected_props_ty_for_context);
            let expected_props_ty = self.jsx_apply_intrinsic_attributes(expected_props_ty);
            let actual_props = self.jsx_actual_props(
              elem.loc,
              &elem.stx.attributes,
              &elem.stx.children,
              Some(expected_props_ty_for_context),
            );
            self.check_jsx_props(id.loc, &actual_props, expected_props_ty);
          }
        } else {
          let component_ty = self
            .lookup(name)
            .map(|binding| binding.ty)
            .unwrap_or_else(|| {
              self.diagnostics.push(codes::UNKNOWN_IDENTIFIER.error(
                format!("unknown identifier `{name}`"),
                Span::new(self.file, loc_to_range(self.file, id.loc)),
              ));
              prim.any
            });
          if let Some(constraint) = element_type_constraint {
            if !self.relate.is_assignable(component_ty, constraint) {
              self.diagnostics.push(codes::JSX_INVALID_ELEMENT_TYPE.error(
                format!(
                  "Its type '{}' is not a valid JSX element type.",
                  TypeDisplay::new(self.store.as_ref(), component_ty)
                ),
                Span::new(self.file, loc_to_range(self.file, id.loc)),
              ));
            }
          }
          let expected_props_ty = self.jsx_expected_props_for_value_tag(component_ty, elem.loc).map(
            |expected| {
              let refined = self.refine_jsx_expected_props_by_discriminants(
                expected,
                &elem.stx.attributes,
                &elem.stx.children,
              );
              self.jsx_apply_intrinsic_attributes(refined)
            },
          );
          let actual_props = self.jsx_actual_props(
            elem.loc,
            &elem.stx.attributes,
            &elem.stx.children,
            expected_props_ty,
          );
          self.check_jsx_value_tag(component_ty, &actual_props, element_ty, elem.loc, id.loc);
        }
      }
      Some(JsxElemName::Member(member)) => {
        let base_name = member.stx.base.stx.name.as_str();
        if base_name.contains(':') || base_name.contains('-') {
          let mut tag = base_name.to_string();
          for segment in member.stx.path.iter() {
            tag.push('.');
            tag.push_str(segment);
          }
          if let Some(constraint) = element_type_constraint {
            let tag_ty = self.store.intern_type(TypeKind::StringLiteral(
              self.store.intern_name_ref(&tag),
            ));
            if !self.relate.is_assignable(tag_ty, constraint) {
              self.diagnostics.push(codes::JSX_INVALID_ELEMENT_TYPE.error(
                format!(
                  "Its type '{}' is not a valid JSX element type.",
                  TypeDisplay::new(self.store.as_ref(), tag_ty)
                ),
                Span::new(self.file, loc_to_range(self.file, member.loc)),
              ));
            }
          }
          let intrinsic_elements = self.jsx_intrinsic_elements_type(elem.loc);
          let expected_props_ty = if intrinsic_elements != prim.unknown {
            self.member_type(intrinsic_elements, &tag)
          } else {
            prim.unknown
          };
          if expected_props_ty == prim.unknown {
            let _ = self.jsx_actual_props(elem.loc, &elem.stx.attributes, &elem.stx.children, None);
            if intrinsic_elements != prim.unknown {
              self
                .diagnostics
                .push(codes::JSX_UNKNOWN_INTRINSIC_ELEMENT.error(
                  format!("unknown JSX intrinsic element `{tag}`"),
                  Span::new(self.file, loc_to_range(self.file, member.loc)),
                ));
            }
          } else {
            let expected_props_ty_for_context = self.refine_jsx_expected_props_by_discriminants(
              expected_props_ty,
              &elem.stx.attributes,
              &elem.stx.children,
            );
            let expected_props_ty_for_context =
              self.jsx_apply_intrinsic_attributes(expected_props_ty_for_context);
            let expected_props_ty = self.jsx_apply_intrinsic_attributes(expected_props_ty);
            let actual_props = self.jsx_actual_props(
              elem.loc,
              &elem.stx.attributes,
              &elem.stx.children,
              Some(expected_props_ty_for_context),
            );
            self.check_jsx_props(member.loc, &actual_props, expected_props_ty);
          }
        } else {
          // Member expressions like `<Foo.Bar />` are treated like looking up
          // `Foo` and then checking `.Bar` as a value.
          let mut current = self
            .lookup(base_name)
            .map(|binding| binding.ty)
            .unwrap_or_else(|| {
              self.diagnostics.push(codes::UNKNOWN_IDENTIFIER.error(
                format!("unknown identifier `{base_name}`"),
                Span::new(self.file, loc_to_range(self.file, member.stx.base.loc)),
              ));
              prim.any
            });
          for segment in member.stx.path.iter() {
            current = self.member_type(current, segment);
          }
          if let Some(constraint) = element_type_constraint {
            if !self.relate.is_assignable(current, constraint) {
              self.diagnostics.push(codes::JSX_INVALID_ELEMENT_TYPE.error(
                format!(
                  "Its type '{}' is not a valid JSX element type.",
                  TypeDisplay::new(self.store.as_ref(), current)
                ),
                Span::new(self.file, loc_to_range(self.file, member.loc)),
              ));
            }
          }
          let expected_props_ty = self.jsx_expected_props_for_value_tag(current, elem.loc).map(
            |expected| {
              let refined = self.refine_jsx_expected_props_by_discriminants(
                expected,
                &elem.stx.attributes,
                &elem.stx.children,
              );
              self.jsx_apply_intrinsic_attributes(refined)
            },
          );
          let actual_props = self.jsx_actual_props(
            elem.loc,
            &elem.stx.attributes,
            &elem.stx.children,
            expected_props_ty,
          );
          self.check_jsx_value_tag(current, &actual_props, element_ty, elem.loc, member.loc);
        }
      }
    }
    self.record_expr_type(elem.loc, element_ty);
    element_ty
  }

  fn check_jsx_value_tag(
    &mut self,
    tag_ty: TypeId,
    actual_props: &JsxActualProps,
    element_ty: TypeId,
    elem_loc: Loc,
    tag_loc: Loc,
  ) {
    let prim = self.store.primitive_ids();
    if matches!(
      self.store.type_kind(tag_ty),
      TypeKind::Any | TypeKind::Unknown
    ) {
      return;
    }

    let expanded = self.expand_for_props(tag_ty);
    if expanded != tag_ty {
      self.check_jsx_value_tag(expanded, actual_props, element_ty, elem_loc, tag_loc);
      return;
    }

    match self.store.type_kind(tag_ty) {
      TypeKind::Union(members) => {
        for member in members {
          let before = self.diagnostics.len();
          self.check_jsx_value_tag(member, actual_props, element_ty, elem_loc, tag_loc);
          if self.diagnostics.len() > before {
            return;
          }
        }
      }
      TypeKind::StringLiteral(name_id) => {
        let tag = self.store.name(name_id);
        self.check_jsx_intrinsic_tag(tag.as_str(), actual_props, elem_loc, tag_loc);
      }
      TypeKind::String => {
        // Dynamic intrinsic tag; allow it only when `JSX.IntrinsicElements` provides a string
        // index signature.
        let intrinsic_elements = self.jsx_intrinsic_elements_type(elem_loc);
        if intrinsic_elements == prim.unknown {
          return;
        }
        let expected_props_ty = self.member_type_for_index_key(intrinsic_elements, prim.string);
        if expected_props_ty == prim.unknown {
          self.diagnostics.push(codes::NO_OVERLOAD.error(
            "JSX tag type `string` is not assignable to JSX.IntrinsicElements",
            Span::new(self.file, loc_to_range(self.file, tag_loc)),
          ));
          return;
        }
        let expected_props_ty = self.jsx_apply_intrinsic_attributes(expected_props_ty);
        self.check_jsx_props(tag_loc, actual_props, expected_props_ty);
      }
      _ => {
        self.check_jsx_component(tag_ty, actual_props, element_ty, elem_loc, tag_loc);
      }
    }
  }

  fn jsx_expected_props_for_value_tag(&mut self, tag_ty: TypeId, elem_loc: Loc) -> Option<TypeId> {
    let prim = self.store.primitive_ids();
    if matches!(
      self.store.type_kind(tag_ty),
      TypeKind::Any | TypeKind::Unknown
    ) {
      return None;
    }

    let expanded = self.expand_for_props(tag_ty);
    if expanded != tag_ty {
      return self.jsx_expected_props_for_value_tag(expanded, elem_loc);
    }

    match self.store.type_kind(tag_ty) {
      TypeKind::Union(members) => {
        let mut collected = Vec::new();
        for member in members {
          let props_ty = self.jsx_expected_props_for_value_tag(member, elem_loc)?;
          collected.push(props_ty);
        }
        if collected.is_empty() {
          None
        } else {
          Some(self.store.intersection(collected))
        }
      }
      TypeKind::StringLiteral(name_id) => {
        let intrinsic_elements = self.jsx_intrinsic_elements_type(elem_loc);
        if intrinsic_elements == prim.unknown {
          return None;
        }
        let tag = self.store.name(name_id);
        let expected_props_ty = self.member_type(intrinsic_elements, tag.as_str());
        (expected_props_ty != prim.unknown).then_some(expected_props_ty)
      }
      TypeKind::String => {
        let intrinsic_elements = self.jsx_intrinsic_elements_type(elem_loc);
        if intrinsic_elements == prim.unknown {
          return None;
        }
        let expected_props_ty = self.member_type_for_index_key(intrinsic_elements, prim.string);
        (expected_props_ty != prim.unknown).then_some(expected_props_ty)
      }
      _ => {
        let call_sigs = self.jsx_component_call_signatures(tag_ty);
        let is_construct = call_sigs.is_empty();
        let sigs = if !is_construct {
          call_sigs
        } else {
          self.jsx_component_construct_signatures(tag_ty)
        };
        if sigs.is_empty() {
          return None;
        }
        let empty_props = {
          let shape_id = self.store.intern_shape(Shape::new());
          let obj = self.store.intern_object(ObjectType { shape: shape_id });
          self.store.intern_type(TypeKind::Object(obj))
        };
        let mut props = Vec::new();
        for sig_id in sigs {
          let sig = self.store.signature(sig_id);
          let mut props_ty = sig.params.first().map(|p| p.ty).unwrap_or(empty_props);
          let ret_ty = sig.ret;

          if is_construct {
            match self.jsx_element_attributes_prop_name(elem_loc) {
              JsxAttributesPropertyName::Missing => {}
              JsxAttributesPropertyName::Empty => {
                props_ty = ret_ty;
              }
              JsxAttributesPropertyName::Name(attrs_prop) => {
                let prop_name = self.store.name(attrs_prop);
                if self.type_has_prop(ret_ty, &prop_name) {
                  props_ty = self.member_type(ret_ty, &prop_name);
                } else {
                  // The instance type does not have the required member, so we can't determine a
                  // props type. TypeScript treats this as `unknown` (and may later emit TS2607 if
                  // explicit attributes are present).
                  return None;
                }
              }
            };
          }

          props_ty = self.jsx_apply_library_managed_attributes(tag_ty, props_ty);
          if is_construct {
            let class_attrs = self.jsx_intrinsic_class_attributes_type(ret_ty);
            if !matches!(self.store.type_kind(class_attrs), TypeKind::EmptyObject) {
              props_ty = self.store.intersection(vec![props_ty, class_attrs]);
            }
          }
          props.push(props_ty);
        }
        props.sort();
        props.dedup();
        let expected_props = if props.len() == 1 {
          props[0]
        } else {
          self.store.union(props)
        };
        (expected_props != prim.unknown).then_some(expected_props)
      }
    }
  }

  fn check_jsx_intrinsic_tag(
    &mut self,
    tag: &str,
    actual_props: &JsxActualProps,
    elem_loc: Loc,
    tag_loc: Loc,
  ) {
    let prim = self.store.primitive_ids();
    let intrinsic_elements = self.jsx_intrinsic_elements_type(elem_loc);
    if intrinsic_elements == prim.unknown {
      return;
    }
    let expected_props_ty = self.member_type(intrinsic_elements, tag);
    if expected_props_ty == prim.unknown {
      self
        .diagnostics
        .push(codes::JSX_UNKNOWN_INTRINSIC_ELEMENT.error(
          format!("unknown JSX intrinsic element `{tag}`"),
          Span::new(self.file, loc_to_range(self.file, tag_loc)),
        ));
      return;
    }
    let expected_props_ty = self.jsx_apply_intrinsic_attributes(expected_props_ty);
    self.check_jsx_props(tag_loc, actual_props, expected_props_ty);
  }

  fn jsx_discriminant_value_type(&mut self, expr: &Node<AstExpr>) -> Option<TypeId> {
    let prim = self.store.primitive_ids();
    let is_literal_type = |checker: &Checker<'_>, ty: TypeId| {
      let ty = checker.expand_ref(ty);
      ty == prim.null
        || matches!(
          checker.store.type_kind(ty),
          TypeKind::StringLiteral(_)
            | TypeKind::NumberLiteral(_)
            | TypeKind::BooleanLiteral(_)
            | TypeKind::BigIntLiteral(_)
        )
    };
    match expr.stx.as_ref() {
      AstExpr::LitStr(str_lit) => {
        let name = self.store.intern_name_ref(&str_lit.stx.value);
        Some(self.store.intern_type(TypeKind::StringLiteral(name)))
      }
      AstExpr::LitNum(num) => Some(
        self
          .store
          .intern_type(TypeKind::NumberLiteral(OrderedFloat::from(num.stx.value.0))),
      ),
      AstExpr::LitBigInt(bigint) => {
        // `parse-js` stores bigint literals as canonical decimal strings (without the trailing `n`).
        // `types-ts-interned` represents bigint literal types as `num_bigint::BigInt`.
        let parsed = bigint
          .stx
          .value
          .parse::<BigInt>()
          .unwrap_or_else(|_| BigInt::from(0u8));
        Some(self.store.intern_type(TypeKind::BigIntLiteral(parsed)))
      }
      AstExpr::LitBool(b) => Some(self.store.intern_type(TypeKind::BooleanLiteral(b.stx.value))),
      AstExpr::LitNull(_) => Some(prim.null),
      AstExpr::TypeAssertion(assert) if assert.stx.const_assertion => {
        self.jsx_discriminant_value_type(&assert.stx.expression)
      }
      AstExpr::NonNullAssertion(assert) => self.jsx_discriminant_value_type(&assert.stx.expression),
      AstExpr::SatisfiesExpr(satisfies) => self.jsx_discriminant_value_type(&satisfies.stx.expression),
      AstExpr::Id(id) => {
        let binding_ty = self.lookup(&id.stx.name).map(|binding| binding.ty);
        let ty = binding_ty.or_else(|| self.recorded_expr_type(expr.loc))?;
        is_literal_type(self, ty).then_some(ty)
      }
      AstExpr::Member(member) if !member.stx.optional_chaining => {
        // Only handle the simple, context-free case `Id.prop` where `Id` is a
        // known binding and `prop` resolves to a literal type (e.g. `const K =
        // { A: "a", B: "b" } as const; <Foo kind={K.A} />`).
        let left = match member.stx.left.stx.as_ref() {
          AstExpr::Id(id) => self.lookup(&id.stx.name).map(|binding| binding.ty),
          _ => None,
        };
        let left = left.or_else(|| self.recorded_expr_type(member.stx.left.loc))?;
        let prop_ty = self.member_type(left, &member.stx.right);
        is_literal_type(self, prop_ty).then_some(prop_ty)
      }
      _ => None,
    }
  }

  fn refine_jsx_expected_props_by_discriminants(
    &mut self,
    expected: TypeId,
    attrs: &[JsxAttr],
    _children: &[JsxElemChild],
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    let expected_expanded = self.expand_ref(expected);
    let TypeKind::Union(members) = self.store.type_kind(expected_expanded) else {
      return expected;
    };

    let mut discriminants: Vec<(String, TypeId)> = Vec::new();
    for attr in attrs {
      let JsxAttr::Named { name, value } = attr else {
        continue;
      };

      if name.stx.namespace.is_some()
        || name.stx.name.contains('-')
        || name.stx.name.contains(':')
      {
        continue;
      }

      let value_ty = match value {
        None => Some(self.store.intern_type(TypeKind::BooleanLiteral(true))),
        Some(JsxAttrVal::Text(text)) => Some(self.jsx_attr_text_type(text)),
        Some(JsxAttrVal::Expression(container)) => {
          if is_empty_jsx_expr_placeholder(&container.stx.value) {
            None
          } else {
            self.jsx_discriminant_value_type(&container.stx.value)
          }
        }
        Some(JsxAttrVal::Element(_)) => None,
      };
      let Some(value_ty) = value_ty else {
        continue;
      };

      discriminants.push((name.stx.name.clone(), value_ty));
    }

    if discriminants.is_empty() {
      return expected;
    }

    // Narrow the contextual type incrementally. If a candidate discriminant
    // would eliminate *all* union members (e.g. React's `key` attribute, which
    // typically comes from `JSX.IntrinsicAttributes` and is not present on the
    // raw props union), ignore it so other discriminants can still refine.
    let mut current: Vec<TypeId> = members.clone();
    for (name, lit_ty) in discriminants.iter() {
      let mut subset: Vec<TypeId> = Vec::new();
      for member in current.iter().copied() {
        let prop_ty = self.member_type(member, name.as_str());
        if prop_ty != prim.unknown && self.relate.is_assignable(*lit_ty, prop_ty) {
          subset.push(member);
        }
      }
      if subset.is_empty() {
        continue;
      }
      subset.sort_by(|a, b| self.store.type_cmp(*a, *b));
      subset.dedup();
      current = subset;
      if current.len() <= 1 {
        break;
      }
    }

    current.sort_by(|a, b| self.store.type_cmp(*a, *b));
    current.dedup();

    match current.len() {
      0 => expected,
      1 => current[0],
      _ if current.len() == members.len() => expected,
      _ => self.store.union(current),
    }
  }

  fn jsx_actual_props(
    &mut self,
    loc: Loc,
    attrs: &[JsxAttr],
    children: &[JsxElemChild],
    expected: Option<TypeId>,
  ) -> JsxActualProps {
    fn is_object_like_jsx_spread_type(
      checker: &Checker<'_>,
      ty: TypeId,
      seen: &mut HashSet<TypeId>,
      depth: usize,
    ) -> bool {
      if depth > 32 {
        return true;
      }

      let ty = checker.store.canon(ty);
      let ty = checker.expand_ref(ty);
      let expanded = checker.expand_for_props(ty);
      if expanded != ty {
        return is_object_like_jsx_spread_type(checker, expanded, seen, depth + 1);
      }

      let ty = checker.store.canon(ty);
      if !seen.insert(ty) {
        return true;
      }

      match checker.store.type_kind(ty) {
        TypeKind::Object(_) | TypeKind::Mapped(_) => true,
        TypeKind::Union(members) | TypeKind::Intersection(members) => members
          .iter()
          .copied()
          .all(|member| is_object_like_jsx_spread_type(checker, member, seen, depth + 1)),
        _ => false,
      }
    }

    let prim = self.store.primitive_ids();
    let explicit_attr_count = attrs.len();
    let mut props = HashSet::new();
    let mut named_props = Vec::new();
    let mut shape = Shape::new();
    let mut spreads = Vec::new();
    let children_key = self.jsx_children_prop_key(loc);
    let children_prop_name = children_key.map(|key| self.store.name(key));
    let mut explicit_children_attr = false;
    let mut explicit_children_attr_loc: Option<Loc> = None;
    for attr in attrs {
      match attr {
        JsxAttr::Named { name, value } => {
          let key_string = if let Some(namespace) = name.stx.namespace.as_ref() {
            format!("{namespace}:{}", name.stx.name)
          } else {
            name.stx.name.clone()
          };
          if children_prop_name.as_deref() == Some(key_string.as_str()) {
            explicit_children_attr = true;
            let end = match value {
              None => name.loc.1,
              Some(JsxAttrVal::Text(text)) => text.loc.1,
              Some(JsxAttrVal::Expression(expr)) => expr.loc.1.saturating_add(1),
              Some(JsxAttrVal::Element(elem)) => elem.loc.1,
            };
            explicit_children_attr_loc = Some(Loc(name.loc.0, end));
          }
          // JSX attributes with hyphens (e.g. `data-test`) are permitted even when the props type
          // doesn't include a corresponding string-literal key. `tsc` excludes these keys from
          // excess property checking, so only track non-hyphenated attribute names here.
          if !key_string.contains('-') {
            if props.insert(key_string.clone()) {
              named_props.push((key_string.clone(), loc_to_range(self.file, name.loc)));
            }
          }
          let key = PropKey::String(self.store.intern_name_ref(&key_string));
          let value_ty = match value {
            None => self.store.intern_type(TypeKind::BooleanLiteral(true)),
            Some(JsxAttrVal::Text(text)) => self.jsx_attr_text_type(text),
            Some(JsxAttrVal::Expression(expr)) => {
              if is_empty_jsx_expr_placeholder(&expr.stx.value) {
                prim.unknown
              } else {
                let expected_ty = expected
                  .filter(|ty| {
                    !matches!(self.store.type_kind(*ty), TypeKind::Any | TypeKind::Unknown)
                  })
                  .map(|props_ty| self.member_type(props_ty, &key_string))
                  .unwrap_or(prim.unknown);
                let value_ty = self.check_expr_with_expected(&expr.stx.value, expected_ty);
                if expected_ty != prim.unknown
                  && !matches!(
                    self.store.type_kind(expected_ty),
                    TypeKind::Any | TypeKind::Unknown
                  )
                {
                  if let AstExpr::LitObj(obj) = expr.stx.value.stx.as_ref() {
                    if let Some(range) = self.excess_property_range(obj, expected_ty) {
                      self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                        "excess property",
                        Span::new(self.file, range),
                      ));
                    }
                  }
                }
                value_ty
              }
            }
            Some(JsxAttrVal::Element(elem)) => self.check_jsx_elem(elem),
          };
          shape.properties.push(types_ts_interned::Property {
            key,
            data: PropData {
              ty: value_ty,
              optional: false,
              readonly: false,
              accessibility: None,
              is_method: false,
              origin: None,
              declared_on: None,
            },
          });
        }
        JsxAttr::Spread { value } => {
          let expected_ty = expected
            .filter(|ty| !matches!(self.store.type_kind(*ty), TypeKind::Any | TypeKind::Unknown))
            .unwrap_or(prim.unknown);
          let spread_ty = self.check_expr_with_expected(&value.stx.value, expected_ty);
          if !matches!(self.store.type_kind(spread_ty), TypeKind::Any) {
            let mut seen = HashSet::new();
            if !is_object_like_jsx_spread_type(self, spread_ty, &mut seen, 0) {
              self.diagnostics.push(codes::JSX_SPREAD_ATTR_MUST_BE_OBJECT.error(
                "Spread types may only be created from object types.",
                Span::new(self.file, loc_to_range(self.file, value.stx.value.loc)),
              ));
            }
          }
          spreads.push(spread_ty);
        }
      }
    }

    if let Some(children_key_id) = children_key {
      let children_prop_name = children_prop_name.expect("children prop name");
      let expected_children_prop_ty = expected
        .filter(|ty| !matches!(self.store.type_kind(*ty), TypeKind::Any | TypeKind::Unknown))
        .map(|props_ty| self.member_type(props_ty, &children_prop_name));

      let semantic_children_ty = self.jsx_children_prop_type(children, expected_children_prop_ty);
      let has_semantic_children = semantic_children_ty.is_some();
      if let Some(children_ty) = semantic_children_ty {
        props.insert(children_prop_name.clone());
        let key = PropKey::String(children_key_id);
        shape.properties.push(types_ts_interned::Property {
          key,
          data: PropData {
            ty: children_ty,
            optional: false,
            readonly: false,
            accessibility: None,
            is_method: false,
            origin: None,
            declared_on: None,
          },
        });
      }

      if explicit_children_attr && has_semantic_children {
        let span_loc = explicit_children_attr_loc.unwrap_or(loc);
        self
          .diagnostics
          .push(codes::JSX_CHILDREN_SPECIFIED_TWICE.error(
            format!(
              "'{children_prop_name}' are specified twice. The attribute named '{children_prop_name}' will be overwritten."
            ),
            Span::new(self.file, loc_to_range(self.file, span_loc)),
          ));
      }
    } else {
      let _ = self.jsx_children_prop_type(children, None);
    }

    let shape_id = self.store.intern_shape(shape);
    let obj = self.store.intern_object(ObjectType { shape: shape_id });
    let mut ty = self.store.intern_type(TypeKind::Object(obj));
    if !spreads.is_empty() {
      spreads.insert(0, ty);
      ty = self.store.intersection(spreads);
    }
    JsxActualProps {
      ty,
      props,
      named_props,
      explicit_attr_count,
    }
  }

  fn jsx_children_prop_type(
    &mut self,
    children: &[JsxElemChild],
    expected: Option<TypeId>,
  ) -> Option<TypeId> {
    let prim = self.store.primitive_ids();
    let mut semantic_children = Vec::new();
    let mut has_spread_child = false;
    for (idx, child) in children.iter().enumerate() {
      match child {
        JsxElemChild::Text(text) => {
          if !text.stx.value.trim().is_empty() {
            semantic_children.push(idx);
          }
        }
        JsxElemChild::Expr(expr) => {
          if is_empty_jsx_expr_placeholder(&expr.stx.value) {
            continue;
          }
          if expr.stx.spread {
            has_spread_child = true;
          }
          semantic_children.push(idx);
        }
        JsxElemChild::Element(_) => semantic_children.push(idx),
      }
    }

    if semantic_children.is_empty() {
      return None;
    }

    // `member_type` adds `undefined` when `children` is optional. For contextual
    // typing we want the element type itself (e.g. tuple indexing) rather than
    // immediately collapsing to `unknown` due to union-with-undefined.
    let expected_children_ty_raw = expected.unwrap_or(prim.unknown);
    let expected_children_ty = match narrow_non_nullish(expected_children_ty_raw, &self.store).0 {
      ty if ty != prim.never => ty,
      _ => expected_children_ty_raw,
    };
    let (expected_is_array_like, expected_is_tuple_like) = {
      let mut queue: VecDeque<TypeId> = VecDeque::from([expected_children_ty]);
      let mut seen = HashSet::new();
      let mut array_like = false;
      let mut tuple_like = false;
      while let Some(ty) = queue.pop_front() {
        let ty = self.expand_ref(ty);
        if !seen.insert(ty) {
          continue;
        }
        match self.store.type_kind(ty) {
          TypeKind::Tuple(_) => {
            array_like = true;
            tuple_like = true;
          }
          TypeKind::Array { .. } => {
            array_like = true;
          }
          TypeKind::Union(members) | TypeKind::Intersection(members) => {
            for member in members {
              queue.push_back(member);
            }
          }
          _ => {}
        }
        if tuple_like {
          break;
        }
      }
      (array_like, tuple_like)
    };

    let expected_spread_ty = if expected_is_array_like {
      expected_children_ty
    } else {
      prim.unknown
    };

    let semantic_len = semantic_children.len();
    let should_return_tuple = semantic_len > 1 && !has_spread_child && expected_is_tuple_like;
    let should_check_children_assignability = should_return_tuple
      && {
        let mut queue: VecDeque<TypeId> = VecDeque::from([expected_children_ty]);
        let mut seen = HashSet::new();
        let mut found_type_param = false;
        while let Some(ty) = queue.pop_front() {
          let ty = self.expand_ref(ty);
          if !seen.insert(ty) {
            continue;
          }
          match self.store.type_kind(ty) {
            TypeKind::TypeParam(_) => {
              found_type_param = true;
              break;
            }
            TypeKind::Infer { .. } => {
              found_type_param = true;
              break;
            }
            TypeKind::Tuple(elems) => {
              for elem in elems {
                queue.push_back(elem.ty);
              }
            }
            TypeKind::Array { ty, .. } => {
              queue.push_back(ty);
            }
            TypeKind::Ref { args, .. } => {
              queue.extend(args);
            }
            TypeKind::Object(obj_id) => {
              let shape = self.store.shape(self.store.object(obj_id).shape);
              for prop in shape.properties.iter() {
                queue.push_back(prop.data.ty);
              }
              for idx in shape.indexers.iter() {
                queue.push_back(idx.key_type);
                queue.push_back(idx.value_type);
              }
              for sig_id in shape
                .call_signatures
                .iter()
                .copied()
                .chain(shape.construct_signatures.iter().copied())
              {
                let sig = self.store.signature(sig_id);
                for param in sig.params.iter() {
                  queue.push_back(param.ty);
                }
                if let Some(this_param) = sig.this_param {
                  queue.push_back(this_param);
                }
                queue.push_back(sig.ret);
              }
            }
            TypeKind::Callable { overloads } => {
              for sig_id in overloads {
                let sig = self.store.signature(sig_id);
                for param in sig.params.iter() {
                  queue.push_back(param.ty);
                }
                queue.push_back(sig.ret);
              }
            }
            TypeKind::Union(members) | TypeKind::Intersection(members) => {
              for member in members {
                queue.push_back(member);
              }
            }
            TypeKind::Mapped(mapped) => {
              queue.push_back(mapped.source);
              queue.push_back(mapped.value);
              if let Some(name_type) = mapped.name_type {
                queue.push_back(name_type);
              }
              if let Some(as_type) = mapped.as_type {
                queue.push_back(as_type);
              }
            }
            TypeKind::TemplateLiteral(tpl) => {
              for span in tpl.spans {
                queue.push_back(span.ty);
              }
            }
            TypeKind::Intrinsic { ty, .. } => {
              queue.push_back(ty);
            }
            TypeKind::IndexedAccess { obj, index } => {
              queue.push_back(obj);
              queue.push_back(index);
            }
            TypeKind::KeyOf(inner) => {
              queue.push_back(inner);
            }
            TypeKind::Conditional {
              check,
              extends,
              true_ty,
              false_ty,
              ..
            } => {
              queue.push_back(check);
              queue.push_back(extends);
              queue.push_back(true_ty);
              queue.push_back(false_ty);
            }
            TypeKind::Predicate { asserted, .. } => {
              if let Some(asserted) = asserted {
                queue.push_back(asserted);
              }
            }
            _ => {}
          }
        }
        !found_type_param
      };

    let mut collected = Vec::new();

    for (semantic_idx, child_idx) in semantic_children.into_iter().enumerate() {
      match &children[child_idx] {
        JsxElemChild::Text(text) => {
          let expected_ty = if semantic_len == 1 {
            expected_children_ty
          } else if expected_is_array_like {
            let key_ty = self
              .store
              .intern_type(TypeKind::NumberLiteral(OrderedFloat::from(semantic_idx as f64)));
            self.member_type_for_index_key(expected_children_ty, key_ty)
          } else {
            expected_children_ty
          };

          let mut ty = self.jsx_child_text_type(text).unwrap_or(prim.unknown);
          if should_check_children_assignability
            && expected_ty != prim.unknown
            && !matches!(self.store.type_kind(expected_ty), TypeKind::Any | TypeKind::Unknown)
            && !matches!(self.store.type_kind(ty), TypeKind::Any | TypeKind::Unknown)
            && !self.relate.is_assignable(ty, expected_ty)
          {
            self.diagnostics.push(codes::TYPE_MISMATCH.error(
              "type mismatch",
              Span::new(self.file, loc_to_range(self.file, text.loc)),
            ));
            ty = expected_ty;
          }

          if should_return_tuple {
            collected.push(ty);
          } else if ty != prim.unknown {
            collected.push(ty);
          }
        }
        JsxElemChild::Expr(expr) => {
          let expected_ty = if expr.stx.spread {
            expected_spread_ty
          } else if semantic_len == 1 {
            expected_children_ty
          } else if expected_is_array_like {
            let key_ty = self
              .store
              .intern_type(TypeKind::NumberLiteral(OrderedFloat::from(semantic_idx as f64)));
            self.member_type_for_index_key(expected_children_ty, key_ty)
          } else {
            expected_children_ty
          };

          let expr_ty = self.check_expr_with_expected(&expr.stx.value, expected_ty);
          let mut forced_ty = None;

          if expr.stx.spread {
            let expanded = self.expand_ref(expr_ty);
            if !matches!(self.store.type_kind(expanded), TypeKind::Any)
              && !self.is_valid_jsx_spread_child_type(expanded)
            {
              let expr_range = loc_to_range(self.file, expr.loc);
              let spread_range = TextRange::new(
                expr_range.start.saturating_sub(4),
                expr_range.end.saturating_add(1),
              );
              self
                .diagnostics
                .push(codes::JSX_SPREAD_CHILD_MUST_BE_ARRAY.error(
                  "JSX spread child must be an array type.",
                  Span::new(self.file, spread_range),
                ));
            }
          }

          if should_check_children_assignability && !expr.stx.spread && expected_ty != prim.unknown {
            let before = self.diagnostics.len();
            // `parse-js` assigns JSX expression containers a span that excludes the surrounding
            // `{`/`}` tokens. `tsc` anchors diagnostics on the full container (including braces),
            // so expand the span by one byte on each side for JSX child assignability checks.
            let container = loc_to_range(self.file, expr.loc);
            let container = TextRange::new(
              container.start.saturating_sub(1),
              container.end.saturating_add(1),
            );
            self.check_assignable(
              &expr.stx.value,
              expr_ty,
              expected_ty,
              Some(container),
            );
            if self.diagnostics.len() > before {
              forced_ty = Some(expected_ty);
            }
          }
          if !expr.stx.spread
            && !should_check_children_assignability
            && expected_ty != prim.unknown
            && !matches!(
              self.store.type_kind(expected_ty),
              TypeKind::Any | TypeKind::Unknown
            )
          {
            if let AstExpr::LitObj(obj) = expr.stx.value.stx.as_ref() {
              if let Some(range) = self.excess_property_range(obj, expected_ty) {
                self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                  "excess property",
                  Span::new(self.file, range),
                ));
              }
            }
          }

          let ty = if expr.stx.spread {
            self.spread_element_type(expr_ty)
          } else {
            forced_ty.unwrap_or(expr_ty)
          };

          if should_return_tuple {
            collected.push(ty);
          } else if ty != prim.unknown {
            collected.push(ty);
          }
        }
        JsxElemChild::Element(elem) => {
          let expected_ty = if semantic_len == 1 {
            expected_children_ty
          } else if expected_is_array_like {
            let key_ty = self
              .store
              .intern_type(TypeKind::NumberLiteral(OrderedFloat::from(semantic_idx as f64)));
            self.member_type_for_index_key(expected_children_ty, key_ty)
          } else {
            expected_children_ty
          };

          let mut ty = self.check_jsx_elem(elem);
          if should_check_children_assignability
            && expected_ty != prim.unknown
            && !matches!(self.store.type_kind(expected_ty), TypeKind::Any | TypeKind::Unknown)
            && !matches!(self.store.type_kind(ty), TypeKind::Any | TypeKind::Unknown)
            && !self.relate.is_assignable(ty, expected_ty)
          {
            self.diagnostics.push(codes::TYPE_MISMATCH.error(
              "type mismatch",
              Span::new(self.file, loc_to_range(self.file, elem.loc)),
            ));
            ty = expected_ty;
          }

          collected.push(ty);
        }
      }
    }

    if collected.is_empty() {
      return None;
    }

    if has_spread_child {
      let ty = self.store.union(collected);
      return Some(self.store.intern_type(TypeKind::Array {
        ty,
        readonly: false,
      }));
    }

    if semantic_len > 1 {
      if should_return_tuple {
        let elems = collected
          .into_iter()
          .map(|ty| types_ts_interned::TupleElem {
            ty,
            optional: false,
            rest: false,
            readonly: false,
          })
          .collect();
        return Some(self.store.intern_type(TypeKind::Tuple(elems)));
      }

      let ty = self.store.union(collected);
      return Some(self.store.intern_type(TypeKind::Array {
        ty,
        readonly: false,
      }));
    }

    Some(collected[0])
  }

  fn is_valid_jsx_spread_child_type(&self, ty: TypeId) -> bool {
    let ty = self.expand_ref(ty);
    match self.store.type_kind(ty) {
      TypeKind::Array { .. } | TypeKind::Tuple(_) => true,
      TypeKind::Union(members) | TypeKind::Intersection(members) => {
        members
          .into_iter()
          .all(|member| self.is_valid_jsx_spread_child_type(member))
      }
      _ => false,
    }
  }

  fn jsx_attr_text_type(&mut self, text: &Node<JsxText>) -> TypeId {
    let name = self.store.intern_name_ref(&text.stx.value);
    self.store.intern_type(TypeKind::StringLiteral(name))
  }

  fn jsx_child_text_type(&mut self, text: &Node<JsxText>) -> Option<TypeId> {
    let prim = self.store.primitive_ids();
    let trimmed = text.stx.value.trim();
    if trimmed.is_empty() {
      return None;
    }
    Some(prim.string)
  }

  fn jsx_first_missing_required_prop(&self, target: TypeId, actual_ty: TypeId) -> Option<String> {
    let mut required = Vec::new();
    let mut seen = HashSet::new();
    self.jsx_collect_required_props(target, &mut required, &mut seen);
    required.sort();
    required.dedup();
    required
      .into_iter()
      .find(|prop| !self.type_has_prop(actual_ty, prop.as_str()))
  }

  fn jsx_collect_required_props(
    &self,
    target: TypeId,
    out: &mut Vec<String>,
    seen: &mut HashSet<TypeId>,
  ) {
    let target = self.expand_ref(target);
    let expanded = self.expand_for_props(target);
    if expanded != target {
      self.jsx_collect_required_props(expanded, out, seen);
      return;
    }
    if !seen.insert(target) {
      return;
    }
    match self.store.type_kind(target) {
      TypeKind::Object(obj_id) => {
        let shape = self.store.shape(self.store.object(obj_id).shape);
        for prop in shape.properties.iter() {
          if prop.data.optional {
            continue;
          }
          match prop.key {
            PropKey::String(name) | PropKey::Symbol(name) => out.push(self.store.name(name)),
            PropKey::Number(num) => out.push(num.to_string()),
          }
        }
      }
      TypeKind::Intersection(members) => {
        for member in members {
          self.jsx_collect_required_props(member, out, seen);
        }
      }
      // Required property sets for unions depend on which branch is selected, so
      // avoid reporting TS2741 in that case and fall back to assignability
      // diagnostics.
      TypeKind::Union(_) => {}
      _ => {}
    }
  }

  fn check_jsx_props(&mut self, loc: Loc, actual: &JsxActualProps, expected: TypeId) {
    if matches!(
      self.store.type_kind(expected),
      TypeKind::Any | TypeKind::Unknown
    ) {
      return;
    }
    // In `tsc`, spreading an `any` value into the JSX attributes object makes the entire props
    // type `any`, which bypasses both excess-property and assignability checks. Mirror that by
    // skipping all JSX props diagnostics when the computed props type is `any`.
    if matches!(self.store.type_kind(actual.ty), TypeKind::Any) {
      return;
    }
    if !matches!(
      self.store.type_kind(actual.ty),
      TypeKind::Any | TypeKind::Unknown
    ) {
      if let Some(missing) = self.jsx_first_missing_required_prop(expected, actual.ty) {
        self.diagnostics.push(codes::MISSING_REQUIRED_PROPERTY.error(
          format!("Property '{missing}' is missing in JSX props."),
          Span::new(self.file, loc_to_range(self.file, loc)),
        ));
        return;
      }
    }

    let filtered;
    let props_for_excess_check = if actual.props.iter().any(|p| p.contains('-')) {
      filtered = actual
        .props
        .iter()
        .filter(|p| !p.contains('-'))
        .cloned()
        .collect::<HashSet<String>>();
      &filtered
    } else {
      &actual.props
    };

    if !self.type_accepts_props(expected, props_for_excess_check) {
      let mut single = HashSet::with_capacity(1);
      let mut range = None;
      for (prop, prop_range) in actual.named_props.iter() {
        if !props_for_excess_check.contains(prop) {
          continue;
        }
        single.clear();
        single.insert(prop.clone());
        if !self.type_accepts_props(expected, &single) {
          range = Some(*prop_range);
          break;
        }
      }
      self.diagnostics.push(codes::EXCESS_PROPERTY.error(
        "excess property",
        Span::new(self.file, range.unwrap_or_else(|| loc_to_range(self.file, loc))),
      ));
      return;
    }
    if matches!(
      self.store.type_kind(actual.ty),
      TypeKind::Any | TypeKind::Unknown
    ) {
      return;
    }
    if self.relate.is_assignable(actual.ty, expected) {
      return;
    }
    self.diagnostics.push(codes::TYPE_MISMATCH.error(
      "type mismatch",
      Span::new(self.file, loc_to_range(self.file, loc)),
    ));
  }

  fn jsx_apply_intrinsic_attributes(&mut self, expected: TypeId) -> TypeId {
    if matches!(
      self.store.type_kind(expected),
      TypeKind::Any | TypeKind::Unknown
    ) {
      return expected;
    }
    let intrinsic = self.jsx_intrinsic_attributes_type();
    if matches!(self.store.type_kind(intrinsic), TypeKind::EmptyObject) {
      return expected;
    }
    self.store.intersection(vec![expected, intrinsic])
  }

  fn check_jsx_component(
    &mut self,
    component_ty: TypeId,
    actual_props: &JsxActualProps,
    element_ty: TypeId,
    elem_loc: Loc,
    tag_loc: Loc,
  ) {
    let span = Span::new(self.file, loc_to_range(self.file, tag_loc));
    let elem_span = Span::new(self.file, loc_to_range(self.file, elem_loc));
    let prim = self.store.primitive_ids();
    if matches!(
      self.store.type_kind(component_ty),
      TypeKind::Any | TypeKind::Unknown
    ) {
      return;
    }

    let call_sigs = self.jsx_component_call_signatures(component_ty);
    let is_construct = call_sigs.is_empty();
    let (sigs, contextual_return) = if !is_construct {
      let contextual_return = !matches!(
        self.store.type_kind(element_ty),
        TypeKind::Any | TypeKind::Unknown
      );
      (call_sigs, contextual_return)
    } else {
      (self.jsx_component_construct_signatures(component_ty), false)
    };

    if sigs.is_empty() {
      self.diagnostics.push(
        codes::NO_OVERLOAD
          .error("JSX component is not callable or constructable", span)
          .with_note(format!(
            "component has type {}",
            TypeDisplay::new(self.store.as_ref(), component_ty)
          )),
      );
      return;
    }

    let empty_props = {
      let shape_id = self.store.intern_shape(Shape::new());
      let obj = self.store.intern_object(ObjectType { shape: shape_id });
      self.store.intern_type(TypeKind::Object(obj))
    };

    let element_class_ty = is_construct.then(|| self.jsx_element_class_type());
    let enforce_element_class = element_class_ty
      .is_some_and(|ty| !matches!(self.store.type_kind(ty), TypeKind::Any | TypeKind::Unknown));

    let args = [CallArgType::new(actual_props.ty)];
    let contextual_return_ty = contextual_return.then_some(element_ty);
    let valid_return_ty =
      contextual_return_ty.map(|el_ty| self.store.union(vec![el_ty, prim.null]));
    let mut filtered_props: Vec<TypeId> = Vec::new();
    let mut all_props: Vec<TypeId> = Vec::new();
    let mut saw_valid_return = false;
    for sig_id in sigs.iter().copied() {
      let sig = self.store.signature(sig_id);
      let mut props_ty = sig.params.first().map(|p| p.ty).unwrap_or(empty_props);
      let mut ret_ty = sig.ret;
      if !sig.type_params.is_empty() && !sig.params.is_empty() {
        let inference = infer_type_arguments_for_call(
          &self.store,
          &self.relate,
          &sig,
          &args,
          None,
          contextual_return_ty,
        );
        let mut substituter = Substituter::new(Arc::clone(&self.store), inference.substitutions);
        props_ty = substituter.substitute_type(props_ty);
        ret_ty = substituter.substitute_type(ret_ty);
      }
      if enforce_element_class
        && !matches!(
          self.store.type_kind(ret_ty),
          TypeKind::Any | TypeKind::Unknown
        )
      {
        let class_ty = element_class_ty.expect("enforced element class type");
        if !self.relate.is_assignable(ret_ty, class_ty) {
          continue;
        }
      }
      if is_construct {
        match self.jsx_element_attributes_prop_name(elem_loc) {
          JsxAttributesPropertyName::Missing => {}
          JsxAttributesPropertyName::Empty => {
            props_ty = ret_ty;
          }
          JsxAttributesPropertyName::Name(attrs_prop) => {
            let prop_name = self.store.name(attrs_prop);
            if self.type_has_prop(ret_ty, &prop_name) {
              props_ty = self.member_type(ret_ty, &prop_name);
            } else {
              if actual_props.explicit_attr_count > 0 {
                self.diagnostics.push(codes::JSX_ELEMENT_CLASS_DOES_NOT_SUPPORT_ATTRIBUTES.error(
                  format!(
                    "JSX element class does not support attributes because it does not have a '{prop_name}' property.",
                  ),
                  elem_span,
                ));
                return;
              }
              // We can't determine a props type when the required instance member is missing.
              // TypeScript treats the expected props as `unknown` (and only emits TS2607 when
              // explicit attributes are present).
              all_props.push(prim.unknown);
              continue;
            }
          }
        };
      }
      props_ty = self.jsx_apply_library_managed_attributes(component_ty, props_ty);
      if is_construct {
        let class_attrs = self.jsx_intrinsic_class_attributes_type(ret_ty);
        if !matches!(self.store.type_kind(class_attrs), TypeKind::EmptyObject) {
          props_ty = self.store.intersection(vec![props_ty, class_attrs]);
        }
      }
      all_props.push(props_ty);
      if let Some(valid_return) = valid_return_ty {
        let return_ok = matches!(
          self.store.type_kind(ret_ty),
          TypeKind::Any | TypeKind::Unknown
        ) || self.relate.is_assignable(ret_ty, valid_return);
        if return_ok {
          saw_valid_return = true;
          filtered_props.push(props_ty);
        }
      }
    }
    if enforce_element_class && all_props.is_empty() {
      let class_ty = element_class_ty.expect("enforced element class type");
      self.diagnostics.push(
        codes::NO_OVERLOAD
          .error(
            "JSX class component does not satisfy JSX.ElementClass",
            span,
          )
          .with_note(format!(
            "expected JSX.ElementClass {}, got component type {}",
            TypeDisplay::new(self.store.as_ref(), class_ty),
            TypeDisplay::new(self.store.as_ref(), component_ty)
          )),
      );
      return;
    }
    if valid_return_ty.is_some() && !saw_valid_return {
      let expected = valid_return_ty.expect("return type computed");
      self.diagnostics.push(
        codes::NO_OVERLOAD
          .error("JSX component return type is not a valid JSX element", span)
          .with_note(format!(
            "expected return type assignable to {}, got component type {}",
            TypeDisplay::new(self.store.as_ref(), expected),
            TypeDisplay::new(self.store.as_ref(), component_ty),
          )),
      );
      return;
    }
    let mut props = if contextual_return_ty.is_some() && !filtered_props.is_empty() {
      filtered_props
    } else {
      all_props
    };
    props.sort();
    props.dedup();
    let expected_props = if props.len() == 1 {
      props[0]
    } else {
      self.store.union(props)
    };
    if expected_props == prim.unknown {
      return;
    }
    let expected_props = self.jsx_apply_intrinsic_attributes(expected_props);
    self.check_jsx_props(tag_loc, actual_props, expected_props);
  }

  fn jsx_component_call_signatures(&self, ty: TypeId) -> Vec<SignatureId> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    self.jsx_collect_call_signatures(ty, &mut out, &mut seen);
    out.sort();
    out.dedup();
    out
  }

  fn jsx_collect_call_signatures(
    &self,
    ty: TypeId,
    out: &mut Vec<SignatureId>,
    seen: &mut HashSet<TypeId>,
  ) {
    if !seen.insert(ty) {
      return;
    }
    let expanded = self.expand_for_props(ty);
    if expanded != ty {
      self.jsx_collect_call_signatures(expanded, out, seen);
      return;
    }
    match self.store.type_kind(ty) {
      TypeKind::Callable { overloads } => out.extend(overloads),
      TypeKind::Object(obj) => {
        let shape = self.store.shape(self.store.object(obj).shape);
        out.extend(shape.call_signatures);
      }
      TypeKind::Union(members) | TypeKind::Intersection(members) => {
        for member in members {
          self.jsx_collect_call_signatures(member, out, seen);
        }
      }
      TypeKind::Ref { def, args } => {
        if let Some(expander) = self.ref_expander {
          if let Some(expanded) = expander.expand_ref(self.store.as_ref(), def, &args) {
            self.jsx_collect_call_signatures(expanded, out, seen);
          }
        }
      }
      _ => {}
    }
  }

  fn jsx_component_construct_signatures(&self, ty: TypeId) -> Vec<SignatureId> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    self.jsx_collect_construct_signatures(ty, &mut out, &mut seen);
    out.sort();
    out.dedup();
    out
  }

  fn jsx_collect_construct_signatures(
    &self,
    ty: TypeId,
    out: &mut Vec<SignatureId>,
    seen: &mut HashSet<TypeId>,
  ) {
    if !seen.insert(ty) {
      return;
    }
    let expanded = self.expand_for_props(ty);
    if expanded != ty {
      self.jsx_collect_construct_signatures(expanded, out, seen);
      return;
    }
    match self.store.type_kind(ty) {
      TypeKind::Object(obj) => {
        let shape = self.store.shape(self.store.object(obj).shape);
        out.extend(shape.construct_signatures);
      }
      TypeKind::Union(members) | TypeKind::Intersection(members) => {
        for member in members {
          self.jsx_collect_construct_signatures(member, out, seen);
        }
      }
      TypeKind::Ref { def, args } => {
        if let Some(expander) = self.ref_expander {
          if let Some(expanded) = expander.expand_ref(self.store.as_ref(), def, &args) {
            self.jsx_collect_construct_signatures(expanded, out, seen);
          }
        }
      }
      _ => {}
    }
  }

  fn resolve_type_ref(&mut self, path: &[&str]) -> Option<TypeId> {
    let resolver = self.type_resolver.as_ref()?;
    let segments: Vec<String> = path.iter().map(|s| s.to_string()).collect();
    let def = resolver.resolve_type_name(&segments)?;
    Some(self.store.canon(self.store.intern_type(TypeKind::Ref {
      def,
      args: Vec::new(),
    })))
  }

  fn resolve_jsx_type_ref(&mut self, path: &[&str]) -> Option<TypeId> {
    let resolver = self.type_resolver.as_ref()?;
    let segments: Vec<String> = path.iter().map(|s| s.to_string()).collect();

    if let Some(module) = self.jsx_runtime_module.as_deref() {
      if let Some(def) = resolver.resolve_import_type(module, Some(&segments)) {
        return Some(self.store.canon(self.store.intern_type(TypeKind::Ref {
          def,
          args: Vec::new(),
        })));
      }
    }

    self.resolve_type_ref(path)
  }

  fn check_jsx_runtime_module(&mut self, loc: Loc) {
    let Some(module) = self.jsx_runtime_module.as_deref() else {
      return;
    };
    if self.jsx_runtime_module_exists == Some(true) {
      return;
    }

    if self.jsx_runtime_module_exists.is_none() {
      let Some(resolver) = self.type_resolver.as_ref() else {
        return;
      };
      self.jsx_runtime_module_exists = Some(resolver.resolve_import_typeof(module, None).is_some());
    }

    if self.jsx_runtime_module_exists == Some(false) && !self.jsx_runtime_module_missing_reported {
      self.jsx_runtime_module_missing_reported = true;
      self.diagnostics.push(codes::JSX_RUNTIME_MODULE_MISSING.error(
        format!(
          "This JSX tag requires the module path '{module}' to exist, but none could be found. Make sure you have types for the appropriate package installed."
        ),
        Span::new(self.file, loc_to_range(self.file, loc)),
      ));
    }
  }

  fn report_jsx_namespace_missing(&mut self, loc: Loc) {
    if self.jsx_namespace_missing_reported {
      return;
    }
    self.jsx_namespace_missing_reported = true;
    self.diagnostics.push(codes::JSX_NAMESPACE_MISSING.error(
      "missing JSX namespace typings",
      Span::new(self.file, loc_to_range(self.file, loc)),
    ));
  }

  fn jsx_element_type_constraint_type(&mut self) -> Option<TypeId> {
    if let Some(cached) = self.jsx_element_type_constraint_ty {
      return cached;
    }
    let resolved = self.resolve_jsx_type_ref(&["JSX", "ElementType"]);
    let ty = resolved
      .map(|ty| self.expand_ref(ty))
      .and_then(|ty| match self.store.type_kind(ty) {
        TypeKind::Any | TypeKind::Unknown => None,
        _ => Some(ty),
      });
    self.jsx_element_type_constraint_ty = Some(ty);
    ty
  }

  fn jsx_element_type(&mut self, loc: Loc) -> TypeId {
    if let Some(ty) = self.jsx_element_ty {
      return ty;
    }
    let prim = self.store.primitive_ids();
    let resolved = self.resolve_jsx_type_ref(&["JSX", "Element"]);
    if resolved.is_none() {
      self.report_jsx_namespace_missing(loc);
    }
    let ty = resolved.unwrap_or(prim.unknown);
    self.jsx_element_ty = Some(ty);
    ty
  }

  fn jsx_element_class_type(&mut self) -> TypeId {
    if let Some(ty) = self.jsx_element_class_ty {
      return ty;
    }
    let prim = self.store.primitive_ids();
    let ty = self
      .resolve_jsx_type_ref(&["JSX", "ElementClass"])
      .unwrap_or(prim.unknown);
    self.jsx_element_class_ty = Some(ty);
    ty
  }

  fn jsx_intrinsic_elements_type(&mut self, loc: Loc) -> TypeId {
    if let Some(ty) = self.jsx_intrinsic_elements_ty {
      return ty;
    }
    let prim = self.store.primitive_ids();
    let resolved = self.resolve_jsx_type_ref(&["JSX", "IntrinsicElements"]);
    if resolved.is_none() {
      self.report_jsx_namespace_missing(loc);
    }
    let ty = resolved.unwrap_or(prim.unknown);
    self.jsx_intrinsic_elements_ty = Some(ty);
    ty
  }

  fn jsx_intrinsic_attributes_type(&mut self) -> TypeId {
    if let Some(ty) = self.jsx_intrinsic_attributes_ty {
      return ty;
    }
    // `JSX.IntrinsicAttributes` is optional; when absent treat it as `{}` so it
    // neither contributes additional props nor disables checks.
    let ty = self
      .resolve_jsx_type_ref(&["JSX", "IntrinsicAttributes"])
      .unwrap_or_else(|| self.store.intern_type(TypeKind::EmptyObject));
    self.jsx_intrinsic_attributes_ty = Some(ty);
    ty
  }

  fn jsx_library_managed_attributes_def_id(&mut self) -> Option<DefId> {
    if let Some(cached) = self.jsx_library_managed_attributes_def.as_ref() {
      return *cached;
    }
    let def = self
      .resolve_jsx_type_ref(&["JSX", "LibraryManagedAttributes"])
      .and_then(|ty| match self.store.type_kind(ty) {
        TypeKind::Ref { def, args } if args.is_empty() => Some(def),
        _ => None,
      });
    self.jsx_library_managed_attributes_def = Some(def);
    def
  }

  fn jsx_apply_library_managed_attributes(&mut self, component: TypeId, props: TypeId) -> TypeId {
    if matches!(
      self.store.type_kind(props),
      TypeKind::Any | TypeKind::Unknown
    ) {
      return props;
    }
    let Some(def) = self.jsx_library_managed_attributes_def_id() else {
      return props;
    };
    self.store.canon(self.store.intern_type(TypeKind::Ref {
      def,
      args: vec![component, props],
    }))
  }

  fn jsx_intrinsic_class_attributes_def_id(&mut self) -> Option<DefId> {
    if let Some(cached) = self.jsx_intrinsic_class_attributes_def.as_ref() {
      return *cached;
    }
    let def = self
      .resolve_jsx_type_ref(&["JSX", "IntrinsicClassAttributes"])
      .and_then(|ty| match self.store.type_kind(ty) {
        TypeKind::Ref { def, args } if args.is_empty() => Some(def),
        _ => None,
      });
    self.jsx_intrinsic_class_attributes_def = Some(def);
    def
  }

  fn jsx_intrinsic_class_attributes_type(&mut self, instance: TypeId) -> TypeId {
    let Some(def) = self.jsx_intrinsic_class_attributes_def_id() else {
      return self.store.intern_type(TypeKind::EmptyObject);
    };
    self.store.canon(self.store.intern_type(TypeKind::Ref {
      def,
      args: vec![instance],
    }))
  }

  fn jsx_element_attributes_prop_name(&mut self, loc: Loc) -> JsxAttributesPropertyName {
    if let Some(cached) = self.jsx_element_attributes_prop_name {
      return cached;
    }
    let Some(attrs_ty) = self.resolve_jsx_type_ref(&["JSX", "ElementAttributesProperty"]) else {
      let name = JsxAttributesPropertyName::Missing;
      self.jsx_element_attributes_prop_name = Some(name);
      return name;
    };
    // TypeScript reports TS2608 (global JSX container type has multiple
    // properties) at the declaration site. When `skipLibCheck` is enabled, those
    // diagnostics are suppressed because they originate from a `.d.ts` file;
    // mirror that by anchoring diagnostics on the resolved declaration span
    // whenever possible.
    let container_span = match self.store.type_kind(attrs_ty) {
      TypeKind::Ref { def, args } if args.is_empty() => self
        .type_resolver
        .as_ref()
        .and_then(|resolver| resolver.span_of_def(def))
        .unwrap_or_else(|| Span::new(self.file, loc_to_range(self.file, loc))),
      _ => Span::new(self.file, loc_to_range(self.file, loc)),
    };
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    self.jsx_collect_children_attribute_keys(attrs_ty, &mut candidates, &mut seen);
    candidates.sort();
    candidates.dedup();

    let resolved = match candidates.as_slice() {
      [] => JsxAttributesPropertyName::Empty,
      [only] => JsxAttributesPropertyName::Name(self.store.intern_name_ref(only)),
      _ => {
        self.diagnostics.push(
          codes::JSX_GLOBAL_TYPE_MAY_NOT_HAVE_MORE_THAN_ONE_PROPERTY.error(
            "The global type 'JSX.ElementAttributesProperty' may not have more than one property.",
            container_span,
          ),
        );
        JsxAttributesPropertyName::Missing
      }
    };

    self.jsx_element_attributes_prop_name = Some(resolved);
    resolved
  }

  fn jsx_children_prop_key(&mut self, loc: Loc) -> Option<TsNameId> {
    if let Some(cached) = self.jsx_children_prop_name {
      return cached;
    }
    if matches!(self.jsx_mode, Some(JsxMode::ReactJsx | JsxMode::ReactJsxdev)) {
      let children = self.store.intern_name_ref("children");
      let selected = Some(children);
      self.jsx_children_prop_name = Some(selected);
      return selected;
    }

    let Some(children_attr_ty) = self.resolve_jsx_type_ref(&["JSX", "ElementChildrenAttribute"])
    else {
      let children = self.store.intern_name_ref("children");
      let selected = Some(children);
      self.jsx_children_prop_name = Some(selected);
      return selected;
    };
    // See note in `jsx_element_attributes_prop_name` about anchoring TS2608
    // diagnostics for `skipLibCheck` fidelity.
    let container_span = match self.store.type_kind(children_attr_ty) {
      TypeKind::Ref { def, args } if args.is_empty() => self
        .type_resolver
        .as_ref()
        .and_then(|resolver| resolver.span_of_def(def))
        .unwrap_or_else(|| Span::new(self.file, loc_to_range(self.file, loc))),
      _ => Span::new(self.file, loc_to_range(self.file, loc)),
    };

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    self.jsx_collect_children_attribute_keys(children_attr_ty, &mut candidates, &mut seen);
    candidates.sort();
    candidates.dedup();
    let selected = match candidates.as_slice() {
      [] => None,
      [only] => Some(self.store.intern_name_ref(only)),
      _ => {
        self.diagnostics.push(
          codes::JSX_GLOBAL_TYPE_MAY_NOT_HAVE_MORE_THAN_ONE_PROPERTY.error(
            "The global type 'JSX.ElementChildrenAttribute' may not have more than one property.",
            container_span,
          ),
        );
        None
      }
    };

    self.jsx_children_prop_name = Some(selected);
    selected
  }

  fn jsx_collect_children_attribute_keys(
    &self,
    ty: TypeId,
    out: &mut Vec<String>,
    seen: &mut HashSet<TypeId>,
  ) {
    if !seen.insert(ty) {
      return;
    }
    let expanded = self.expand_for_props(ty);
    if expanded != ty {
      self.jsx_collect_children_attribute_keys(expanded, out, seen);
      return;
    }
    match self.store.type_kind(ty) {
      TypeKind::Object(obj_id) => {
        let shape = self.store.shape(self.store.object(obj_id).shape);
        for prop in shape.properties.iter() {
          match prop.key {
            PropKey::String(name) | PropKey::Symbol(name) => out.push(self.store.name(name)),
            PropKey::Number(num) => out.push(num.to_string()),
          }
        }
      }
      TypeKind::Union(members) | TypeKind::Intersection(members) => {
        for member in members {
          self.jsx_collect_children_attribute_keys(member, out, seen);
        }
      }
      TypeKind::Ref { def, args } => {
        if let Some(expander) = self.ref_expander {
          if let Some(expanded) = expander.expand_ref(self.store.as_ref(), def, &args) {
            self.jsx_collect_children_attribute_keys(expanded, out, seen);
          }
        }
      }
      _ => {}
    }
  }

  fn constant_computed_key_name(&self, expr: &Node<AstExpr>) -> Option<String> {
    match expr.stx.as_ref() {
      AstExpr::LitStr(str_lit) => Some(str_lit.stx.value.clone()),
      AstExpr::LitNum(num_lit) => Some(num_lit.stx.value.0.to_string()),
      AstExpr::LitTemplate(tpl) => {
        let mut out = String::new();
        for part in tpl.stx.parts.iter() {
          match part {
            parse_js::ast::expr::lit::LitTemplatePart::String(s) => out.push_str(s),
            parse_js::ast::expr::lit::LitTemplatePart::Substitution(_) => return None,
          }
        }
        Some(out)
      }
      AstExpr::TypeAssertion(assert) => self.constant_computed_key_name(&assert.stx.expression),
      AstExpr::NonNullAssertion(assert) => self.constant_computed_key_name(&assert.stx.expression),
      AstExpr::SatisfiesExpr(expr) => self.constant_computed_key_name(&expr.stx.expression),
      _ => None,
    }
  }

  fn const_inference_type(&self, expr: &Node<AstExpr>) -> TypeId {
    let prim = self.store.primitive_ids();
    match expr.stx.as_ref() {
      AstExpr::LitNum(num) => self
        .store
        .intern_type(TypeKind::NumberLiteral(OrderedFloat::from(num.stx.value.0))),
      AstExpr::LitStr(str_lit) => {
        let name = self.store.intern_name_ref(&str_lit.stx.value);
        self.store.intern_type(TypeKind::StringLiteral(name))
      }
      AstExpr::LitBool(b) => self
        .store
        .intern_type(TypeKind::BooleanLiteral(b.stx.value)),
      AstExpr::LitNull(_) => prim.null,
      AstExpr::LitBigInt(value) => {
        let trimmed = value.stx.value.trim_end_matches('n');
        let parsed = trimmed
          .parse::<BigInt>()
          .unwrap_or_else(|_| BigInt::from(0u8));
        self.store.intern_type(TypeKind::BigIntLiteral(parsed))
      }
      AstExpr::LitArr(arr) => {
        let mut elems = Vec::new();
        for elem in arr.stx.elements.iter() {
          match elem {
            parse_js::ast::expr::lit::LitArrElem::Single(v) => {
              elems.push(types_ts_interned::TupleElem {
                ty: self.const_inference_type(v),
                optional: false,
                rest: false,
                readonly: true,
              })
            }
            parse_js::ast::expr::lit::LitArrElem::Rest(v) => {
              elems.push(types_ts_interned::TupleElem {
                ty: self.const_inference_type(v),
                optional: false,
                rest: true,
                readonly: true,
              })
            }
            parse_js::ast::expr::lit::LitArrElem::Empty => {
              elems.push(types_ts_interned::TupleElem {
                ty: prim.undefined,
                optional: true,
                rest: false,
                readonly: true,
              })
            }
          }
        }
        self.store.intern_type(TypeKind::Tuple(elems))
      }
      AstExpr::LitObj(obj) => {
        let mut shape = Shape::new();
        for member in obj.stx.members.iter() {
          match &member.stx.typ {
            ObjMemberType::Valued { key, val } => {
              let prop_key = match key {
                ClassOrObjKey::Direct(direct) => {
                  Some(PropKey::String(self.store.intern_name_ref(&direct.stx.key)))
                }
                ClassOrObjKey::Computed(expr) => {
                  if let Some(literal) = self.constant_computed_key_name(expr) {
                    Some(PropKey::String(self.store.intern_name_ref(&literal)))
                  } else {
                    let key_ty = self.const_inference_type(expr);
                    match self.store.type_kind(key_ty) {
                    TypeKind::StringLiteral(id) => Some(PropKey::String(id)),
                    TypeKind::NumberLiteral(num) => Some(PropKey::String(
                      self.store.intern_name(num.0.to_string()),
                    )),
                    _ => None,
                    }
                  }
                }
              };

              if let (Some(prop_key), ClassOrObjVal::Prop(Some(value))) = (prop_key, val) {
                let value_ty = self.const_inference_type(value);
                shape.properties.push(types_ts_interned::Property {
                  key: prop_key,
                  data: PropData {
                    ty: value_ty,
                    optional: false,
                    readonly: true,
                    accessibility: None,
                    is_method: false,
                    origin: None,
                    declared_on: None,
                  },
                });
              }
            }
            ObjMemberType::Shorthand { id } => {
              let key = PropKey::String(self.store.intern_name_ref(&id.stx.name));
              let ty = self
                .lookup(&id.stx.name)
                .map(|b| b.ty)
                .unwrap_or(prim.unknown);
              shape.properties.push(types_ts_interned::Property {
                key,
                data: PropData {
                  ty,
                  optional: false,
                  readonly: true,
                  accessibility: None,
                  is_method: false,
                  origin: None,
                  declared_on: None,
                },
              });
            }
            ObjMemberType::Rest { .. } => {}
          }
        }
        let shape_id = self.store.intern_shape(shape);
        let obj = self.store.intern_object(ObjectType { shape: shape_id });
        self.store.intern_type(TypeKind::Object(obj))
      }
      AstExpr::TypeAssertion(assert) if assert.stx.const_assertion => {
        self.const_inference_type(&assert.stx.expression)
      }
      _ => {
        let range = loc_to_range(self.file, expr.loc);
        self
          .expr_map
          .get(&range)
          .and_then(|id| self.expr_types.get(id.0 as usize))
          .copied()
          .unwrap_or(prim.unknown)
      }
    }
  }

  fn const_assertion_type(&mut self, expr: &Node<AstExpr>) -> TypeId {
    let prim = self.store.primitive_ids();
    let ty = match expr.stx.as_ref() {
      AstExpr::LitNum(num) => self
        .store
        .intern_type(TypeKind::NumberLiteral(OrderedFloat::from(num.stx.value.0))),
      AstExpr::LitStr(str_lit) => {
        let name = self.store.intern_name_ref(&str_lit.stx.value);
        self.store.intern_type(TypeKind::StringLiteral(name))
      }
      AstExpr::LitBool(b) => self
        .store
        .intern_type(TypeKind::BooleanLiteral(b.stx.value)),
      AstExpr::LitNull(_) => prim.null,
      AstExpr::LitBigInt(value) => {
        let trimmed = value.stx.value.trim_end_matches('n');
        let parsed = trimmed
          .parse::<BigInt>()
          .unwrap_or_else(|_| BigInt::from(0u8));
        self.store.intern_type(TypeKind::BigIntLiteral(parsed))
      }
      AstExpr::LitArr(arr) => {
        let mut elems = Vec::new();
        for elem in arr.stx.elements.iter() {
          match elem {
            parse_js::ast::expr::lit::LitArrElem::Single(v) => {
              elems.push(types_ts_interned::TupleElem {
                ty: self.const_assertion_type(v),
                optional: false,
                rest: false,
                readonly: true,
              })
            }
            parse_js::ast::expr::lit::LitArrElem::Rest(v) => {
              elems.push(types_ts_interned::TupleElem {
                ty: self.const_assertion_type(v),
                optional: false,
                rest: true,
                readonly: true,
              })
            }
            parse_js::ast::expr::lit::LitArrElem::Empty => {
              elems.push(types_ts_interned::TupleElem {
                ty: prim.undefined,
                optional: true,
                rest: false,
                readonly: true,
              })
            }
          }
        }
        self.store.intern_type(TypeKind::Tuple(elems))
      }
      AstExpr::LitObj(obj) => {
        let mut shape = Shape::new();
        for member in obj.stx.members.iter() {
          match &member.stx.typ {
            ObjMemberType::Valued { key, val } => {
              let prop_key = match key {
                ClassOrObjKey::Direct(direct) => {
                  Some(PropKey::String(self.store.intern_name_ref(&direct.stx.key)))
                }
                ClassOrObjKey::Computed(expr) => {
                  let key_ty = self.check_expr(expr);
                  if let Some(literal) = self.constant_computed_key_name(expr) {
                    Some(PropKey::String(self.store.intern_name_ref(&literal)))
                  } else {
                    match self.store.type_kind(key_ty) {
                      TypeKind::StringLiteral(id) => Some(PropKey::String(id)),
                      TypeKind::NumberLiteral(num) => Some(PropKey::String(
                        self.store.intern_name(num.0.to_string()),
                      )),
                      _ => None,
                    }
                  }
                }
              };

              if let ClassOrObjVal::Prop(Some(value)) = val {
                let value_ty = self.const_assertion_type(value);
                if let Some(prop_key) = prop_key {
                  shape.properties.push(types_ts_interned::Property {
                    key: prop_key,
                    data: PropData {
                      ty: value_ty,
                      optional: false,
                      readonly: true,
                      accessibility: None,
                      is_method: false,
                      origin: None,
                      declared_on: None,
                    },
                  });
                }
              }
            }
            ObjMemberType::Shorthand { id } => {
              let key = PropKey::String(self.store.intern_name_ref(&id.stx.name));
              let ty = self
                .lookup(&id.stx.name)
                .map(|b| b.ty)
                .unwrap_or(prim.unknown);
              shape.properties.push(types_ts_interned::Property {
                key,
                data: PropData {
                  ty,
                  optional: false,
                  readonly: true,
                  accessibility: None,
                  is_method: false,
                  origin: None,
                  declared_on: None,
                },
              });
            }
            ObjMemberType::Rest { val } => {
              let _ = self.check_expr(val);
            }
          }
        }
        let shape_id = self.store.intern_shape(shape);
        let obj = self.store.intern_object(ObjectType { shape: shape_id });
        self.store.intern_type(TypeKind::Object(obj))
      }
      AstExpr::TypeAssertion(assert) => {
        if assert.stx.const_assertion {
          self.const_assertion_type(&assert.stx.expression)
        } else if let Some(annotation) = assert.stx.type_annotation.as_ref() {
          self.lowerer.lower_type_expr(annotation)
        } else {
          self.check_expr(&assert.stx.expression)
        }
      }
      _ => self.check_expr(expr),
    };
    self.record_expr_type(expr.loc, ty);
    ty
  }

  fn member_type(&mut self, obj: TypeId, prop: &str) -> TypeId {
    let receiver = self.store.canon(obj);
    self.member_type_with_receiver(obj, prop, receiver)
  }

  fn member_type_with_receiver(&mut self, obj: TypeId, prop: &str, receiver: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    let ty = self.member_type_opt(obj, prop).unwrap_or(prim.unknown);
    substitute_this_type(&self.store, ty, receiver)
  }

  fn member_type_opt(&mut self, obj: TypeId, prop: &str) -> Option<TypeId> {
    let prim = self.store.primitive_ids();
    let lookup_obj = if matches!(self.store.type_kind(self.store.canon(obj)), TypeKind::This) {
      self.current_this_ty
    } else {
      obj
    };
    let obj = self.expand_callable_type(lookup_obj);
    match self.store.type_kind(obj) {
      TypeKind::OmitConstructSignatures(inner) => self.member_type_opt(inner, prop),
      TypeKind::InheritConstructSignatures { .. } => None,
      // `expand_callable_type` above follows any resolvable references with a local
      // cycle guard and expands type parameters through their constraints. If we
      // still have a `Ref`, treat it as unknown to avoid infinitely recursing on
      // self-referential expansions (e.g. during in-progress type computation).
      TypeKind::Ref { .. } => Some(prim.unknown),
      TypeKind::Any => Some(prim.any),
      TypeKind::Unknown => Some(prim.unknown),
      TypeKind::Callable { .. } if prop == "call" => {
        let sigs = callable_signatures(self.store.as_ref(), obj);
        if sigs.is_empty() {
          Some(prim.unknown)
        } else {
          Some(self.build_call_method_type(sigs))
        }
      }
      // Callables have `Function.prototype` members (e.g. `apply`/`bind`) even if
      // we don't fully model `CallableFunction` / `NewableFunction` yet. Keep
      // the checker closer to `tsc` by allowing these members on callable types.
      //
      // We currently treat them as `any` until the full lib surface is plumbed
      // through in a principled way.
      TypeKind::Callable { .. } if matches!(prop, "apply" | "bind") => Some(prim.any),
      TypeKind::Callable { .. } => None,
      TypeKind::Object(obj_id) => {
        let shape = self.store.shape(self.store.object(obj_id).shape);
        for candidate in shape.properties.iter() {
          let matches = match &candidate.key {
            PropKey::String(name_id) => self.store.name(*name_id) == prop,
            PropKey::Number(num) => prop.parse::<i64>().ok() == Some(*num),
            _ => false,
          };
          if matches {
            let mut ty = candidate.data.ty;
            if candidate.data.optional {
              ty = self.store.union(vec![ty, prim.undefined]);
            }
            return Some(ty);
          }
        }
        if prop == "call" && !shape.call_signatures.is_empty() {
          return Some(self.build_call_method_type(shape.call_signatures.clone()));
        }
        if matches!(prop, "apply" | "bind") && !shape.call_signatures.is_empty() {
          return Some(prim.any);
        }

        let key = if let Some(idx) = parse_canonical_index_str(prop) {
          PropKey::Number(idx)
        } else {
          PropKey::String(self.store.intern_name_ref(prop))
        };
        let mut matches = Vec::new();
        for idxer in shape.indexers.iter() {
          if crate::type_queries::indexer_accepts_key(&key, idxer.key_type, &self.store) {
            matches.push(idxer.value_type);
          }
        }
        if matches.is_empty() {
          None
        } else {
          Some(self.store.union(matches))
        }
      }
      TypeKind::Union(members) => {
        let mut collected = Vec::new();
        for member in members {
          match self.member_type_opt(member, prop) {
            Some(ty) => collected.push(ty),
            None => return None,
          }
        }
        Some(self.store.union(collected))
      }
      TypeKind::Intersection(members) => {
        let mut collected = Vec::new();
        for member in members {
          if let Some(ty) = self.member_type_opt(member, prop) {
            collected.push(ty);
          }
        }
        if collected.is_empty() {
          None
        } else if collected.len() == 1 {
          Some(collected[0])
        } else {
          Some(self.store.intersection(collected))
        }
      }
      TypeKind::Array { ty, .. } => {
        if prop == "length" {
          Some(prim.number)
        } else if parse_canonical_index_str(prop).is_some() {
          // Treat canonical numeric string keys (`"0"`, `"1"`, ...) as element access.
          Some(ty)
        } else {
          // Keep this permissive: we don't fully model the Array prototype surface yet, and we
          // prefer returning `unknown` over issuing "property does not exist" diagnostics.
          Some(prim.unknown)
        }
      }
      TypeKind::Tuple(_) => {
        if prop == "length" {
          // For fixed-length tuples `length` is a numeric literal in TypeScript, but `number` is a
          // sufficient approximation for most internal uses (and for native-js codegen).
          Some(prim.number)
        } else if let Some(idx) = parse_canonical_index_str(prop) {
          // Reuse the indexed-access helper so rest/optional element rules match computed access.
          let key_ty = self
            .store
            .intern_type(TypeKind::NumberLiteral((idx as f64).into()));
          Some(self.member_type_for_index_key(obj, key_ty))
        } else {
          Some(prim.unknown)
        }
      }
      _ => Some(prim.unknown),
    }
  }

  fn check_member_access_for_type(
    &mut self,
    receiver_ty: TypeId,
    prop: &str,
    span: TextRange,
    receiver_kind: MemberAccessReceiver,
    hash_private_fallback: bool,
  ) {
    fn inner(
      checker: &mut Checker<'_>,
      receiver_ty: TypeId,
      obj: TypeId,
      prop: &str,
      span: TextRange,
      receiver_kind: MemberAccessReceiver,
      hash_private_fallback: bool,
      seen: &mut HashSet<TypeId>,
    ) -> bool {
      let obj = checker.expand_callable_type(obj);
      let obj = checker.store.canon(obj);
      if !seen.insert(obj) {
        return false;
      }
      match checker.store.type_kind(obj) {
        TypeKind::Object(obj_id) => {
          let shape = checker.store.shape(checker.store.object(obj_id).shape);
          for candidate in shape.properties.iter() {
            let matches = match &candidate.key {
              PropKey::String(name_id) => checker.store.name(*name_id) == prop,
              PropKey::Number(num) => prop.parse::<i64>().ok() == Some(*num),
              _ => false,
            };
            if matches {
              return checker.check_member_access(
                receiver_ty,
                receiver_kind,
                prop,
                &candidate.data,
                span,
                hash_private_fallback,
              );
            }
          }
          false
        }
        TypeKind::Union(members) => {
          for member in members {
            if inner(
              checker,
              receiver_ty,
              member,
              prop,
              span,
              receiver_kind,
              hash_private_fallback,
              seen,
            ) {
              return true;
            }
          }
          false
        }
        TypeKind::Intersection(members) => {
          for member in members {
            if checker.member_type_opt(member, prop).is_some()
              && inner(
                checker,
                receiver_ty,
                member,
                prop,
                span,
                receiver_kind,
                hash_private_fallback,
                seen,
              )
            {
              return true;
            }
          }
          false
        }
        _ => false,
      }
    }

    let mut seen = HashSet::new();
    let _ = inner(
      self,
      receiver_ty,
      receiver_ty,
      prop,
      span,
      receiver_kind,
      hash_private_fallback,
      &mut seen,
    );
  }

  fn is_subclass_of(&self, derived: DefId, base: DefId) -> bool {
    if derived == base {
      return true;
    }
    // Without the ref expander, we cannot reliably walk the class `extends` chain.
    if self.ref_expander.is_none() {
      return false;
    }
    let mut queue: VecDeque<DefId> = VecDeque::new();
    let mut seen: HashSet<DefId> = HashSet::new();
    queue.push_back(derived);
    while let Some(next) = queue.pop_front() {
      if !seen.insert(next) {
        continue;
      }
      if next == base {
        return true;
      }
      let ref_ty = self.store.intern_type(TypeKind::Ref {
        def: next,
        args: Vec::new(),
      });
      let expanded = self.expand_ref(ref_ty);
      if let TypeKind::Intersection(parts) = self.store.type_kind(expanded) {
        for part in parts {
          if let TypeKind::Ref { def, .. } = self.store.type_kind(part) {
            if def == base {
              return true;
            }
            queue.push_back(def);
          }
        }
      }
    }
    false
  }

  fn check_member_access(
    &mut self,
    receiver_ty: TypeId,
    receiver_kind: MemberAccessReceiver,
    prop: &str,
    prop_data: &PropData,
    span: TextRange,
    hash_private_fallback: bool,
  ) -> bool {
    let accessibility = prop_data.accessibility.or_else(|| {
      if hash_private_fallback && prop.starts_with('#') {
        Some(Accessibility::Private)
      } else {
        None
      }
    });

    let Some(accessibility) = accessibility else {
      return false;
    };

    let (is_private, is_protected) = match accessibility {
      Accessibility::Private => (true, false),
      Accessibility::Protected => (false, true),
      Accessibility::Public => (false, false),
    };

    if !is_private && !is_protected {
      return false;
    }

    let Some(current_class_type_def) = self.current_class_def else {
      if is_private {
        self.diagnostics.push(codes::PRIVATE_MEMBER_ACCESS.error(
          format!("Property '{prop}' is private and only accessible within class."),
          Span::new(self.file, span),
        ));
      } else {
        self
          .diagnostics
          .push(codes::PROTECTED_MEMBER_ACCESS.error(
            format!("Property '{prop}' is protected and only accessible within class and its subclasses."),
            Span::new(self.file, span),
          ));
      }
      return true;
    };

    let declaring = prop_data.declared_on;
    let declaring_is_value_def =
      declaring.is_some_and(|decl| self.value_defs.values().any(|value_def| *value_def == decl));
    let current_class_for_chain_check = if declaring_is_value_def {
      self
        .value_defs
        .get(&current_class_type_def)
        .copied()
        .unwrap_or(current_class_type_def)
    } else {
      current_class_type_def
    };

    let allowed_by_class = if is_private {
      declaring.is_none() || declaring == Some(current_class_for_chain_check)
    } else {
      declaring.is_none()
        || declaring == Some(current_class_for_chain_check)
        || declaring.is_some_and(|decl| self.is_subclass_of(current_class_for_chain_check, decl))
    };

    if !allowed_by_class {
      if is_private {
        self.diagnostics.push(codes::PRIVATE_MEMBER_ACCESS.error(
          format!("Property '{prop}' is private and only accessible within class."),
          Span::new(self.file, span),
        ));
      } else {
        self
          .diagnostics
          .push(codes::PROTECTED_MEMBER_ACCESS.error(
            format!("Property '{prop}' is protected and only accessible within class and its subclasses."),
            Span::new(self.file, span),
          ));
      }
      return true;
    }

    // TypeScript further restricts protected member access: even in a derived
    // class, a protected member declared in the base can only be accessed
    // through an expression whose type is derived from the *current* class.
    //
    // Example (tsc TS2446):
    //   class Base { protected x = 1; }
    //   class Derived extends Base {
    //     f(b: Base) { return b.x; } // error
    //   }
    //
    // `super.prop` is always allowed once the class-chain check passes.
    if is_protected
      && declaring.is_some()
      && matches!(receiver_kind, MemberAccessReceiver::Other)
    {
      fn uses_value_side_def(
        checker: &Checker<'_>,
        ty: TypeId,
        seen: &mut HashSet<TypeId>,
        depth: usize,
      ) -> bool {
        if depth > 64 {
          return false;
        }
        let ty = checker.store.canon(ty);
        if !seen.insert(ty) {
          return false;
        }
        match checker.store.type_kind(ty) {
          TypeKind::Ref { def, .. } => checker
            .value_defs
            .values()
            .any(|value_def| *value_def == def),
          TypeKind::Union(members) => members
            .iter()
            .copied()
            .all(|member| uses_value_side_def(checker, member, seen, depth + 1)),
          TypeKind::Intersection(members) => members
            .iter()
            .copied()
            .any(|member| uses_value_side_def(checker, member, seen, depth + 1)),
          _ => false,
        }
      }

      let receiver_targets_value_side =
        uses_value_side_def(self, receiver_ty, &mut HashSet::new(), 0);
      let current_receiver_def = if receiver_targets_value_side {
        self
          .value_defs
          .get(&current_class_type_def)
          .copied()
          .unwrap_or(current_class_type_def)
      } else {
        current_class_type_def
      };

      if self.receiver_is_derived_from_current_class(receiver_ty, current_receiver_def) {
        return false;
      }

      self
        .diagnostics
        .push(codes::PROTECTED_MEMBER_ACCESS_THROUGH_INSTANCE.error(
          format!(
            "Property '{prop}' is protected and only accessible through an instance of the current class."
          ),
          Span::new(self.file, span),
        ));
      return true;
    }

    false
  }

  fn type_has_prop(&self, ty: TypeId, prop: &str) -> bool {
    fn inner(
      checker: &Checker<'_>,
      ty: TypeId,
      prop: &str,
      cache: &mut HashMap<TypeId, bool>,
      stack: &mut HashSet<TypeId>,
    ) -> bool {
      let ty = checker.store.canon(ty);
      if let Some(cached) = cache.get(&ty) {
        return *cached;
      }
      if !stack.insert(ty) {
        // Cycle guard.
        return false;
      }

      let expanded = checker.expand_for_props(ty);
      let result = if expanded != ty {
        inner(checker, expanded, prop, cache, stack)
      } else {
        let expanded_ref = checker.expand_ref(ty);
        if expanded_ref != expanded {
          inner(checker, expanded_ref, prop, cache, stack)
        } else {
          match checker.store.type_kind(expanded_ref) {
            TypeKind::OmitConstructSignatures(inner_ty) => inner(checker, inner_ty, prop, cache, stack),
            TypeKind::InheritConstructSignatures { .. } => false,
            TypeKind::TypeParam(param) => checker
              .type_param_constraint(param)
              .is_some_and(|constraint| inner(checker, constraint, prop, cache, stack)),
            TypeKind::Object(obj_id) => {
              let shape = checker.store.shape(checker.store.object(obj_id).shape);
              if matches!(prop, "call" | "apply" | "bind") && !shape.call_signatures.is_empty() {
                true
              } else {
                for candidate in shape.properties.iter() {
                  match candidate.key {
                    PropKey::String(name_id) => {
                      if checker.store.name(name_id) == prop {
                        stack.remove(&ty);
                        cache.insert(ty, true);
                        return true;
                      }
                    }
                    PropKey::Number(num) => {
                      if prop.parse::<i64>().ok() == Some(num) {
                        stack.remove(&ty);
                        cache.insert(ty, true);
                        return true;
                      }
                    }
                    _ => {}
                  }
                }
                if shape.indexers.is_empty() {
                  false
                } else {
                  let key = if let Some(idx) = parse_canonical_index_str(prop) {
                    PropKey::Number(idx)
                  } else {
                    PropKey::String(checker.store.intern_name_ref(prop))
                  };
                  shape.indexers.iter().any(|idxer| {
                    crate::type_queries::indexer_accepts_key(
                      &key,
                      idxer.key_type,
                      checker.store.as_ref(),
                    )
                  })
                }
              }
            }
            // For union types, a property is only considered present if it exists on
            // all constituents (mirrors TS property-access rules).
            TypeKind::Union(members) => members
              .iter()
              .copied()
              .all(|member| inner(checker, member, prop, cache, stack)),
            // Intersection types accumulate properties from all members, so a
            // property exists if any member provides it.
            TypeKind::Intersection(members) => members
              .iter()
              .copied()
              .any(|member| inner(checker, member, prop, cache, stack)),
            TypeKind::Callable { .. } => matches!(prop, "call" | "apply" | "bind"),
            TypeKind::Ref { .. } => false,
            TypeKind::Mapped(_) => true,
            _ => false,
          }
        }
      };

      stack.remove(&ty);
      cache.insert(ty, result);
      result
    }

    inner(self, ty, prop, &mut HashMap::new(), &mut HashSet::new())
  }

  fn receiver_is_derived_from_current_class(&self, receiver_ty: TypeId, current: DefId) -> bool {
    fn inner(
      checker: &Checker<'_>,
      ty: TypeId,
      current: DefId,
      seen: &mut HashSet<TypeId>,
      depth: usize,
    ) -> bool {
      if depth > 64 {
        return false;
      }
      let ty = checker.store.canon(ty);
      if !seen.insert(ty) {
        return false;
      }
      match checker.store.type_kind(ty) {
        TypeKind::Any | TypeKind::Unknown | TypeKind::Never => true,
        TypeKind::This => true,
        TypeKind::TypeParam(param) => checker
          .type_param_constraint(param)
          .is_some_and(|constraint| inner(checker, constraint, current, seen, depth + 1)),
        TypeKind::Ref { def, .. } => checker.is_subclass_of(def, current),
        TypeKind::Union(members) => members
          .iter()
          .copied()
          .all(|member| inner(checker, member, current, seen, depth + 1)),
        TypeKind::Intersection(members) => members
          .iter()
          .copied()
          .any(|member| inner(checker, member, current, seen, depth + 1)),
        TypeKind::Null | TypeKind::Undefined => false,
        TypeKind::Infer { constraint, .. } => constraint
          .is_some_and(|constraint| inner(checker, constraint, current, seen, depth + 1)),
        _ => false,
      }
    }

    inner(self, receiver_ty, current, &mut HashSet::new(), 0)
  }

  fn member_type_for_index_key(&mut self, obj: TypeId, key_ty: TypeId) -> TypeId {
    let receiver = self.store.canon(obj);
    self.member_type_for_index_key_with_receiver(obj, key_ty, receiver)
  }

  fn member_type_for_index_key_with_receiver(
    &mut self,
    obj: TypeId,
    key_ty: TypeId,
    receiver: TypeId,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    let key_ty = self.store.canon(key_ty);
    let lookup_obj = if matches!(self.store.type_kind(self.store.canon(obj)), TypeKind::This) {
      self.current_this_ty
    } else {
      obj
    };
    let obj = self.expand_callable_type(lookup_obj);
    match self.store.type_kind(key_ty) {
      TypeKind::Union(members) => {
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_index_key_with_receiver(obj, member, receiver));
        }
        return substitute_this_type(&self.store, self.store.union(collected), receiver);
      }
      TypeKind::Intersection(members) => {
        // Keep this conservative: treat intersections of key types similarly to unions.
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_index_key_with_receiver(obj, member, receiver));
        }
        return substitute_this_type(&self.store, self.store.union(collected), receiver);
      }
      _ => {}
    }

    let ty = match self.store.type_kind(obj) {
      TypeKind::OmitConstructSignatures(inner) => {
        return self.member_type_for_index_key_with_receiver(inner, key_ty, receiver);
      }
      TypeKind::InheritConstructSignatures { .. } => prim.unknown,
      TypeKind::Union(members) => {
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_index_key_with_receiver(member, key_ty, receiver));
        }
        self.store.union(collected)
      }
      TypeKind::Intersection(members) => {
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_index_key_with_receiver(member, key_ty, receiver));
        }
        self.store.intersection(collected)
      }
      // Indexing into `undefined`/`null` should not poison the entire access with `unknown`.
      //
      // This matters when optional properties (e.g. `children?: [...]`) are read as `T | undefined`
      // and we later index into that union for JSX children contextual typing.
      TypeKind::Undefined => prim.undefined,
      TypeKind::Null => prim.null,
      TypeKind::Ref { .. } => prim.unknown,
      TypeKind::Object(obj_id) => {
        let shape = self.store.shape(self.store.object(obj_id).shape);
        let mut matches = Vec::new();
        for idx in shape.indexers.iter() {
          if self.indexer_key_matches(idx.key_type, key_ty) {
            matches.push(idx.value_type);
          }
        }
        if matches.is_empty() {
          prim.unknown
        } else if matches.len() == 1 {
          matches[0]
        } else {
          matches.sort_by(|a, b| self.store.type_cmp(*a, *b));
          matches.dedup();
          self.store.union(matches)
        }
      }
      TypeKind::Array { ty, .. } => {
        if self.relate.is_assignable(key_ty, prim.number) {
          ty
        } else {
          prim.unknown
        }
      }
      TypeKind::Tuple(elems) => match self.store.type_kind(key_ty) {
        TypeKind::NumberLiteral(num) => {
          let raw = num.0;
          if raw.fract() != 0.0 || raw < 0.0 {
            prim.unknown
          } else {
            let idx = raw as usize;
            if let Some(elem) = elems.get(idx) {
              let mut ty = if elem.rest {
                self.relate.spread_element_type(elem.ty)
              } else {
                elem.ty
              };
              if elem.optional && !self.relate.options.exact_optional_property_types {
                ty = self.store.union(vec![ty, prim.undefined]);
              }
              ty
            } else if let Some(rest) = elems.iter().find(|elem| elem.rest) {
              self.relate.spread_element_type(rest.ty)
            } else {
              prim.undefined
            }
          }
        }
        _ => {
          if !self.relate.is_assignable(key_ty, prim.number) {
            prim.unknown
          } else {
            let mut members = Vec::new();
            for elem in elems {
              let mut ty = if elem.rest {
                self.relate.spread_element_type(elem.ty)
              } else {
                elem.ty
              };
              if elem.optional && !self.relate.options.exact_optional_property_types {
                ty = self.store.union(vec![ty, prim.undefined]);
              }
              members.push(ty);
            }
            self.store.union(members)
          }
        }
      },
      _ => prim.unknown,
    };

    substitute_this_type(&self.store, ty, receiver)
  }

  fn indexer_key_matches(&self, indexer_key: TypeId, key_ty: TypeId) -> bool {
    let prim = self.store.primitive_ids();
    let key_ty = self.store.canon(key_ty);

    let dummy_name = self.store.intern_name_ref("<index>");
    let mut candidates = Vec::new();

    match self.store.type_kind(key_ty) {
      TypeKind::String | TypeKind::StringLiteral(_) => {
        candidates.push(PropKey::String(dummy_name));
      }
      TypeKind::Number | TypeKind::NumberLiteral(_) => {
        candidates.push(PropKey::Number(0));
      }
      TypeKind::Symbol | TypeKind::UniqueSymbol => {
        candidates.push(PropKey::Symbol(dummy_name));
      }
      TypeKind::Any => {
        candidates.push(PropKey::String(dummy_name));
        candidates.push(PropKey::Number(0));
        candidates.push(PropKey::Symbol(dummy_name));
      }
      _ => {
        if self.relate.is_assignable(key_ty, prim.string) {
          candidates.push(PropKey::String(dummy_name));
        }
        if self.relate.is_assignable(key_ty, prim.number) {
          candidates.push(PropKey::Number(0));
        }
        if self.relate.is_assignable(key_ty, prim.symbol) {
          candidates.push(PropKey::Symbol(dummy_name));
        }
      }
    }

    candidates
      .into_iter()
      .any(|key| crate::type_queries::indexer_accepts_key(&key, indexer_key, &self.store))
  }

  fn build_call_method_type(&self, sigs: Vec<SignatureId>) -> TypeId {
    let prim = self.store.primitive_ids();
    let mut overloads = Vec::new();
    for sig_id in sigs {
      let sig = self.store.signature(sig_id);
      let this_arg = sig.this_param.unwrap_or(prim.any);
      let mut params = Vec::with_capacity(sig.params.len() + 1);
      params.push(SigParam {
        name: None,
        ty: this_arg,
        optional: false,
        rest: false,
      });
      params.extend(sig.params.clone());
      let call_sig = Signature {
        params,
        ret: sig.ret,
        type_params: sig.type_params.clone(),
        this_param: None,
      };
      overloads.push(self.store.intern_signature(call_sig));
    }
    overloads.sort();
    overloads.dedup();
    self.store.intern_type(TypeKind::Callable { overloads })
  }

  fn array_literal_context_candidates(
    &self,
    expected: TypeId,
    arity: usize,
  ) -> Vec<ArrayLiteralContext> {
    let mut queue: VecDeque<TypeId> = VecDeque::from([expected]);
    let mut seen = HashSet::new();
    let mut tuples: Vec<Vec<types_ts_interned::TupleElem>> = Vec::new();
    let mut arrays: Vec<TypeId> = Vec::new();

    while let Some(ty) = queue.pop_front() {
      if !seen.insert(ty) {
        continue;
      }
      match self.store.type_kind(ty) {
        TypeKind::Tuple(elems) => tuples.push(elems),
        TypeKind::Array { ty, .. } => arrays.push(ty),
        TypeKind::Union(members) | TypeKind::Intersection(members) => {
          for member in members {
            queue.push_back(member);
          }
        }
        TypeKind::Ref { def, args } => {
          if let Some(expanded) = self
            .ref_expander
            .and_then(|expander| expander.expand_ref(self.store.as_ref(), def, &args))
          {
            queue.push_back(expanded);
          }
        }
        _ => {}
      }
    }

    tuples.sort_by_key(|tuple| {
      let len = tuple.len();
      let diff = len.abs_diff(arity) as u32;
      (diff, len)
    });

    arrays.sort_by(|a, b| self.store.type_cmp(*a, *b));
    arrays.dedup();

    let mut out = Vec::new();
    out.extend(tuples.into_iter().map(ArrayLiteralContext::Tuple));
    out.extend(arrays.into_iter().map(ArrayLiteralContext::Array));
    out
  }

  fn expected_contains_primitive(&self, expected: TypeId, primitive: TypeId) -> bool {
    if expected == primitive {
      return true;
    }
    let mut queue: VecDeque<TypeId> = VecDeque::from([expected]);
    let mut seen = HashSet::new();
    while let Some(ty) = queue.pop_front() {
      if !seen.insert(ty) {
        continue;
      }
      if ty == primitive {
        return true;
      }
      match self.store.type_kind(ty) {
        TypeKind::Any => return true,
        TypeKind::Union(members) | TypeKind::Intersection(members) => {
          for member in members {
            queue.push_back(member);
          }
        }
        TypeKind::Ref { def, args } => {
          if let Some(expanded) = self
            .ref_expander
            .and_then(|expander| expander.expand_ref(self.store.as_ref(), def, &args))
          {
            queue.push_back(expanded);
          }
        }
        _ => {}
      }
    }
    false
  }

  fn contextual_widen_container(&self, inferred: TypeId, expected: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    let should_widen = self.expected_contains_primitive(expected, prim.number)
      || self.expected_contains_primitive(expected, prim.string)
      || self.expected_contains_primitive(expected, prim.boolean)
      || self.expected_contains_primitive(expected, prim.bigint);
    if should_widen {
      self.widen_object_prop(inferred)
    } else {
      inferred
    }
  }

  fn spread_element_type(&self, ty: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    let ty = self.expand_ref(ty);
    match self.store.type_kind(ty) {
      TypeKind::Any => prim.any,
      TypeKind::Unknown => prim.unknown,
      TypeKind::Union(members) => {
        let elems: Vec<_> = members
          .into_iter()
          .map(|m| self.spread_element_type(m))
          .collect();
        self.store.union(elems)
      }
      TypeKind::Intersection(members) => {
        let elems: Vec<_> = members
          .into_iter()
          .map(|m| self.spread_element_type(m))
          .collect();
        self.store.intersection(elems)
      }
      TypeKind::Array { ty, .. } => ty,
      TypeKind::Tuple(elems) => {
        let mut members = Vec::new();
        for elem in elems {
          let mut ty = if elem.rest {
            self.spread_element_type(elem.ty)
          } else {
            elem.ty
          };
          if elem.optional && !self.relate.options.exact_optional_property_types {
            ty = self.store.union(vec![ty, prim.undefined]);
          }
          members.push(ty);
        }
        if members.is_empty() {
          prim.unknown
        } else {
          self.store.union(members)
        }
      }
      TypeKind::Ref { .. } => prim.unknown,
      _ => prim.unknown,
    }
  }

  fn array_literal_type(&mut self, arr: &Node<parse_js::ast::expr::lit::LitArrExpr>) -> TypeId {
    let prim = self.store.primitive_ids();
    let mut elems = Vec::new();
    for elem in arr.stx.elements.iter() {
      match elem {
        parse_js::ast::expr::lit::LitArrElem::Single(v) => elems.push(self.check_expr(v)),
        parse_js::ast::expr::lit::LitArrElem::Rest(v) => {
          let spread = self.check_expr(v);
          elems.push(self.spread_element_type(spread));
        }
        parse_js::ast::expr::lit::LitArrElem::Empty => {}
      }
    }
    let elem_ty = if elems.is_empty() {
      prim.unknown
    } else {
      self.store.union(elems)
    };
    let elem_ty = if self.widen_object_literals {
      self.widen_object_prop(elem_ty)
    } else {
      elem_ty
    };
    self.store.intern_type(TypeKind::Array {
      ty: elem_ty,
      readonly: false,
    })
  }

  fn array_literal_type_with_expected(
    &mut self,
    arr: &Node<parse_js::ast::expr::lit::LitArrExpr>,
    expected: TypeId,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    if arr
      .stx
      .elements
      .iter()
      .any(|e| !matches!(e, parse_js::ast::expr::lit::LitArrElem::Single(_)))
    {
      return self.array_literal_type(arr);
    }

    let elems: Vec<_> = arr
      .stx
      .elements
      .iter()
      .filter_map(|e| match e {
        parse_js::ast::expr::lit::LitArrElem::Single(v) => Some(v),
        _ => None,
      })
      .collect();
    let arity = elems.len();

    let contexts = self.array_literal_context_candidates(expected, arity);
    if contexts.is_empty() {
      return self.array_literal_type(arr);
    }

    let selected = contexts
      .iter()
      .enumerate()
      .find_map(|(context_idx, context)| {
        let ok = elems.iter().enumerate().all(|(idx, expr)| {
          let expected_elem = match context {
            ArrayLiteralContext::Tuple(expected_elems) => expected_elems
              .get(idx)
              .map(|e| e.ty)
              .unwrap_or(prim.unknown),
            ArrayLiteralContext::Array(expected_elem) => *expected_elem,
          };
          !self.has_contextual_excess_properties(expr, expected_elem)
        });
        ok.then_some(context_idx)
      })
      .unwrap_or(0);

    let context = contexts
      .into_iter()
      .nth(selected)
      .unwrap_or_else(|| unreachable!("contexts non-empty; selected index {selected} must exist"));

    match context {
      ArrayLiteralContext::Tuple(expected_elems) => {
        let mut out = Vec::new();
        for (idx, expr) in elems.into_iter().enumerate() {
          let expected_elem = expected_elems
            .get(idx)
            .map(|e| e.ty)
            .unwrap_or(prim.unknown);
          let expr_ty = if expected_elem != prim.unknown {
            self.check_expr_with_expected(expr, expected_elem)
          } else {
            self.check_expr(expr)
          };
          if expected_elem != prim.unknown {
            if let AstExpr::LitObj(obj) = expr.stx.as_ref() {
              if let Some(range) = self.excess_property_range(obj, expected_elem) {
                self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                  "excess property",
                  Span {
                    file: self.file,
                    range,
                  },
                ));
              }
            }
          }
          let stored = if expected_elem != prim.unknown {
            self.contextual_widen_container(expr_ty, expected_elem)
          } else {
            self.widen_object_prop(expr_ty)
          };
          out.push(types_ts_interned::TupleElem {
            ty: stored,
            optional: false,
            rest: false,
            readonly: false,
          });
        }
        self.store.intern_type(TypeKind::Tuple(out))
      }
      ArrayLiteralContext::Array(expected_elem) => {
        let mut out = Vec::new();
        for expr in elems.into_iter() {
          let expr_ty = self.check_expr_with_expected(expr, expected_elem);
          if expected_elem != prim.unknown {
            if let AstExpr::LitObj(obj) = expr.stx.as_ref() {
              if let Some(range) = self.excess_property_range(obj, expected_elem) {
                self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                  "excess property",
                  Span {
                    file: self.file,
                    range,
                  },
                ));
              }
            }
          }
          let stored = self.contextual_widen_container(expr_ty, expected_elem);
          out.push(stored);
        }
        let elem_ty = if out.is_empty() {
          prim.unknown
        } else {
          self.store.union(out)
        };
        self.store.intern_type(TypeKind::Array {
          ty: elem_ty,
          readonly: false,
        })
      }
    }
  }

  fn object_literal_type(&mut self, obj: &Node<parse_js::ast::expr::lit::LitObjExpr>) -> TypeId {
    if obj.stx.members.is_empty() {
      // TypeScript infers `{}` for empty object literals, which is semantically
      // the top type for non-nullish values (and is distinct from the `object`
      // keyword).
      return self.store.intern_type(TypeKind::EmptyObject);
    }

    fn f64_to_i64(num: f64) -> Option<i64> {
      if !num.is_finite() {
        return None;
      }
      if num.fract() != 0.0 {
        return None;
      }
      let as_i64 = num as i64;
      (as_i64 as f64 == num).then_some(as_i64)
    }

    fn strip_computed_key_wrappers<'a>(expr: &'a Node<AstExpr>) -> &'a Node<AstExpr> {
      match expr.stx.as_ref() {
        AstExpr::TypeAssertion(assert) => strip_computed_key_wrappers(&assert.stx.expression),
        AstExpr::NonNullAssertion(assert) => strip_computed_key_wrappers(&assert.stx.expression),
        AstExpr::SatisfiesExpr(expr) => strip_computed_key_wrappers(&expr.stx.expression),
        _ => expr,
      }
    }

    let computed_key_as_prop_key = |checker: &mut Checker<'_>,
                                     expr: &Node<AstExpr>,
                                     key_ty: TypeId|
     -> Option<(PropKey, String)> {
      match expr.stx.as_ref() {
        AstExpr::LitStr(s) => {
          let name = s.stx.value.clone();
          Some((PropKey::String(checker.store.intern_name_ref(&name)), name))
        }
        AstExpr::LitTemplate(tpl) => {
          if tpl
            .stx
            .parts
            .iter()
            .all(|p| matches!(p, parse_js::ast::expr::lit::LitTemplatePart::String(_)))
          {
            let mut combined = String::new();
            for part in tpl.stx.parts.iter() {
              if let parse_js::ast::expr::lit::LitTemplatePart::String(s) = part {
                combined.push_str(s);
              }
            }
            Some((PropKey::String(checker.store.intern_name_ref(&combined)), combined))
          } else {
            None
          }
        }
        AstExpr::LitNum(n) => {
          let value = n.stx.value.0;
          let int_key = f64_to_i64(value)?;
          Some((PropKey::Number(int_key), int_key.to_string()))
        }
        AstExpr::LitBigInt(v) => {
          let name = v.stx.value.trim_end_matches('n').to_string();
          Some((PropKey::String(checker.store.intern_name_ref(&name)), name))
        }
        AstExpr::Member(member)
          if !member.stx.optional_chaining
            && matches!(member.stx.left.stx.as_ref(), AstExpr::Id(id) if id.stx.name == "Symbol") =>
        {
          let name = member.stx.right.clone();
          Some((PropKey::Symbol(checker.store.intern_name_ref(&name)), name))
        }
        AstExpr::ComputedMember(member)
          if !member.stx.optional_chaining
            && matches!(member.stx.object.stx.as_ref(), AstExpr::Id(id) if id.stx.name == "Symbol") =>
        {
          match member.stx.member.stx.as_ref() {
            AstExpr::LitStr(lit) => {
              let name = lit.stx.value.clone();
              Some((PropKey::Symbol(checker.store.intern_name_ref(&name)), name))
            }
            _ => None,
          }
        }
        _ => match checker.store.type_kind(key_ty) {
          TypeKind::StringLiteral(id) => {
            Some((PropKey::String(id), checker.store.name(id).to_string()))
          }
          TypeKind::NumberLiteral(num) => {
            let int_key = f64_to_i64(num.0)?;
            Some((PropKey::Number(int_key), int_key.to_string()))
          }
          _ => None,
        },
      }
    };

    let prim = self.store.primitive_ids();
    let mut shape = Shape::new();
    for member in obj.stx.members.iter() {
      match &member.stx.typ {
        ObjMemberType::Valued { key, val } => {
          let prop_key = match key {
            ClassOrObjKey::Direct(direct) => {
              Some(PropKey::String(self.store.intern_name_ref(&direct.stx.key)))
            }
            ClassOrObjKey::Computed(expr) => {
              let key_ty = self.check_expr(expr);
              computed_key_as_prop_key(self, strip_computed_key_wrappers(expr), key_ty)
                .map(|(key, _)| key)
            }
          };

          match val {
            ClassOrObjVal::Prop(Some(expr)) => {
              let ty = self.check_expr(expr);
              let ty = if self.widen_object_literals {
                self.widen_object_prop(ty)
              } else {
                ty
              };
              if let Some(prop_key) = prop_key {
                shape.properties.push(types_ts_interned::Property {
                  key: prop_key,
                  data: PropData {
                    ty,
                    optional: false,
                    readonly: false,
                    accessibility: None,
                    is_method: false,
                    origin: None,
                    declared_on: None,
                  },
                });
              }
            }
            ClassOrObjVal::Method(method) => {
              let ty = self.function_type(&method.stx.func);
              // Object literal methods are lowered into HIR as `ExprKind::FunctionExpr`
              // spans (keyed by `method.loc`), so ensure we record an expression type
              // for `type_at` queries.
              self.record_expr_type(method.loc, ty);
              if let Some(prop_key) = prop_key {
                shape.properties.push(types_ts_interned::Property {
                  key: prop_key,
                  data: PropData {
                    ty,
                    optional: false,
                    readonly: false,
                    accessibility: None,
                    is_method: true,
                    origin: None,
                    declared_on: None,
                  },
                });
              }
            }
            ClassOrObjVal::Getter(getter) => {
              let ty = self.function_type(&getter.stx.func);
              self.record_expr_type(getter.loc, ty);
            }
            ClassOrObjVal::Setter(setter) => {
              let ty = self.function_type(&setter.stx.func);
              self.record_expr_type(setter.loc, ty);
            }
            _ => {}
          }
        }
        ObjMemberType::Shorthand { id } => {
          let name = id.stx.name.clone();
          let key = PropKey::String(self.store.intern_name_ref(&name));
          let value_ty = match self.lookup(&name) {
            Some(binding) => binding.ty,
            None => {
              let mut range = loc_to_range(self.file, id.loc);
              if range.start == range.end {
                let len = name.len() as u32;
                range.start = range.start.saturating_sub(len);
                range.end = range.start.saturating_add(len);
              }
              self.diagnostics.push(codes::UNKNOWN_IDENTIFIER.error(
                format!("unknown identifier `{}`", name),
                Span {
                  file: self.file,
                  range,
                },
              ));
              prim.any
            }
          };
          self.record_expr_type(id.loc, value_ty);
          let ty = if self.widen_object_literals {
            self.widen_object_prop(value_ty)
          } else {
            value_ty
          };
          shape.properties.push(types_ts_interned::Property {
            key,
            data: PropData {
              ty,
              optional: false,
              readonly: false,
              accessibility: None,
              is_method: false,
              origin: None,
              declared_on: None,
            },
          });
        }
        ObjMemberType::Rest { val } => {
          let _ = self.check_expr(val);
        }
      }
    }
    let shape_id = self.store.intern_shape(shape);
    let obj = self.store.intern_object(ObjectType { shape: shape_id });
    self.store.intern_type(TypeKind::Object(obj))
  }

  fn object_literal_type_with_expected(
    &mut self,
    obj: &Node<parse_js::ast::expr::lit::LitObjExpr>,
    expected: TypeId,
  ) -> TypeId {
    let prim = self.store.primitive_ids();

    fn f64_to_i64(num: f64) -> Option<i64> {
      if !num.is_finite() {
        return None;
      }
      if num.fract() != 0.0 {
        return None;
      }
      let as_i64 = num as i64;
      (as_i64 as f64 == num).then_some(as_i64)
    }

    fn strip_computed_key_wrappers<'a>(expr: &'a Node<AstExpr>) -> &'a Node<AstExpr> {
      match expr.stx.as_ref() {
        AstExpr::TypeAssertion(assert) => strip_computed_key_wrappers(&assert.stx.expression),
        AstExpr::NonNullAssertion(assert) => strip_computed_key_wrappers(&assert.stx.expression),
        AstExpr::SatisfiesExpr(expr) => strip_computed_key_wrappers(&expr.stx.expression),
        _ => expr,
      }
    }

    let computed_key_as_prop_key = |checker: &mut Checker<'_>,
                                     expr: &Node<AstExpr>,
                                     key_ty: TypeId|
     -> Option<(PropKey, String)> {
      match expr.stx.as_ref() {
        AstExpr::LitStr(s) => {
          let name = s.stx.value.clone();
          Some((PropKey::String(checker.store.intern_name_ref(&name)), name))
        }
        AstExpr::LitTemplate(tpl) => {
          if tpl
            .stx
            .parts
            .iter()
            .all(|p| matches!(p, parse_js::ast::expr::lit::LitTemplatePart::String(_)))
          {
            let mut combined = String::new();
            for part in tpl.stx.parts.iter() {
              if let parse_js::ast::expr::lit::LitTemplatePart::String(s) = part {
                combined.push_str(s);
              }
            }
            Some((PropKey::String(checker.store.intern_name_ref(&combined)), combined))
          } else {
            None
          }
        }
        AstExpr::LitNum(n) => {
          let value = n.stx.value.0;
          let int_key = f64_to_i64(value)?;
          Some((PropKey::Number(int_key), int_key.to_string()))
        }
        AstExpr::LitBigInt(v) => {
          let name = v.stx.value.trim_end_matches('n').to_string();
          Some((PropKey::String(checker.store.intern_name_ref(&name)), name))
        }
        AstExpr::Member(member)
          if !member.stx.optional_chaining
            && matches!(member.stx.left.stx.as_ref(), AstExpr::Id(id) if id.stx.name == "Symbol") =>
        {
          let name = member.stx.right.clone();
          Some((PropKey::Symbol(checker.store.intern_name_ref(&name)), name))
        }
        AstExpr::ComputedMember(member)
          if !member.stx.optional_chaining
            && matches!(member.stx.object.stx.as_ref(), AstExpr::Id(id) if id.stx.name == "Symbol") =>
        {
          match member.stx.member.stx.as_ref() {
            AstExpr::LitStr(lit) => {
              let name = lit.stx.value.clone();
              Some((PropKey::Symbol(checker.store.intern_name_ref(&name)), name))
            }
            _ => None,
          }
        }
        _ => match checker.store.type_kind(key_ty) {
          TypeKind::StringLiteral(id) => {
            Some((PropKey::String(id), checker.store.name(id).to_string()))
          }
          TypeKind::NumberLiteral(num) => {
            let int_key = f64_to_i64(num.0)?;
            Some((PropKey::Number(int_key), int_key.to_string()))
          }
          _ => None,
        },
      }
    };

    let mut shape = Shape::new();
    for member in obj.stx.members.iter() {
      match &member.stx.typ {
        ObjMemberType::Valued { key, val } => {
          let (prop_key, expected_name) = match key {
            ClassOrObjKey::Direct(direct) => {
              let name = direct.stx.key.clone();
              (
                Some(PropKey::String(self.store.intern_name_ref(&name))),
                Some(name),
              )
            }
            ClassOrObjKey::Computed(expr) => {
              let key_ty = self.check_expr(expr);
              if let Some((prop_key, name)) = computed_key_as_prop_key(
                self,
                strip_computed_key_wrappers(expr),
                key_ty,
              ) {
                (Some(prop_key), Some(name))
              } else {
                (None, None)
              }
            }
          };

          let expected_prop = expected_name
            .as_deref()
            .map(|name| self.member_type(expected, name))
            .unwrap_or(prim.unknown);

          match val {
            ClassOrObjVal::Prop(Some(expr)) => {
              let expr_ty = if expected_prop != prim.unknown {
                self.check_expr_with_expected(expr, expected_prop)
              } else {
                self.check_expr(expr)
              };
              // Nested object literals are also "fresh" and participate in excess
              // property checks when contextually typed by an expected property
              // type.
              //
              // Without this, `let x: { nested: { foo: number } } = { nested: { foo: 1, bar: 2 } }`
              // would be accepted because `{ foo: 1, bar: 2 }` is structurally
              // assignable to `{ foo: number }` once it is no longer treated as a
              // fresh literal.
              if expected_prop != prim.unknown {
                if let AstExpr::LitObj(nested_obj) = expr.stx.as_ref() {
                  if let Some(range) = self.excess_property_range(nested_obj, expected_prop) {
                    self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                      "excess property",
                      Span {
                        file: self.file,
                        range,
                      },
                    ));
                  }
                }
              }
              let ty = if expected_prop != prim.unknown {
                self.contextual_widen_container(expr_ty, expected_prop)
              } else if self.widen_object_literals {
                self.widen_object_prop(expr_ty)
              } else {
                expr_ty
              };
              if let Some(prop_key) = prop_key {
                shape.properties.push(types_ts_interned::Property {
                  key: prop_key,
                  data: PropData {
                    ty,
                    optional: false,
                    readonly: false,
                    accessibility: None,
                    is_method: false,
                    origin: None,
                    declared_on: None,
                  },
                });
              }
            }
            ClassOrObjVal::Method(method) => {
              let mut ty = self.function_type(&method.stx.func);
              if expected_prop != prim.unknown {
                let expected_callable = self
                  .contextual_callable_type(expected_prop)
                  .unwrap_or(expected_prop);
                if let Some(refined) =
                  self.refine_function_expr_with_expected(&method.stx.func, expected_callable)
                {
                  ty = refined;
                }
              }
              self.record_expr_type(method.loc, ty);
              if let Some(prop_key) = prop_key {
                shape.properties.push(types_ts_interned::Property {
                  key: prop_key,
                  data: PropData {
                    ty,
                    optional: false,
                    readonly: false,
                    accessibility: None,
                    is_method: true,
                    origin: None,
                    declared_on: None,
                  },
                });
              }
            }
            ClassOrObjVal::Getter(getter) => {
              let ty = self.function_type(&getter.stx.func);
              self.record_expr_type(getter.loc, ty);
            }
            ClassOrObjVal::Setter(setter) => {
              let ty = self.function_type(&setter.stx.func);
              self.record_expr_type(setter.loc, ty);
            }
            _ => {}
          }
        }
        ObjMemberType::Shorthand { id } => {
          let name = id.stx.name.clone();
          let key = PropKey::String(self.store.intern_name_ref(&name));
          let value_ty = match self.lookup(&name) {
            Some(binding) => binding.ty,
            None => {
              let mut range = loc_to_range(self.file, id.loc);
              if range.start == range.end {
                let len = name.len() as u32;
                range.start = range.start.saturating_sub(len);
                range.end = range.start.saturating_add(len);
              }
              self.diagnostics.push(codes::UNKNOWN_IDENTIFIER.error(
                format!("unknown identifier `{}`", name),
                Span {
                  file: self.file,
                  range,
                },
              ));
              prim.any
            }
          };
          self.record_expr_type(id.loc, value_ty);
          let expected_prop = self.member_type(expected, &name);
          let ty = if expected_prop != prim.unknown {
            self.contextual_widen_container(value_ty, expected_prop)
          } else if self.widen_object_literals {
            self.widen_object_prop(value_ty)
          } else {
            value_ty
          };
          shape.properties.push(types_ts_interned::Property {
            key,
            data: PropData {
              ty,
              optional: false,
              readonly: false,
              accessibility: None,
              is_method: false,
              origin: None,
              declared_on: None,
            },
          });
        }
        ObjMemberType::Rest { val } => {
          let _ = self.check_expr(val);
        }
      }
    }
    let shape_id = self.store.intern_shape(shape);
    let obj = self.store.intern_object(ObjectType { shape: shape_id });
    self.store.intern_type(TypeKind::Object(obj))
  }

  fn widen_object_prop(&self, ty: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    match self.store.type_kind(ty) {
      TypeKind::NumberLiteral(_) => prim.number,
      TypeKind::StringLiteral(_) => prim.string,
      TypeKind::BooleanLiteral(_) => prim.boolean,
      TypeKind::BigIntLiteral(_) => prim.bigint,
      TypeKind::Union(members) => {
        let mapped: Vec<_> = members
          .into_iter()
          .map(|m| self.widen_object_prop(m))
          .collect();
        self.store.union(mapped)
      }
      TypeKind::Intersection(members) => {
        let mapped: Vec<_> = members
          .into_iter()
          .map(|m| self.widen_object_prop(m))
          .collect();
        self.store.intersection(mapped)
      }
      _ => ty,
    }
  }

  fn resolve_ident(&mut self, name: &str, expr: &Node<AstExpr>) -> TypeId {
    if let Some(binding) = self.lookup(name) {
      return binding.ty;
    }
    let mut range = loc_to_range(self.file, expr.loc);
    if range.start == range.end {
      let len = name.len() as u32;
      range.start = range.start.saturating_sub(len);
      range.end = range.start.saturating_add(len);
    }
    if std::env::var_os("DEBUG_RESOLVE_IDENT").is_some() {
      let mut scopes: Vec<(usize, usize, bool)> = self
        .scopes
        .iter()
        .enumerate()
        .map(|(idx, scope)| (idx, scope.bindings.len(), scope.bindings.contains_key(name)))
        .collect();
      scopes.reverse();
      let mut keys: Vec<String> = self
        .scopes
        .iter()
        .flat_map(|scope| scope.bindings.keys().cloned())
        .collect();
      keys.sort();
      keys.dedup();
      let preview: Vec<&str> = keys.iter().take(32).map(|s| s.as_str()).collect();
      eprintln!(
        "DEBUG_RESOLVE_IDENT: file={:?} body_kind={:?} name={:?} range={:?} scopes_rev={:?} keys={} preview={:?}",
        self.file,
        self.body_kind,
        name,
        range,
        scopes,
        keys.len(),
        preview
      );
    }
    self.diagnostics.push(codes::UNKNOWN_IDENTIFIER.error(
      format!("unknown identifier `{}`", name),
      Span {
        file: self.file,
        range,
      },
    ));
    // Match TypeScript's "error type" behaviour: unknown identifiers in value
    // positions behave like `any` so we don't cascade into follow-on diagnostics
    // like TS2322 assignability errors.
    self.store.primitive_ids().any
  }

  fn check_unary(&mut self, op: OperatorName, arg: &Node<AstExpr>) -> TypeId {
    match op {
      OperatorName::LogicalNot => {
        let _ = self.check_expr(arg);
        self.store.primitive_ids().boolean
      }
      OperatorName::Delete => {
        let _ = self.check_expr(arg);
        self.store.primitive_ids().boolean
      }
      OperatorName::UnaryPlus | OperatorName::UnaryNegation | OperatorName::BitwiseNot => {
        let _ = self.check_expr(arg);
        self.store.primitive_ids().number
      }
      OperatorName::PrefixIncrement | OperatorName::PrefixDecrement => {
        let prim = self.store.primitive_ids();
        let operand_ty = self.check_expr(arg);
        if self.is_bigint_like(self.base_type(operand_ty)) {
          prim.bigint
        } else {
          prim.number
        }
      }
      OperatorName::Await => {
        let inner = self.check_expr(arg);
        awaited_type(self.store.as_ref(), inner, self.ref_expander)
      }
      OperatorName::Typeof => {
        let _ = self.check_expr(arg);
        self.store.primitive_ids().string
      }
      OperatorName::Void => {
        let _ = self.check_expr(arg);
        self.store.primitive_ids().undefined
      }
      _ => {
        let _ = self.check_expr(arg);
        self.store.primitive_ids().unknown
      }
    }
  }

  fn check_binary(
    &mut self,
    op: OperatorName,
    left: &Node<AstExpr>,
    right: &Node<AstExpr>,
  ) -> TypeId {
    if op.is_assignment() {
      return self.check_assignment(op, left, right);
    }
    let lty = self.check_expr(left);
    let rty = self.check_expr(right);
    match op {
      OperatorName::Addition => {
        let left_kind = self.store.type_kind(lty);
        let right_kind = self.store.type_kind(rty);
        if matches!(left_kind, TypeKind::String | TypeKind::StringLiteral(_))
          || matches!(right_kind, TypeKind::String | TypeKind::StringLiteral(_))
        {
          self.store.primitive_ids().string
        } else if matches!(left_kind, TypeKind::Number | TypeKind::NumberLiteral(_))
          && matches!(right_kind, TypeKind::Number | TypeKind::NumberLiteral(_))
        {
          self.store.primitive_ids().number
        } else {
          self.store.union(vec![lty, rty])
        }
      }
      OperatorName::Subtraction
      | OperatorName::Multiplication
      | OperatorName::Division
      | OperatorName::Exponentiation
      | OperatorName::Remainder => self.store.primitive_ids().number,
      OperatorName::BitwiseAnd
      | OperatorName::BitwiseLeftShift
      | OperatorName::BitwiseOr
      | OperatorName::BitwiseRightShift
      | OperatorName::BitwiseUnsignedRightShift
      | OperatorName::BitwiseXor => self.store.primitive_ids().number,
      OperatorName::LogicalAnd | OperatorName::LogicalOr | OperatorName::NullishCoalescing => {
        self.store.union(vec![lty, rty])
      }
      OperatorName::Equality
      | OperatorName::Inequality
      | OperatorName::StrictEquality
      | OperatorName::StrictInequality
      | OperatorName::LessThan
      | OperatorName::LessThanOrEqual
      | OperatorName::GreaterThan
      | OperatorName::GreaterThanOrEqual => self.store.primitive_ids().boolean,
      OperatorName::In | OperatorName::Instanceof => self.store.primitive_ids().boolean,
      OperatorName::Comma => rty,
      _ => self.store.union(vec![lty, rty]),
    }
  }

  fn check_assignment(
    &mut self,
    op: OperatorName,
    left: &Node<AstExpr>,
    right: &Node<AstExpr>,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    match left.stx.as_ref() {
      AstExpr::Id(id) => {
        if let Some((scope_idx, binding)) = self.lookup_with_scope(&id.stx.name) {
          let mut value_ty = if matches!(op, OperatorName::Assignment) {
            self.check_expr_with_expected(right, binding.ty)
          } else {
            self.check_expr(right)
          };
          if matches!(op, OperatorName::Assignment) {
            if let AstExpr::LitObj(obj) = right.stx.as_ref() {
              if let Some(range) = self.excess_property_range(obj, binding.ty) {
                self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                  "excess property",
                  Span {
                    file: self.file,
                    range,
                  },
                ));
              }
            }
          }
          if matches!(op, OperatorName::Assignment) && !self.relate.is_assignable(value_ty, binding.ty)
          {
            if let Some(instantiated) =
              self.contextually_instantiate_generic_callable(value_ty, binding.ty)
            {
              value_ty = instantiated;
            }
          }
          if !self.relate.is_assignable(value_ty, binding.ty) {
            self.diagnostics.push(codes::TYPE_MISMATCH.error(
              "assignment type mismatch",
              Span {
                file: self.file,
                range: loc_to_range(self.file, right.loc),
              },
            ));
          }
          // Keep the binding type stable: the base (AST) checker should not narrow mutable bindings
          // to RHS literal types (e.g. `x = 10` => `x: 10`), since flow-sensitive updates are
          // handled by `FlowBodyChecker`.
          let next_binding_ty = if binding.ty == prim.unknown {
            self.base_type(value_ty)
          } else {
            binding.ty
          };
          self.insert_binding_in_scope(
            scope_idx,
            id.stx.name.clone(),
            next_binding_ty,
            binding.type_params,
          );
          return self.base_type(value_ty);
        } else {
          let value_ty = self.check_expr(right);
          self.insert_binding(id.stx.name.clone(), value_ty, Vec::new());
          return value_ty;
        }
      }
      AstExpr::IdPat(id) => {
        if let Some((scope_idx, binding)) = self.lookup_with_scope(&id.stx.name) {
          let mut value_ty = if matches!(op, OperatorName::Assignment) {
            self.check_expr_with_expected(right, binding.ty)
          } else {
            self.check_expr(right)
          };
          if matches!(op, OperatorName::Assignment) {
            if let AstExpr::LitObj(obj) = right.stx.as_ref() {
              if let Some(range) = self.excess_property_range(obj, binding.ty) {
                self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                  "excess property",
                  Span {
                    file: self.file,
                    range,
                  },
                ));
              }
            }
          }
          if matches!(op, OperatorName::Assignment) && !self.relate.is_assignable(value_ty, binding.ty)
          {
            if let Some(instantiated) =
              self.contextually_instantiate_generic_callable(value_ty, binding.ty)
            {
              value_ty = instantiated;
            }
          }
          if !self.relate.is_assignable(value_ty, binding.ty) {
            self.diagnostics.push(codes::TYPE_MISMATCH.error(
              "assignment type mismatch",
              Span {
                file: self.file,
                range: loc_to_range(self.file, right.loc),
              },
            ));
          }
          let next_binding_ty = if binding.ty == prim.unknown {
            self.base_type(value_ty)
          } else {
            binding.ty
          };
          self.insert_binding_in_scope(
            scope_idx,
            id.stx.name.clone(),
            next_binding_ty,
            binding.type_params,
          );
          return self.base_type(value_ty);
        } else {
          let value_ty = self.check_expr(right);
          self.insert_binding(id.stx.name.clone(), value_ty, Vec::new());
          return value_ty;
        }
      }
      AstExpr::Member(mem) => {
        let obj_ty = self.check_expr(&mem.stx.left);
        let target_ty = self.member_type(obj_ty, &mem.stx.right);
        let mut value_ty = if matches!(op, OperatorName::Assignment) && target_ty != prim.unknown {
          self.check_expr_with_expected(right, target_ty)
        } else {
          self.check_expr(right)
        };
        if target_ty != prim.unknown {
          if matches!(op, OperatorName::Assignment) {
            if let AstExpr::LitObj(obj) = right.stx.as_ref() {
              if let Some(range) = self.excess_property_range(obj, target_ty) {
                self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                  "excess property",
                  Span {
                    file: self.file,
                    range,
                  },
                ));
              }
            }
          }
          if matches!(op, OperatorName::Assignment) && !self.relate.is_assignable(value_ty, target_ty)
          {
            if let Some(instantiated) =
              self.contextually_instantiate_generic_callable(value_ty, target_ty)
            {
              value_ty = instantiated;
            }
          }
          if !self.relate.is_assignable(value_ty, target_ty) {
            self.diagnostics.push(codes::TYPE_MISMATCH.error(
              "assignment type mismatch",
              Span {
                file: self.file,
                range: loc_to_range(self.file, left.loc),
              },
            ));
          }
        }
        return value_ty;
      }
      AstExpr::ComputedMember(mem) => {
        let obj_ty = self.check_expr(&mem.stx.object);
        let _ = self.check_expr(&mem.stx.member);
        let prop = match mem.stx.member.stx.as_ref() {
          AstExpr::LitStr(str_lit) => Some(str_lit.stx.value.clone()),
          AstExpr::LitNum(num) => Some(num.stx.value.0.to_string()),
          _ => None,
        };
        let target_ty = prop
          .as_deref()
          .map(|key| self.member_type(obj_ty, key))
          .unwrap_or(prim.unknown);
        let mut value_ty = if matches!(op, OperatorName::Assignment) && target_ty != prim.unknown {
          self.check_expr_with_expected(right, target_ty)
        } else {
          self.check_expr(right)
        };
        if target_ty != prim.unknown {
          if matches!(op, OperatorName::Assignment) {
            if let AstExpr::LitObj(obj) = right.stx.as_ref() {
              if let Some(range) = self.excess_property_range(obj, target_ty) {
                self.diagnostics.push(codes::EXCESS_PROPERTY.error(
                  "excess property",
                  Span {
                    file: self.file,
                    range,
                  },
                ));
              }
            }
          }
          if matches!(op, OperatorName::Assignment) && !self.relate.is_assignable(value_ty, target_ty)
          {
            if let Some(instantiated) =
              self.contextually_instantiate_generic_callable(value_ty, target_ty)
            {
              value_ty = instantiated;
            }
          }
          if !self.relate.is_assignable(value_ty, target_ty) {
            self.diagnostics.push(codes::TYPE_MISMATCH.error(
              "assignment type mismatch",
              Span {
                file: self.file,
                range: loc_to_range(self.file, left.loc),
              },
            ));
          }
        }
        return value_ty;
      }
      AstExpr::ArrPat(arr) => {
        let value_ty = self.check_expr(right);
        let span = loc_to_range(self.file, arr.loc);
        if let Some(pat) = self.index.pats.get(&span).copied() {
          let pat = unsafe { &*pat };
          self.bind_pattern(pat, value_ty);
        }
        return value_ty;
      }
      AstExpr::ObjPat(obj) => {
        let value_ty = self.check_expr(right);
        let span = loc_to_range(self.file, obj.loc);
        if let Some(pat) = self.index.pats.get(&span).copied() {
          let pat = unsafe { &*pat };
          self.bind_pattern(pat, value_ty);
        }
        return value_ty;
      }
      _ => {}
    }
    self.check_expr(right)
  }

  fn check_expr_with_expected(&mut self, expr: &Node<AstExpr>, expected: TypeId) -> TypeId {
    let expected = self.store.canon(expected);
    let prim = self.store.primitive_ids();
    if expected == prim.unknown {
      return self.check_expr(expr);
    }

    let ty = match expr.stx.as_ref() {
      AstExpr::LitObj(obj) => self.object_literal_type_with_expected(obj, expected),
      AstExpr::LitArr(arr) => self.array_literal_type_with_expected(arr, expected),
      AstExpr::ArrowFunc(arrow) => self
        .refine_function_expr_with_expected(&arrow.stx.func, expected)
        .or_else(|| self.contextual_callable_type(expected))
        .unwrap_or_else(|| self.check_expr(expr)),
      AstExpr::Func(func) => self
        .refine_function_expr_with_expected(&func.stx.func, expected)
        .or_else(|| self.contextual_callable_type(expected))
        .unwrap_or_else(|| self.check_expr(expr)),
      AstExpr::Call(call) => {
        let contextual_return = match self.store.type_kind(expected) {
          TypeKind::Any | TypeKind::Unknown => None,
          _ => Some(expected),
        };
        self.check_call_expr(call, contextual_return)
      }
      AstExpr::TaggedTemplate(tagged) => {
        let contextual_return = match self.store.type_kind(expected) {
          TypeKind::Any | TypeKind::Unknown => None,
          _ => Some(expected),
        };
        self.check_tagged_template_expr(tagged, expr.loc, contextual_return)
      }
      AstExpr::Unary(un) if matches!(un.stx.operator, OperatorName::New) => {
        let contextual_return = match self.store.type_kind(expected) {
          TypeKind::Any | TypeKind::Unknown => None,
          _ => Some(expected),
        };
        self.check_new_expr(un, expr.loc, contextual_return)
      }
      _ => self.check_expr(expr),
    };

    let contextual = self.contextual_arg_type(ty, expected);
    self.record_expr_type(expr.loc, contextual);
    contextual
  }

  fn refine_function_expr_with_expected(
    &mut self,
    func: &Node<Func>,
    expected: TypeId,
  ) -> Option<TypeId> {
    let expected_sig = {
      let mut param_count = func.stx.parameters.len();
      if matches!(
        func.stx.parameters.first().map(|p| p.stx.pattern.stx.pat.stx.as_ref()),
        Some(AstPat::Id(id)) if id.stx.name == "this"
      ) {
        param_count = param_count.saturating_sub(1);
      }
      self
        .contextual_signature_for_function_expr(expected, param_count)
        .or_else(|| self.first_callable_signature(expected))?
    };
    let prim = self.store.primitive_ids();

    // TypeScript performs contextual signature instantiation when a generic
    // contextual function type is used to type a non-generic function/arrow
    // expression:
    //
    //   const f: <T>(x: T) => T = x => 1;
    //
    // It infers concrete type arguments for the contextual signature's type
    // parameters from the function expression, then uses the instantiated (now
    // non-generic) signature for contextual typing.
    let expected_sig = if !expected_sig.type_params.is_empty() && func.stx.type_parameters.is_none()
    {
      let saved_no_implicit_any = self.no_implicit_any;
      // When computing the "actual" signature for inference, treat untyped
      // parameters as `unknown` (not `any`) regardless of `--noImplicitAny`.
      self.no_implicit_any = false;

      // `function_type` may emit diagnostics while inferring the return type.
      // Those diagnostics should not leak into the final checking pass that
      // uses the instantiated contextual signature.
      let saved_diag_len = self.diagnostics.len();
      let actual_ty = self.function_type(func);
      self.diagnostics.truncate(saved_diag_len);

      self.no_implicit_any = saved_no_implicit_any;
      match self.first_callable_signature(actual_ty) {
        Some(mut actual_sig) => {
          // TypeScript widens literal return types when inferring contextual type
          // arguments (`() => 1` contributes `number`, not `1`).
          actual_sig.ret = self.base_type(actual_sig.ret);

          let inference = infer_type_arguments_from_contextual_signature(
            &self.store,
            &self.relate,
            &expected_sig.type_params,
            &expected_sig,
            &actual_sig,
          );

          let prim = self.store.primitive_ids();
          let inferred_anything_concrete = expected_sig.type_params.iter().any(|tp| {
            inference
              .substitutions
              .get(&tp.id)
              .is_some_and(|arg| self.store.canon(*arg) != prim.unknown)
          });

          if inference.diagnostics.is_empty() && inferred_anything_concrete {
            let mut substituter =
              Substituter::new(Arc::clone(&self.store), inference.substitutions);
            let instantiated_sig_id = substituter.substitute_signature(&expected_sig);
            let mut instantiated_sig = self.store.signature(instantiated_sig_id);
            // This is the result of contextual instantiation; it should not retain
            // the original type parameters.
            instantiated_sig.type_params.clear();
            instantiated_sig
          } else {
            expected_sig
          }
        }
        None => expected_sig,
      }
    } else {
      expected_sig
    };

    let saved_expected = self.expected_return;
    let saved_async = self.in_async_function;
    let saved_returns = std::mem::take(&mut self.return_types);
    let saved_this = self.current_this_ty;
    let saved_super = self.current_super_ty;
    let saved_super_ctor = self.current_super_ctor_ty;

    if !func.stx.arrow {
      // Non-arrow functions do not lexically capture `this`/`super`.
      self.current_this_ty = prim.unknown;
      self.current_super_ty = prim.unknown;
      self.current_super_ctor_ty = prim.unknown;
    }

    self.in_async_function = func.stx.async_;
    self.expected_return = Some(if func.stx.async_ {
      awaited_type(self.store.as_ref(), expected_sig.ret, self.ref_expander)
    } else {
      expected_sig.ret
    });
    let use_contextual_type_params =
      func.stx.type_parameters.is_none() && !expected_sig.type_params.is_empty();
    if use_contextual_type_params {
      self.type_param_scopes.push(expected_sig.type_params.clone());
    }

    let explicit_this_ty = func.stx.parameters.first().and_then(|param| {
      matches!(
        param.stx.pattern.stx.pat.stx.as_ref(),
        AstPat::Id(id) if id.stx.name == "this"
      )
      .then(|| {
        param
          .stx
          .type_annotation
          .as_ref()
          .map(|ann| self.lowerer.lower_type_expr(ann))
      })
      .flatten()
    });
    let contextual_this_ty = expected_sig
      .this_param
      .filter(|ty| *ty != prim.unknown);
    if let Some(this_ty) = explicit_this_ty.or(contextual_this_ty) {
      self.current_this_ty = this_ty;
    }
    self.scopes.push(Scope::default());
    self.var_scopes.push(self.scopes.len().saturating_sub(1));
    let bind_type_params = if use_contextual_type_params {
      expected_sig.type_params.as_slice()
    } else {
      &[]
    };
    self.bind_params(func, bind_type_params, Some(&expected_sig));
    self.check_function_body(func);
    self.var_scopes.pop();
    self.scopes.pop();
    if use_contextual_type_params {
      self.type_param_scopes.pop();
    }

    let inferred_ret = if self.return_types.is_empty() {
      prim.void
    } else {
      self.store.union(self.return_types.clone())
    };

    self.return_types = saved_returns;
    self.expected_return = saved_expected;
    self.in_async_function = saved_async;
    self.current_this_ty = saved_this;
    self.current_super_ty = saved_super;
    self.current_super_ctor_ty = saved_super_ctor;

    let mut instantiated = expected_sig;
    instantiated.ret = if func.stx.async_ {
      self.async_function_return_type(inferred_ret)
    } else {
      inferred_ret
    };
    let sig_id = self.store.intern_signature(instantiated);
    Some(self.store.intern_type(TypeKind::Callable {
      overloads: vec![sig_id],
    }))
  }

  fn instantiate_generic_contextual_signature_for_function_expr(
    &mut self,
    func: &Node<Func>,
    contextual_sig: &Signature,
  ) -> Option<Signature> {
    if contextual_sig.type_params.is_empty() {
      return None;
    }
    // Explicitly generic functions are checked against the generic contextual
    // signature directly; only non-generic expressions participate in
    // contextual signature instantiation.
    if func.stx.type_parameters.is_some() {
      return None;
    }

    let prim = self.store.primitive_ids();

    // Build an "actual" signature for the function expression. Parameter types
    // come from annotations (or `unknown`), and the return type is inferred from
    // a probe body check with no contextual expected return.
    let mut actual_params = Vec::new();
    for (idx, param) in func.stx.parameters.iter().enumerate() {
      if idx == 0
        && matches!(
          param.stx.pattern.stx.pat.stx.as_ref(),
          AstPat::Id(id) if id.stx.name == "this"
        )
      {
        // `this` parameters are modeled on signatures via `this_param`, not as
        // regular parameters.
        continue;
      }
      let ty = param
        .stx
        .type_annotation
        .as_ref()
        .map(|ann| self.lowerer.lower_type_expr(ann))
        .unwrap_or(prim.unknown);
      actual_params.push(SigParam {
        name: None,
        ty,
        optional: param.stx.optional,
        rest: param.stx.rest,
      });
    }

    let actual_ret = if let Some(ann) = func.stx.return_type.as_ref() {
      self.lowerer.lower_type_expr(ann)
    } else if func.stx.body.is_some() {
      // Probe the body to infer a return type, but discard diagnostics so the
      // real contextual check is the only source of errors.
      let saved_diag_len = self.diagnostics.len();
      let saved_implicit_any = self.implicit_any_reported.clone();
      let saved_jsx_namespace_missing = self.jsx_namespace_missing_reported;
      let saved_no_implicit_any = self.no_implicit_any;

      let saved_expected = self.expected_return;
      let saved_async = self.in_async_function;
      let saved_returns = std::mem::take(&mut self.return_types);

      self.in_async_function = func.stx.async_;
      self.expected_return = None;
      // Always use `unknown` for unannotated parameters during the probe
      // inference pass (avoid `--noImplicitAny` diagnostics + `any`).
      self.no_implicit_any = false;

      self.scopes.push(Scope::default());
      self.var_scopes.push(self.scopes.len().saturating_sub(1));
      self.bind_params(func, &[], None);
      self.check_function_body(func);
      self.var_scopes.pop();
      self.scopes.pop();

      let inferred_ret = if self.return_types.is_empty() {
        prim.void
      } else {
        self.store.union(self.return_types.clone())
      };
      let inferred_ret = if func.stx.async_ {
        self.async_function_return_type(inferred_ret)
      } else {
        inferred_ret
      };

      self.return_types = saved_returns;
      self.expected_return = saved_expected;
      self.in_async_function = saved_async;
      self.no_implicit_any = saved_no_implicit_any;

      self.diagnostics.truncate(saved_diag_len);
      self.implicit_any_reported = saved_implicit_any;
      self.jsx_namespace_missing_reported = saved_jsx_namespace_missing;

      inferred_ret
    } else {
      prim.unknown
    };

    let actual_sig = Signature {
      params: actual_params,
      ret: actual_ret,
      type_params: Vec::new(),
      this_param: None,
    };

    let inference = infer_type_arguments_from_contextual_signature(
      &self.store,
      &self.relate,
      &contextual_sig.type_params,
      contextual_sig,
      &actual_sig,
    );
    if !inference.diagnostics.is_empty() {
      return None;
    }

    let mut substituter = Substituter::new(Arc::clone(&self.store), inference.substitutions);
    let instantiated_id = substituter.substitute_signature(contextual_sig);
    let mut instantiated_sig = self.store.signature(instantiated_id);
    instantiated_sig.type_params.clear();
    Some(instantiated_sig)
  }

  fn contextual_signature_for_function_expr(
    &mut self,
    expected: TypeId,
    param_count: usize,
  ) -> Option<Signature> {
    fn collect_all_signatures(
      checker: &Checker<'_>,
      ty: TypeId,
      out: &mut Vec<SignatureId>,
      seen: &mut HashSet<TypeId>,
    ) {
      let ty = checker.expand_callable_type(ty);
      if !seen.insert(ty) {
        return;
      }
      match checker.store.type_kind(ty) {
        TypeKind::Callable { overloads } => out.extend(overloads.iter().copied()),
        TypeKind::Object(obj) => {
          let shape = checker.store.shape(checker.store.object(obj).shape);
          out.extend(shape.call_signatures.iter().copied());
        }
        TypeKind::Union(members) | TypeKind::Intersection(members) => {
          for member in members {
            collect_all_signatures(checker, member, out, seen);
          }
        }
        _ => {}
      }
    }

    let mut sigs = Vec::new();
    collect_all_signatures(self, expected, &mut sigs, &mut HashSet::new());
    sigs.sort();
    sigs.dedup();

    let mut matching: Vec<(SignatureId, Signature)> = sigs
      .into_iter()
      .filter_map(|sig_id| {
        let sig = self.store.signature(sig_id);
        signature_allows_arg_count(self.store.as_ref(), &sig, param_count).then_some((sig_id, sig))
      })
      .collect();

    let Some((_, first)) = matching.first() else {
      return None;
    };

    if matching.len() == 1 {
      return Some(first.clone());
    }

    let first = first.clone();
    matching.retain(|(_, sig)| sig.type_params.is_empty());
    if matching.is_empty() {
      // If all viable signatures are generic, still pick a contextual signature
      // so we can perform contextual signature instantiation during refinement.
      return Some(first);
    }
    if matching.len() == 1 {
      return Some(matching.pop().expect("single signature").1);
    }

    matching.sort_by_key(|(sig_id, _)| *sig_id);

    let prim = self.store.primitive_ids();
    let merged_len = matching
      .iter()
      .map(|(_, sig)| sig.params.len())
      .max()
      .unwrap_or(0);
    let has_rest = matching
      .iter()
      .any(|(_, sig)| sig.params.iter().any(|p| p.rest));
    let mut params = Vec::with_capacity(merged_len);

    for idx in 0..merged_len {
      let rest = has_rest && merged_len > 0 && idx == merged_len - 1;
      let optional = matching
        .iter()
        .any(|(_, sig)| signature_allows_arg_count(self.store.as_ref(), sig, idx));

      let mut member_tys: Vec<TypeId> = Vec::new();
      for (_, sig) in matching.iter() {
        if !signature_allows_arg_count(self.store.as_ref(), sig, idx + 1) {
          continue;
        }

        if rest {
          let rest_idx = sig.params.iter().position(|p| p.rest);
          if let Some(rest_idx) = rest_idx {
            if idx >= rest_idx {
              member_tys.push(sig.params[rest_idx].ty);
              continue;
            }
          }

          if let Some(elem_ty) = expected_arg_type_at(self.store.as_ref(), sig, idx) {
            member_tys.push(self.store.intern_type(TypeKind::Array {
              ty: elem_ty,
              readonly: false,
            }));
          }
        } else if let Some(ty) = expected_arg_type_at(self.store.as_ref(), sig, idx) {
          member_tys.push(ty);
        }
      }

      member_tys.sort();
      member_tys.dedup();
      let ty = match member_tys.len() {
        0 => prim.unknown,
        1 => member_tys[0],
        _ => self.store.union(member_tys),
      };
      params.push(SigParam {
        name: None,
        ty,
        optional,
        rest,
      });
    }

    let mut ret_tys: Vec<TypeId> = matching.iter().map(|(_, sig)| sig.ret).collect();
    ret_tys.sort();
    ret_tys.dedup();
    let ret = match ret_tys.len() {
      0 => prim.unknown,
      1 => ret_tys[0],
      _ => self.store.union(ret_tys),
    };

    let mut this_tys: Vec<TypeId> = matching.iter().filter_map(|(_, sig)| sig.this_param).collect();
    this_tys.sort();
    this_tys.dedup();
    let this_param = match this_tys.len() {
      0 => None,
      1 => Some(this_tys[0]),
      _ => Some(self.store.union(this_tys)),
    };

    Some(Signature {
      params,
      ret,
      type_params: Vec::new(),
      this_param,
    })
  }

  fn first_callable_signature(&self, ty: TypeId) -> Option<Signature> {
    let ty = self.expand_callable_type(ty);
    match self.store.type_kind(ty) {
      TypeKind::Callable { overloads } => overloads.first().map(|sig| self.store.signature(*sig)),
      TypeKind::Object(obj) => {
        let shape = self.store.shape(self.store.object(obj).shape);
        shape
          .call_signatures
          .first()
          .map(|sig_id| self.store.signature(*sig_id))
      }
      TypeKind::Union(members) | TypeKind::Intersection(members) => members
        .iter()
        .copied()
        .find_map(|member| self.first_callable_signature(member)),
      TypeKind::Ref { .. } => None,
      _ => None,
    }
  }

  fn contextual_callable_type(&mut self, ty: TypeId) -> Option<TypeId> {
    fn inner(checker: &mut Checker<'_>, ty: TypeId, seen: &mut HashSet<TypeId>) -> Option<TypeId> {
      if !seen.insert(ty) {
        return None;
      }
      match checker.store.type_kind(ty) {
        TypeKind::Callable { .. } => Some(ty),
        TypeKind::TypeParam(param) => checker
          .type_param_constraint(param)
          .and_then(|constraint| inner(checker, constraint, seen)),
        TypeKind::Object(obj) => {
          let shape = checker.store.shape(checker.store.object(obj).shape);
          if shape.call_signatures.is_empty() {
            None
          } else {
            let mut overloads = shape.call_signatures.clone();
            overloads.sort();
            overloads.dedup();
            Some(checker.store.intern_type(TypeKind::Callable { overloads }))
          }
        }
        TypeKind::Union(members) | TypeKind::Intersection(members) => members
          .iter()
          .copied()
          .find_map(|member| inner(checker, member, seen)),
        TypeKind::Ref { def, args } => checker
          .ref_expander
          .and_then(|expander| expander.expand_ref(checker.store.as_ref(), def, &args))
          .and_then(|expanded| inner(checker, expanded, seen)),
        _ => None,
      }
    }
    inner(self, ty, &mut HashSet::new())
  }

  fn bind_pattern(&mut self, pat: &Node<AstPat>, value: TypeId) {
    self.bind_pattern_with_type_params(pat, value, Vec::new());
  }

  fn bind_pattern_in_scope(&mut self, pat: &Node<AstPat>, value: TypeId, scope_index: usize) {
    self.bind_pattern_with_type_params_in_scope(pat, value, Vec::new(), scope_index);
  }

  fn bind_pattern_with_type_params(
    &mut self,
    pat: &Node<AstPat>,
    value: TypeId,
    type_params: Vec<TypeParamDecl>,
  ) {
    self.record_pat_type(pat.loc, value);
    match pat.stx.as_ref() {
      AstPat::Id(id) => {
        self.insert_binding(id.stx.name.clone(), value, type_params);
      }
      AstPat::Arr(arr) => self.bind_array_pattern(arr, value, type_params),
      AstPat::Obj(obj) => self.bind_object_pattern(obj, value, type_params),
      AstPat::AssignTarget(expr) => {
        let target_ty = self.check_expr(expr);
        if !self.relate.is_assignable(value, target_ty) {
          self.diagnostics.push(codes::TYPE_MISMATCH.error(
            "assignment type mismatch",
            Span {
              file: self.file,
              range: loc_to_range(self.file, pat.loc),
            },
          ));
        }
      }
    }
  }

  fn bind_pattern_with_type_params_in_scope(
    &mut self,
    pat: &Node<AstPat>,
    value: TypeId,
    type_params: Vec<TypeParamDecl>,
    scope_index: usize,
  ) {
    self.record_pat_type(pat.loc, value);
    match pat.stx.as_ref() {
      AstPat::Id(id) => {
        self.insert_binding_in_scope(scope_index, id.stx.name.clone(), value, type_params);
      }
      AstPat::Arr(arr) => self.bind_array_pattern_in_scope(arr, value, type_params, scope_index),
      AstPat::Obj(obj) => self.bind_object_pattern_in_scope(obj, value, type_params, scope_index),
      AstPat::AssignTarget(expr) => {
        let target_ty = self.check_expr(expr);
        if !self.relate.is_assignable(value, target_ty) {
          self.diagnostics.push(codes::TYPE_MISMATCH.error(
            "assignment type mismatch",
            Span {
              file: self.file,
              range: loc_to_range(self.file, pat.loc),
            },
          ));
        }
      }
    }
  }

  fn bind_array_pattern(
    &mut self,
    arr: &Node<ArrPat>,
    value: TypeId,
    type_params: Vec<TypeParamDecl>,
  ) {
    let prim = self.store.primitive_ids();
    let element_ty = match self.store.type_kind(value) {
      TypeKind::Array { ty, .. } => ty,
      TypeKind::Tuple(elems) => elems.first().map(|e| e.ty).unwrap_or(prim.unknown),
      TypeKind::Any => prim.any,
      _ => prim.unknown,
    };
    for (idx, elem) in arr.stx.elements.iter().enumerate() {
      if let Some(elem) = elem {
        let mut target_ty = match self.store.type_kind(value) {
          TypeKind::Tuple(elems) => elems.get(idx).map(|e| e.ty).unwrap_or(element_ty),
          _ => element_ty,
        };
        if let Some(default) = &elem.default_value {
          let default_ty = self.check_expr(default);
          target_ty = self.store.union(vec![target_ty, default_ty]);
        }
        self.bind_pattern(&elem.target, target_ty);
      }
    }
    if let Some(rest) = &arr.stx.rest {
      let rest_ty = match self.store.type_kind(value) {
        TypeKind::Array { ty, readonly } => {
          self.store.intern_type(TypeKind::Array { ty, readonly })
        }
        TypeKind::Any => self.store.intern_type(TypeKind::Array {
          ty: prim.any,
          readonly: false,
        }),
        TypeKind::Tuple(elems) => {
          let elems: Vec<TypeId> = elems.into_iter().map(|e| e.ty).collect();
          let elem_ty = if elems.is_empty() {
            prim.unknown
          } else {
            self.store.union(elems)
          };
          self.store.intern_type(TypeKind::Array {
            ty: elem_ty,
            readonly: false,
          })
        }
        _ => self.store.intern_type(TypeKind::Array {
          ty: prim.unknown,
          readonly: false,
        }),
      };
      self.bind_pattern_with_type_params(rest, rest_ty, type_params.clone());
    }
  }

  fn bind_array_pattern_in_scope(
    &mut self,
    arr: &Node<ArrPat>,
    value: TypeId,
    type_params: Vec<TypeParamDecl>,
    scope_index: usize,
  ) {
    let prim = self.store.primitive_ids();
    let element_ty = match self.store.type_kind(value) {
      TypeKind::Array { ty, .. } => ty,
      TypeKind::Tuple(elems) => elems.first().map(|e| e.ty).unwrap_or(prim.unknown),
      TypeKind::Any => prim.any,
      _ => prim.unknown,
    };
    for (idx, elem) in arr.stx.elements.iter().enumerate() {
      if let Some(elem) = elem {
        let mut target_ty = match self.store.type_kind(value) {
          TypeKind::Tuple(elems) => elems.get(idx).map(|e| e.ty).unwrap_or(element_ty),
          _ => element_ty,
        };
        if let Some(default) = &elem.default_value {
          let default_ty = self.check_expr(default);
          target_ty = self.store.union(vec![target_ty, default_ty]);
        }
        self.bind_pattern_in_scope(&elem.target, target_ty, scope_index);
      }
    }
    if let Some(rest) = &arr.stx.rest {
      let rest_ty = match self.store.type_kind(value) {
        TypeKind::Array { ty, readonly } => self.store.intern_type(TypeKind::Array { ty, readonly }),
        TypeKind::Any => self.store.intern_type(TypeKind::Array {
          ty: prim.any,
          readonly: false,
        }),
        TypeKind::Tuple(elems) => {
          let elems: Vec<TypeId> = elems.into_iter().map(|e| e.ty).collect();
          let elem_ty = if elems.is_empty() {
            prim.unknown
          } else {
            self.store.union(elems)
          };
          self.store.intern_type(TypeKind::Array {
            ty: elem_ty,
            readonly: false,
          })
        }
        _ => self.store.intern_type(TypeKind::Array {
          ty: prim.unknown,
          readonly: false,
        }),
      };
      self.bind_pattern_with_type_params_in_scope(rest, rest_ty, type_params.clone(), scope_index);
    }
  }

  fn bind_object_pattern(
    &mut self,
    obj: &Node<ObjPat>,
    value: TypeId,
    type_params: Vec<TypeParamDecl>,
  ) {
    let prim = self.store.primitive_ids();
    let value_is_any = matches!(self.store.type_kind(value), TypeKind::Any);
    for prop in obj.stx.properties.iter() {
      let mut prop_ty = if value_is_any { prim.any } else { prim.unknown };
      match &prop.stx.key {
        ClassOrObjKey::Direct(direct) => {
          if !value_is_any {
            if let Some(ty) = self.member_type_opt(value, &direct.stx.key) {
              let key_range = loc_to_range(self.file, direct.loc);
              self.check_member_access_for_type(
                value,
                &direct.stx.key,
                key_range,
                MemberAccessReceiver::Other,
                false,
              );
              prop_ty = ty;
            }
          }
        }
        ClassOrObjKey::Computed(expr) => {
          // Always type-check the key expression so identifier resolution and
          // other nested diagnostics are produced.
          let _ = self.check_expr(expr);

          if !value_is_any {
            let literal_key = match expr.stx.as_ref() {
              AstExpr::LitStr(str_lit) => Some(str_lit.stx.value.clone()),
              AstExpr::LitNum(num_lit) => Some(num_lit.stx.value.0.to_string()),
              AstExpr::LitTemplate(tpl) => {
                let mut out = String::new();
                let mut has_substitution = false;
                for part in tpl.stx.parts.iter() {
                  match part {
                    parse_js::ast::expr::lit::LitTemplatePart::String(s) => out.push_str(s),
                    parse_js::ast::expr::lit::LitTemplatePart::Substitution(_) => {
                      has_substitution = true;
                      break;
                    }
                  }
                }
                if has_substitution { None } else { Some(out) }
              }
              _ => None,
            };
            if let Some(key) = literal_key {
              if let Some(ty) = self.member_type_opt(value, &key) {
                let key_range = loc_to_range(self.file, expr.loc);
                self.check_member_access_for_type(
                  value,
                  &key,
                  key_range,
                  MemberAccessReceiver::Other,
                  false,
                );
                prop_ty = ty;
              }
            }
          }
        }
      }
      if let Some(default) = &prop.stx.default_value {
        let default_ty = self.check_expr(default);
        prop_ty = self.store.union(vec![prop_ty, default_ty]);
      }
      self.bind_pattern(&prop.stx.target, prop_ty);
    }
    if let Some(rest) = &obj.stx.rest {
      self.bind_pattern_with_type_params(rest, value, type_params);
    }
  }

  fn bind_object_pattern_in_scope(
    &mut self,
    obj: &Node<ObjPat>,
    value: TypeId,
    type_params: Vec<TypeParamDecl>,
    scope_index: usize,
  ) {
    let prim = self.store.primitive_ids();
    let value_is_any = matches!(self.store.type_kind(value), TypeKind::Any);
    for prop in obj.stx.properties.iter() {
      let mut prop_ty = if value_is_any { prim.any } else { prim.unknown };
      match &prop.stx.key {
        ClassOrObjKey::Direct(direct) => {
          if !value_is_any {
            if let Some(ty) = self.member_type_opt(value, &direct.stx.key) {
              let key_range = loc_to_range(self.file, direct.loc);
              self.check_member_access_for_type(
                value,
                &direct.stx.key,
                key_range,
                MemberAccessReceiver::Other,
                false,
              );
              prop_ty = ty;
            }
          }
        }
        ClassOrObjKey::Computed(expr) => {
          // Always type-check the key expression so identifier resolution and
          // other nested diagnostics are produced.
          let _ = self.check_expr(expr);

          if !value_is_any {
            let literal_key = match expr.stx.as_ref() {
              AstExpr::LitStr(str_lit) => Some(str_lit.stx.value.clone()),
              AstExpr::LitNum(num_lit) => Some(num_lit.stx.value.0.to_string()),
              AstExpr::LitTemplate(tpl) => {
                let mut out = String::new();
                let mut has_substitution = false;
                for part in tpl.stx.parts.iter() {
                  match part {
                    parse_js::ast::expr::lit::LitTemplatePart::String(s) => out.push_str(s),
                    parse_js::ast::expr::lit::LitTemplatePart::Substitution(_) => {
                      has_substitution = true;
                      break;
                    }
                  }
                }
                if has_substitution { None } else { Some(out) }
              }
              _ => None,
            };
            if let Some(key) = literal_key {
              if let Some(ty) = self.member_type_opt(value, &key) {
                let key_range = loc_to_range(self.file, expr.loc);
                self.check_member_access_for_type(
                  value,
                  &key,
                  key_range,
                  MemberAccessReceiver::Other,
                  false,
                );
                prop_ty = ty;
              }
            }
          }
        }
      }
      if let Some(default) = &prop.stx.default_value {
        let default_ty = self.check_expr(default);
        prop_ty = self.store.union(vec![prop_ty, default_ty]);
      }
      self.bind_pattern_in_scope(&prop.stx.target, prop_ty, scope_index);
    }
    if let Some(rest) = &obj.stx.rest {
      self.bind_pattern_with_type_params_in_scope(rest, value, type_params, scope_index);
    }
  }

  fn base_type(&self, ty: TypeId) -> TypeId {
    match self.store.type_kind(ty) {
      TypeKind::BooleanLiteral(_) => self.store.primitive_ids().boolean,
      TypeKind::NumberLiteral(_) => self.store.primitive_ids().number,
      TypeKind::StringLiteral(_) => self.store.primitive_ids().string,
      TypeKind::BigIntLiteral(_) => self.store.primitive_ids().bigint,
      TypeKind::Union(members) => {
        let mapped: Vec<_> = members.into_iter().map(|m| self.base_type(m)).collect();
        self.store.union(mapped)
      }
      TypeKind::Intersection(members) => {
        let mapped: Vec<_> = members.into_iter().map(|m| self.base_type(m)).collect();
        self.store.intersection(mapped)
      }
      _ => ty,
    }
  }

  fn is_bigint_like(&self, ty: TypeId) -> bool {
    match self.store.type_kind(ty) {
      TypeKind::BigInt | TypeKind::BigIntLiteral(_) => true,
      TypeKind::Union(members) => members.iter().all(|m| self.is_bigint_like(*m)),
      TypeKind::Intersection(members) => members.iter().all(|m| self.is_bigint_like(*m)),
      _ => false,
    }
  }

  fn promise_type(&self, inner: TypeId) -> Option<TypeId> {
    let resolver = self.type_resolver.as_ref()?;
    let def = resolver.resolve_type_name(&["Promise".to_string()])?;
    Some(self.store.canon(self.store.intern_type(TypeKind::Ref {
      def,
      args: vec![inner],
    })))
  }

  fn async_function_return_type(&self, ret: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    let inner = awaited_type(self.store.as_ref(), ret, self.ref_expander);
    self.promise_type(inner).unwrap_or(prim.unknown)
  }

  fn function_type(&mut self, func: &Node<Func>) -> TypeId {
    let mut type_params = Vec::new();
    let pushed_type_params = func.stx.type_parameters.is_some();
    if let Some(params) = func.stx.type_parameters.as_ref() {
      self.lowerer.push_type_param_scope();
      type_params = self.lower_type_params(params);
    }
    let prim = self.store.primitive_ids();
    let mut this_param = None;
    let mut params = Vec::new();
    for (idx, p) in func.stx.parameters.iter().enumerate() {
      if idx == 0
        && matches!(
          p.stx.pattern.stx.pat.stx.as_ref(),
          AstPat::Id(id) if id.stx.name == "this"
        )
      {
        this_param = Some(
          p.stx
            .type_annotation
            .as_ref()
            .map(|t| self.lowerer.lower_type_expr(t))
            .unwrap_or(prim.unknown),
        );
        continue;
      }
      let name = match p.stx.pattern.stx.pat.stx.as_ref() {
        AstPat::Id(id) => Some(self.store.intern_name_ref(&id.stx.name)),
        _ => None,
      };
      params.push(SigParam {
        name,
        ty: p
          .stx
          .type_annotation
          .as_ref()
          .map(|t| self.lowerer.lower_type_expr(t))
          .unwrap_or(prim.unknown),
        optional: p.stx.optional,
        rest: p.stx.rest,
      });
    }
    let ret = func
      .stx
      .return_type
      .as_ref()
      .map(|t| self.lowerer.lower_type_expr(t))
      .unwrap_or(prim.unknown);
    let mut ret = if func.stx.async_ {
      self.async_function_return_type(ret)
    } else {
      ret
    };

    // When no explicit return type is provided, infer it from the function
    // body. This is particularly important for generic arrow functions like:
    //
    //   const id = <T>(x: T) => x;
    //
    // where the return type should be `T`, not `unknown`.
    if func.stx.return_type.is_none() && func.stx.body.is_some() {
      let saved_expected = self.expected_return;
      let saved_async = self.in_async_function;
      let saved_returns = std::mem::take(&mut self.return_types);
      let (saved_this, saved_super, saved_super_ctor) = (
        self.current_this_ty,
        self.current_super_ty,
        self.current_super_ctor_ty,
      );
      let saved_type_resolver = self.type_resolver.clone();
      let saved_lowerer_resolver = saved_type_resolver.clone();

      let pushed_scope = Scope::default();
      let pushed_type_param_scope = !type_params.is_empty();
      if pushed_type_param_scope {
        self.type_param_scopes.push(type_params.clone());
      }

      if !func.stx.arrow {
        let prim = self.store.primitive_ids();
        let func_span = loc_to_range(self.file, func.loc);
        if let Some(ctx) = self.index.class_member_function(func_span) {
          let (this_ty, super_ty) = self.this_super_for_class(ctx.class_index, ctx.is_static);
          self.current_this_ty = this_ty;
          self.current_super_ty = super_ty;
        } else {
          self.current_this_ty = prim.unknown;
          self.current_super_ty = prim.unknown;
        }
        self.current_super_ctor_ty = self.super_ctor_for_span(func_span);
      }

      self.in_async_function = func.stx.async_;
      self.expected_return = None;
      if let Some(this_ty) = this_param {
        self.current_this_ty = this_ty;
      }
      self.scopes.push(pushed_scope);
      self.bind_params(func, &type_params, None);
      // `function_type` can be called while checking an *outer* body (e.g. the
      // top-level body binding a function declaration). When inferring a return
      // type by checking the nested function body, ensure local class
      // declarations inside that body are visible to type references (`C`) and
      // `typeof` queries by layering a body-local resolver.
      //
      // This mirrors `check_body_with_expander`, but is driven directly from the
      // parse-js AST rather than HIR since return inference happens without
      // switching to the function's dedicated HIR body checker.
      let body_resolver = match func.stx.body.as_ref() {
        Some(FuncBody::Block(block)) => {
          let mut local_class_defs: HashMap<String, (TextRange, DefId, DefId)> = HashMap::new();
          fn walk_namespace(
            checker: &Checker<'_>,
            body: &NamespaceBody,
            local_class_defs: &mut HashMap<String, (TextRange, DefId, DefId)>,
          ) {
            match body {
              NamespaceBody::Block(stmts) => {
                for stmt in stmts.iter() {
                  walk_stmt(checker, stmt, local_class_defs);
                }
              }
              NamespaceBody::Namespace(inner) => walk_namespace(checker, &inner.stx.body, local_class_defs),
            }
          }
          fn walk_stmt(
            checker: &Checker<'_>,
            stmt: &Node<Stmt>,
            local_class_defs: &mut HashMap<String, (TextRange, DefId, DefId)>,
          ) {
            match stmt.stx.as_ref() {
              Stmt::Block(block) => {
                for stmt in block.stx.body.iter() {
                  walk_stmt(checker, stmt, local_class_defs);
                }
              }
              Stmt::If(if_stmt) => {
                walk_stmt(checker, &if_stmt.stx.consequent, local_class_defs);
                if let Some(alt) = &if_stmt.stx.alternate {
                  walk_stmt(checker, alt, local_class_defs);
                }
              }
              Stmt::While(while_stmt) => {
                walk_stmt(checker, &while_stmt.stx.body, local_class_defs);
              }
              Stmt::DoWhile(do_while) => {
                walk_stmt(checker, &do_while.stx.body, local_class_defs);
              }
              Stmt::ForTriple(for_stmt) => {
                for stmt in for_stmt.stx.body.stx.body.iter() {
                  walk_stmt(checker, stmt, local_class_defs);
                }
              }
              Stmt::ForIn(for_in) => {
                for stmt in for_in.stx.body.stx.body.iter() {
                  walk_stmt(checker, stmt, local_class_defs);
                }
              }
              Stmt::ForOf(for_of) => {
                for stmt in for_of.stx.body.stx.body.iter() {
                  walk_stmt(checker, stmt, local_class_defs);
                }
              }
              Stmt::Switch(sw) => {
                for branch in sw.stx.branches.iter() {
                  for stmt in branch.stx.body.iter() {
                    walk_stmt(checker, stmt, local_class_defs);
                  }
                }
              }
              Stmt::Try(tr) => {
                for stmt in tr.stx.wrapped.stx.body.iter() {
                  walk_stmt(checker, stmt, local_class_defs);
                }
                if let Some(catch) = &tr.stx.catch {
                  for stmt in catch.stx.body.iter() {
                    walk_stmt(checker, stmt, local_class_defs);
                  }
                }
                if let Some(finally) = &tr.stx.finally {
                  for stmt in finally.stx.body.iter() {
                    walk_stmt(checker, stmt, local_class_defs);
                  }
                }
              }
              Stmt::Label(label) => {
                walk_stmt(checker, &label.stx.statement, local_class_defs);
              }
              Stmt::With(w) => {
                walk_stmt(checker, &w.stx.body, local_class_defs);
              }
              Stmt::NamespaceDecl(ns) => walk_namespace(checker, &ns.stx.body, local_class_defs),
              Stmt::ModuleDecl(module) => {
                if let Some(body) = &module.stx.body {
                  for stmt in body.iter() {
                    walk_stmt(checker, stmt, local_class_defs);
                  }
                }
              }
              Stmt::GlobalDecl(global) => {
                for stmt in global.stx.body.iter() {
                  walk_stmt(checker, stmt, local_class_defs);
                }
              }
              Stmt::ClassDecl(class_decl) => {
                let Some(name) = class_decl.stx.name.as_ref() else {
                  return;
                };
                let name = name.stx.name.clone();
                let stmt_span = loc_to_range(checker.file, stmt.loc);
                let type_def = checker
                  .decl_def_by_span
                  .get(&stmt_span)
                  .copied()
                  .or_else(|| {
                    checker
                      .def_spans
                      .and_then(|spans| spans.get(&(checker.file, stmt_span)).copied())
                  });
                let Some(type_def) = type_def else {
                  return;
                };
                let value_def = checker.value_defs.get(&type_def).copied().unwrap_or(type_def);
                let replace = match local_class_defs.get(&name) {
                  None => true,
                  Some((existing_span, existing_def_id, _)) => {
                    (stmt_span.start, stmt_span.end, type_def.0)
                      < (existing_span.start, existing_span.end, existing_def_id.0)
                  }
                };
                if replace {
                  local_class_defs.insert(name, (stmt_span, type_def, value_def));
                }
              }
              // Do not descend into nested function/class bodies.
              Stmt::FunctionDecl(_) => {}
              _ => {}
            }
          }
          for stmt in block.iter() {
            walk_stmt(self, stmt, &mut local_class_defs);
          }
          let (locals_type, locals_value): (HashMap<String, DefId>, HashMap<String, DefId>) =
            local_class_defs
              .into_iter()
              .map(|(name, (_span, type_def, value_def))| {
                ((name.clone(), type_def), (name, value_def))
              })
              .unzip();

          if locals_type.is_empty() && locals_value.is_empty() {
            saved_type_resolver.clone()
          } else {
            Some(Arc::new(BodyLocalTypeResolver {
              locals_type: locals_type,
              locals_value: locals_value,
              inner: saved_type_resolver.clone(),
            }) as Arc<_>)
          }
        }
        _ => saved_type_resolver.clone(),
      };
      self.lowerer.set_resolver(body_resolver.clone());
      self.type_resolver = body_resolver;
      self.check_function_body(func);
      self.scopes.pop();
      self.type_resolver = saved_type_resolver;
      self.lowerer.set_resolver(saved_lowerer_resolver);

      let inferred_ret = if self.return_types.is_empty() {
        prim.void
      } else {
        self.store.union(self.return_types.clone())
      };

      self.return_types = saved_returns;
      self.expected_return = saved_expected;
      self.in_async_function = saved_async;
      self.current_this_ty = saved_this;
      self.current_super_ty = saved_super;
      self.current_super_ctor_ty = saved_super_ctor;
      if pushed_type_param_scope {
        self.type_param_scopes.pop();
      }

      ret = if func.stx.async_ {
        self.async_function_return_type(inferred_ret)
      } else {
        inferred_ret
      };
    }
    if pushed_type_params {
      self.lowerer.pop_type_param_scope();
    }
    let sig = Signature {
      params,
      ret,
      type_params,
      this_param,
    };
    let sig_id = self.store.intern_signature(sig);
    let ty = self.store.intern_type(TypeKind::Callable {
      overloads: vec![sig_id],
    });
    ty
  }

  fn record_expr_type(&mut self, loc: Loc, ty: TypeId) {
    let range = loc_to_range(self.file, loc);
    if let Some(id) = self.expr_map.get(&range) {
      if let Some(slot) = self.expr_types.get_mut(id.0 as usize) {
        *slot = ty;
      }
    }
  }

  fn record_call_signature(&mut self, loc: Loc, signature: Option<SignatureId>) {
    let range = loc_to_range(self.file, loc);
    if let Some(id) = self.expr_map.get(&range) {
      if let Some(slot) = self.call_signatures.get_mut(id.0 as usize) {
        *slot = signature;
      }
    }
  }

  fn record_pat_type(&mut self, loc: Loc, ty: TypeId) {
    let range = loc_to_range(self.file, loc);
    if let Some(id) = self.pat_map.get(&range) {
      if let Some(slot) = self.pat_types.get_mut(id.0 as usize) {
        *slot = ty;
      }
    }
  }

  fn contextual_arg_type(&self, arg_ty: TypeId, param_ty: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    match (self.store.type_kind(arg_ty), self.store.type_kind(param_ty)) {
      (TypeKind::NumberLiteral(_), TypeKind::Number) => prim.number,
      (TypeKind::StringLiteral(_), TypeKind::String) => prim.string,
      (TypeKind::BooleanLiteral(_), TypeKind::Boolean) => prim.boolean,
      (TypeKind::Union(members), TypeKind::Number) => {
        if members
          .iter()
          .all(|m| matches!(self.store.type_kind(*m), TypeKind::NumberLiteral(_)))
        {
          prim.number
        } else {
          arg_ty
        }
      }
      (TypeKind::Union(members), TypeKind::String) => {
        if members
          .iter()
          .all(|m| matches!(self.store.type_kind(*m), TypeKind::StringLiteral(_)))
        {
          prim.string
        } else {
          arg_ty
        }
      }
      (TypeKind::Union(members), TypeKind::Boolean) => {
        if members
          .iter()
          .all(|m| matches!(self.store.type_kind(*m), TypeKind::BooleanLiteral(_)))
        {
          prim.boolean
        } else {
          arg_ty
        }
      }
      _ => arg_ty,
    }
  }

  fn expand_ref(&self, ty: TypeId) -> TypeId {
    let mut current = self.store.canon(ty);
    let Some(expander) = self.ref_expander else {
      return current;
    };
    let mut seen = HashSet::new();
    while seen.insert(current) {
      match self.store.type_kind(current) {
        TypeKind::Ref { def, args } => {
          if let Some(expanded) = expander.expand_ref(self.store.as_ref(), def, &args) {
            current = self.store.canon(expanded);
            continue;
          }
        }
        _ => {}
      }
      break;
    }
    current
  }

  fn expand_for_props(&self, ty: TypeId) -> TypeId {
    let Some(expander) = self.ref_expander else {
      return ty;
    };
    match self.store.type_kind(ty) {
      TypeKind::Ref { .. } | TypeKind::IndexedAccess { .. } => {}
      _ => return ty,
    }
    struct Adapter<'a> {
      hook: &'a dyn types_ts_interned::RelateTypeExpander,
    }

    impl<'a> TypeExpander for Adapter<'a> {
      fn expand(
        &self,
        store: &TypeStore,
        def: types_ts_interned::DefId,
        args: &[TypeId],
      ) -> Option<ExpandedType> {
        self
          .hook
          .expand_ref(store, def, args)
          .map(|ty| ExpandedType {
            params: Vec::new(),
            ty,
          })
      }
    }

    let adapter = Adapter { hook: expander };
    let mut evaluator =
      TypeEvaluator::with_caches(Arc::clone(&self.store), &adapter, self.eval_caches.clone());
    evaluator.evaluate(ty)
  }

  fn has_excess_properties(
    &self,
    obj: &Node<parse_js::ast::expr::lit::LitObjExpr>,
    target: TypeId,
  ) -> bool {
    let mut props = HashSet::new();
    for member in obj.stx.members.iter() {
      match &member.stx.typ {
        ObjMemberType::Valued { key, .. } => {
          if let ClassOrObjKey::Direct(direct) = key {
            props.insert(direct.stx.key.clone());
          }
        }
        ObjMemberType::Shorthand { id } => {
          props.insert(id.stx.name.clone());
        }
        ObjMemberType::Rest { .. } => return false,
      }
    }
    !self.type_accepts_props(target, &props)
  }

  fn excess_property_range(
    &self,
    obj: &Node<parse_js::ast::expr::lit::LitObjExpr>,
    target: TypeId,
  ) -> Option<TextRange> {
    let mut props = HashSet::new();
    let mut ordered_props = Vec::new();
    for member in obj.stx.members.iter() {
      match &member.stx.typ {
        ObjMemberType::Valued { key, .. } => {
          if let ClassOrObjKey::Direct(direct) = key {
            let name = direct.stx.key.clone();
            props.insert(name.clone());
            ordered_props.push((name, loc_to_range(self.file, direct.loc)));
          }
        }
        ObjMemberType::Shorthand { id } => {
          let name = id.stx.name.clone();
          props.insert(name.clone());
          ordered_props.push((name, loc_to_range(self.file, id.loc)));
        }
        ObjMemberType::Rest { .. } => return None,
      }
    }

    if self.type_accepts_props(target, &props) {
      return None;
    }

    let mut single = HashSet::with_capacity(1);
    for (prop, range) in ordered_props {
      single.clear();
      single.insert(prop);
      if !self.type_accepts_props(target, &single) {
        return Some(range);
      }
    }

    Some(loc_to_range(self.file, obj.loc))
  }

  fn has_contextual_excess_properties(&mut self, expr: &Node<AstExpr>, expected: TypeId) -> bool {
    fn inner(
      checker: &mut Checker<'_>,
      expr: &Node<AstExpr>,
      expected: TypeId,
      depth: usize,
    ) -> bool {
      if depth > 32 {
        return false;
      }
      let prim = checker.store.primitive_ids();
      let expected = checker.store.canon(expected);
      if expected == prim.unknown {
        return false;
      }
      match expr.stx.as_ref() {
        AstExpr::LitObj(obj) => {
          if checker.has_excess_properties(obj, expected) {
            return true;
          }
          for member in obj.stx.members.iter() {
            let (name, value) = match &member.stx.typ {
              ObjMemberType::Valued {
                key: ClassOrObjKey::Direct(key),
                val: ClassOrObjVal::Prop(Some(expr)),
              } => (Some(key.stx.key.as_str()), Some(expr)),
              _ => (None, None),
            };
            let (Some(name), Some(value)) = (name, value) else {
              continue;
            };
            let expected_prop = checker.member_type(expected, name);
            if expected_prop == prim.unknown {
              continue;
            }
            if inner(checker, value, expected_prop, depth + 1) {
              return true;
            }
          }
          false
        }
        AstExpr::LitArr(arr) => {
          use parse_js::ast::expr::lit::LitArrElem;

          if arr
            .stx
            .elements
            .iter()
            .any(|e| !matches!(e, LitArrElem::Single(_)))
          {
            return false;
          }

          let elems: Vec<_> = arr
            .stx
            .elements
            .iter()
            .filter_map(|e| match e {
              LitArrElem::Single(v) => Some(v),
              _ => None,
            })
            .collect();
          let arity = elems.len();
          let contexts = checker.array_literal_context_candidates(expected, arity);
          if contexts.is_empty() {
            return false;
          }

          for context in contexts.into_iter() {
            let passes = match context {
              ArrayLiteralContext::Tuple(expected_elems) => {
                elems.iter().enumerate().all(|(idx, expr)| {
                  let expected_elem = expected_elems
                    .get(idx)
                    .map(|e| e.ty)
                    .unwrap_or(prim.unknown);
                  !inner(checker, expr, expected_elem, depth + 1)
                })
              }
              ArrayLiteralContext::Array(expected_elem) => elems
                .iter()
                .all(|expr| !inner(checker, expr, expected_elem, depth + 1)),
            };
            if passes {
              return false;
            }
          }

          true
        }
        _ => false,
      }
    }

    inner(self, expr, expected, 0)
  }

  fn type_accepts_props(&self, target: TypeId, props: &HashSet<String>) -> bool {
    let target = self.expand_ref(target);
    let expanded = self.expand_for_props(target);
    if expanded != target {
      return self.type_accepts_props(expanded, props);
    }
    match self.store.type_kind(target) {
      TypeKind::Union(members) => {
        let mut saw_object = false;
        for member in members {
          let member = self.expand_ref(self.expand_for_props(member));
          match self.store.type_kind(member) {
            TypeKind::Object(_)
            | TypeKind::Union(_)
            | TypeKind::Intersection(_)
            | TypeKind::Mapped(_) => {
              saw_object = true;
              if self.type_accepts_props(member, props) {
                return true;
              }
            }
            _ => {}
          }
        }
        !saw_object
      }
      TypeKind::Intersection(members) => {
        // Excess property checking on intersection targets behaves like checking
        // against the merged property set of all intersected object types. In
        // particular, `{ a } & { b }` should accept `{ a, b }` without treating
        // either `a` or `b` as excess.
        //
        // To keep union semantics correct, distribute unions inside the
        // intersection into a top-level union before applying the merged-props
        // check. This avoids incorrectly accepting `{ a, b }` for targets like
        // `({ a } | { b }) & { key }`.
        let expanded_members: Vec<TypeId> = members
          .iter()
          .copied()
          .map(|member| self.expand_ref(self.expand_for_props(member)))
          .collect();
        for (idx, member) in expanded_members.iter().enumerate() {
          if let TypeKind::Union(options) = self.store.type_kind(*member) {
            let mut distributed = Vec::with_capacity(options.len());
            for option in options {
              let mut parts = Vec::with_capacity(expanded_members.len());
              for (j, other) in expanded_members.iter().enumerate() {
                if j == idx {
                  continue;
                }
                parts.push(*other);
              }
              parts.push(option);
              distributed.push(self.store.intersection(parts));
            }
            return self.type_accepts_props(self.store.union(distributed), props);
          }
        }

        let mut object_members: Vec<TypeId> = Vec::new();
        for member in expanded_members.iter().copied() {
          match self.store.type_kind(member) {
            TypeKind::Object(_)
            | TypeKind::Union(_)
            | TypeKind::Intersection(_)
            | TypeKind::Mapped(_) => {
              object_members.push(member);
            }
            _ => {}
          }
        }

        if object_members.is_empty() {
          return true;
        }

        let mut single = HashSet::with_capacity(1);
        for prop in props {
          single.clear();
          single.insert(prop.clone());
          if !object_members
            .iter()
            .copied()
            .any(|member| self.type_accepts_props(member, &single))
          {
            return false;
          }
        }
        true
      }
      TypeKind::Object(obj_id) => {
        let shape = self.store.shape(self.store.object(obj_id).shape);
        if !shape.indexers.is_empty() {
          return true;
        }
        let mut allowed: HashSet<String> = HashSet::new();
        for prop in shape.properties.iter() {
          match prop.key {
            PropKey::String(name) | PropKey::Symbol(name) => {
              allowed.insert(self.store.name(name));
            }
            PropKey::Number(num) => {
              allowed.insert(num.to_string());
            }
          }
        }
        props.iter().all(|p| allowed.contains(p))
      }
      TypeKind::Mapped(_) => true,
      TypeKind::Ref { .. } => true,
      _ => true,
    }
  }

  fn is_mapped_type(&self, ty: TypeId) -> bool {
    let ty = self.expand_ref(ty);
    match self.store.type_kind(ty) {
      TypeKind::Mapped(_) => true,
      TypeKind::Ref { .. } => false,
      TypeKind::Union(members) | TypeKind::Intersection(members) => members
        .iter()
        .copied()
        .any(|member| self.is_mapped_type(member)),
      TypeKind::IndexedAccess { .. } => {
        let expanded = self.expand_for_props(ty);
        expanded != ty && self.is_mapped_type(expanded)
      }
      _ => false,
    }
  }

  fn check_assignable(
    &mut self,
    expr: &Node<AstExpr>,
    src: TypeId,
    dst: TypeId,
    range_override: Option<TextRange>,
  ) {
    self.check_assignable_with_code(expr, src, dst, range_override, &codes::TYPE_MISMATCH);
  }

  fn contextually_instantiate_generic_callable(&mut self, src: TypeId, dst: TypeId) -> Option<TypeId> {
    // `tsc` permits assigning a generic function value to a non-generic function
    // type by contextually instantiating the generic signature from the
    // destination signature:
    //
    //   const id = <T>(x: T) => x;
    //   const f: (x: number) => number = id;
    //
    // The low-level relation engine rejects signatures with differing type
    // parameter counts, so we opportunistically perform contextual
    // instantiation and retry the assignability check.
    let contextual_sig = self.first_callable_signature(dst)?;
    if !contextual_sig.type_params.is_empty() {
      return None;
    }

    let src = self.expand_callable_type(src);
    let candidate_sigs =
      callable_signatures_with_expander(self.store.as_ref(), src, self.ref_expander);
    for sig_id in candidate_sigs {
      let actual_sig = self.store.signature(sig_id);
      if actual_sig.type_params.is_empty() {
        continue;
      }

      let inference = infer_type_arguments_from_contextual_signature(
        &self.store,
        &self.relate,
        &actual_sig.type_params,
        &contextual_sig,
        &actual_sig,
      );
      if !inference.diagnostics.is_empty() {
        continue;
      }

      let instantiated_sig_id = self.instantiation_cache.instantiate_signature(
        &self.store,
        sig_id,
        &actual_sig,
        &inference.substitutions,
      );
      let mut instantiated_sig = self.store.signature(instantiated_sig_id);
      instantiated_sig.type_params.clear();
      let instantiated_sig_id = self.store.intern_signature(instantiated_sig);
      let instantiated_callable = self.store.intern_type(TypeKind::Callable {
        overloads: vec![instantiated_sig_id],
      });

      if self.relate.is_assignable(instantiated_callable, dst)
        && self.variance_allows_assignability(instantiated_callable, dst)
      {
        return Some(instantiated_callable);
      }
    }

    None
  }

  fn contextually_instantiate_generic_contextual_callable(
    &mut self,
    expr: &Node<AstExpr>,
    src: TypeId,
    dst: TypeId,
  ) -> Option<TypeId> {
    // TypeScript also permits assigning a *non-generic* function expression to
    // a generic contextual function type by instantiating the contextual
    // signature:
    //
    //   const f: <T>(x: T) => T = x => 1; // T inferred as number
    //
    // This is distinct from `contextually_instantiate_generic_callable`, which
    // handles assigning a generic value to a non-generic function type.
    //
    // Restrict this behavior to function/arrow expressions without explicit
    // type parameters to avoid relaxing assignability for arbitrary values.
    let func = match expr.stx.as_ref() {
      AstExpr::ArrowFunc(arrow) => Some(&arrow.stx.func),
      AstExpr::Func(func) => Some(&func.stx.func),
      _ => None,
    }?;
    if func.stx.type_parameters.is_some() {
      return None;
    }

    let src = self.expand_callable_type(src);
    let dst = self.expand_callable_type(dst);

    let src_sigs = callable_signatures_with_expander(self.store.as_ref(), src, self.ref_expander);
    if src_sigs.is_empty() {
      return None;
    }
    let dst_sigs = callable_signatures_with_expander(self.store.as_ref(), dst, self.ref_expander);
    if dst_sigs.is_empty() {
      return None;
    }

    for dst_sig_id in dst_sigs {
      let dst_sig = self.store.signature(dst_sig_id);
      if dst_sig.type_params.is_empty() {
        continue;
      }

      for src_sig_id in src_sigs.iter().copied() {
        let src_sig = self.store.signature(src_sig_id);
        if !src_sig.type_params.is_empty() {
          continue;
        }

        // Mirror contextual instantiation inference behavior for return types:
        // treat literal returns as their base primitive types.
        let mut widened_src_sig = src_sig.clone();
        widened_src_sig.ret = self.base_type(widened_src_sig.ret);

        let inference = infer_type_arguments_from_contextual_signature(
          &self.store,
          &self.relate,
          &dst_sig.type_params,
          &dst_sig,
          &widened_src_sig,
        );
        if !inference.diagnostics.is_empty() {
          continue;
        }

        let prim = self.store.primitive_ids();
        let inferred_anything_concrete = dst_sig.type_params.iter().any(|tp| {
          inference
            .substitutions
            .get(&tp.id)
            .is_some_and(|arg| self.store.canon(*arg) != prim.unknown)
        });
        if !inferred_anything_concrete {
          continue;
        }

        let instantiated_sig_id = self.instantiation_cache.instantiate_signature(
          &self.store,
          dst_sig_id,
          &dst_sig,
          &inference.substitutions,
        );
        let mut instantiated_sig = self.store.signature(instantiated_sig_id);
        instantiated_sig.type_params.clear();
        let instantiated_sig_id = self.store.intern_signature(instantiated_sig);
        let instantiated_callable = self.store.intern_type(TypeKind::Callable {
          overloads: vec![instantiated_sig_id],
        });

        if self.relate.is_assignable(src, instantiated_callable)
          && self.variance_allows_assignability(src, instantiated_callable)
        {
          return Some(instantiated_callable);
        }
      }
    }

    None
  }

  fn check_assignable_with_code(
    &mut self,
    expr: &Node<AstExpr>,
    src: TypeId,
    dst: TypeId,
    range_override: Option<TextRange>,
    base_code: &codes::Code,
  ) {
    let prim = self.store.primitive_ids();
    if matches!(self.store.type_kind(src), TypeKind::Any | TypeKind::Unknown)
      || matches!(self.store.type_kind(dst), TypeKind::Any | TypeKind::Unknown)
    {
      return;
    }
    if let TypeKind::Array { ty, .. } = self.store.type_kind(src) {
      if matches!(self.store.type_kind(ty), TypeKind::Unknown) {
        return;
      }
    }
    if matches!(self.store.type_kind(src), TypeKind::Conditional { .. })
      || matches!(self.store.type_kind(dst), TypeKind::Conditional { .. })
    {
      return;
    }
    if self.is_mapped_type(dst) {
      return;
    }
    if let AstExpr::LitObj(obj) = expr.stx.as_ref() {
      if let Some(range) = self.excess_property_range(obj, dst) {
        self.diagnostics.push(codes::EXCESS_PROPERTY.error(
          "excess property",
          Span {
            file: self.file,
            range,
          },
        ));
        return;
      }
    }
    if self.relate.is_assignable(src, dst) && self.variance_allows_assignability(src, dst) {
      return;
    }
    if self.contextually_instantiate_generic_callable(src, dst).is_some() {
      return;
    }
    if self
      .contextually_instantiate_generic_contextual_callable(expr, src, dst)
      .is_some()
    {
      return;
    }

    if std::env::var("DEBUG_TYPE_MISMATCH").is_ok() {
      eprintln!(
        "DEBUG_TYPE_MISMATCH src={} {:?} dst={} {:?}",
        TypeDisplay::new(self.store.as_ref(), src),
        self.store.type_kind(src),
        TypeDisplay::new(self.store.as_ref(), dst),
        self.store.type_kind(dst)
      );
    }
    let base_range = range_override.unwrap_or_else(|| loc_to_range(self.file, expr.loc));
    if let AstExpr::LitObj(obj) = expr.stx.as_ref() {
      let mut mismatched_props = Vec::new();
      for member in obj.stx.members.iter() {
        let (prop, key_loc) = match &member.stx.typ {
          ObjMemberType::Valued {
            key: ClassOrObjKey::Direct(key),
            ..
          } => (key.stx.key.clone(), Some(key.loc)),
          ObjMemberType::Shorthand { id } => (id.stx.name.clone(), Some(id.loc)),
          _ => continue,
        };
        let prop_src = self.member_type(src, &prop);
        let prop_dst = self.member_type(dst, &prop);
        if prop_src == prim.unknown || prop_dst == prim.unknown {
          continue;
        }
        if !self.relate.is_assignable(prop_src, prop_dst) {
          mismatched_props.push((prop, key_loc));
        }
      }

      if !mismatched_props.is_empty() {
        for (prop, key_loc) in mismatched_props {
          let range = key_loc
            .map(|loc| loc_to_range(self.file, loc))
            .unwrap_or(base_range);
          let range = TextRange::new(range.start, range.start.saturating_add(prop.len() as u32));
          self.diagnostics.push(codes::TYPE_MISMATCH.error(
            "type mismatch",
            Span {
              file: self.file,
              range,
            },
          ));
        }
        return;
      }
    }

    if let AstExpr::LitArr(arr) = expr.stx.as_ref() {
      if arr
        .stx
        .elements
        .iter()
        .all(|elem| matches!(elem, parse_js::ast::expr::lit::LitArrElem::Single(_)))
      {
        let elems: Vec<_> = arr
          .stx
          .elements
          .iter()
          .filter_map(|elem| match elem {
            parse_js::ast::expr::lit::LitArrElem::Single(expr) => Some(expr),
            _ => None,
          })
          .collect();
        let contexts = self.array_literal_context_candidates(dst, elems.len());
        if let Some(context) = contexts.into_iter().next() {
          let mut mismatched_elems: Vec<&Node<AstExpr>> = Vec::new();
          match context {
            ArrayLiteralContext::Tuple(expected_elems) => {
              for (idx, elem) in elems.iter().enumerate() {
                let expected_elem = expected_elems
                  .get(idx)
                  .map(|e| e.ty)
                  .unwrap_or(prim.unknown);
                if expected_elem == prim.unknown {
                  continue;
                }
                let elem_ty = self.recorded_expr_type(elem.loc).unwrap_or(prim.unknown);
                if elem_ty != prim.unknown && !self.relate.is_assignable(elem_ty, expected_elem) {
                  mismatched_elems.push(elem);
                }
              }
            }
            ArrayLiteralContext::Array(expected_elem) => {
              if expected_elem != prim.unknown {
                for elem in elems.iter() {
                  let elem_ty = self.recorded_expr_type(elem.loc).unwrap_or(prim.unknown);
                  if elem_ty != prim.unknown && !self.relate.is_assignable(elem_ty, expected_elem) {
                    mismatched_elems.push(elem);
                  }
                }
              }
            }
          }

          if !mismatched_elems.is_empty() {
            for elem in mismatched_elems {
              self.diagnostics.push(codes::TYPE_MISMATCH.error(
                "type mismatch",
                Span {
                  file: self.file,
                  range: loc_to_range(self.file, elem.loc),
                },
              ));
            }
            return;
          }
        }
      }
    }

    self.diagnostics.push(base_code.error(
      "type mismatch",
      Span {
        file: self.file,
        range: base_range,
      },
    ));
  }

  fn variance_allows_assignability(&self, src: TypeId, dst: TypeId) -> bool {
    let Some(type_param_decls) = self.def_type_param_decls else {
      return true;
    };
    let src = self.store.canon(src);
    let dst = self.store.canon(dst);
    let (def, src_args, dst_args) = match (self.store.type_kind(src), self.store.type_kind(dst)) {
      (
        TypeKind::Ref {
          def: src_def,
          args: src_args,
        },
        TypeKind::Ref {
          def: dst_def,
          args: dst_args,
        },
      ) if src_def == dst_def => (src_def, src_args, dst_args),
      _ => return true,
    };
    let Some(decls) = type_param_decls.get(&def) else {
      return true;
    };
    if !decls.iter().any(|decl| decl.variance.is_some()) {
      return true;
    }
    for (idx, decl) in decls.iter().enumerate() {
      let Some(variance) = decl.variance else {
        continue;
      };
      let Some(src_arg) = src_args.get(idx).copied() else {
        continue;
      };
      let Some(dst_arg) = dst_args.get(idx).copied() else {
        continue;
      };
      match variance {
        TypeParamVariance::Out => {
          if !self.relate.is_assignable(src_arg, dst_arg) {
            return false;
          }
        }
        TypeParamVariance::In => {
          if !self.relate.is_assignable(dst_arg, src_arg) {
            return false;
          }
        }
        TypeParamVariance::InOut => {
          if !self.relate.is_assignable(src_arg, dst_arg)
            || !self.relate.is_assignable(dst_arg, src_arg)
          {
            return false;
          }
        }
      }
    }
    true
  }
}

fn contains_range(outer: TextRange, inner: TextRange) -> bool {
  inner.start >= outer.start && inner.end <= outer.end
}

fn ranges_overlap(a: TextRange, b: TextRange) -> bool {
  a.start < b.end && b.start < a.end
}

fn is_empty_jsx_expr_placeholder(expr: &Node<AstExpr>) -> bool {
  expr.loc.is_empty() && matches!(expr.stx.as_ref(), AstExpr::Id(id) if id.stx.name.is_empty())
}

fn span_for_stmt_list(stmts: &[Node<Stmt>], file: FileId) -> Option<TextRange> {
  let mut start: Option<u32> = None;
  let mut end: Option<u32> = None;
  for stmt in stmts {
    let range = loc_to_range(file, stmt.loc);
    start = Some(start.map_or(range.start, |s| s.min(range.start)));
    end = Some(end.map_or(range.end, |e| e.max(range.end)));
  }
  start.zip(end).map(|(s, e)| TextRange::new(s, e))
}

fn body_range(body: &Body) -> TextRange {
  let mut start = u32::MAX;
  let mut end = 0u32;
  for stmt in body.stmts.iter() {
    start = start.min(stmt.span.start);
    end = end.max(stmt.span.end);
  }
  for expr in body.exprs.iter() {
    start = start.min(expr.span.start);
    end = end.max(expr.span.end);
  }
  for pat in body.pats.iter() {
    if pat.span.start == 0 && pat.span.end == 0 {
      continue;
    }
    start = start.min(pat.span.start);
    end = end.max(pat.span.end);
  }
  if start == u32::MAX {
    match body.kind {
      BodyKind::Class => TextRange::new(0, 0),
      _ => body.span,
    }
  } else {
    TextRange::new(start, end)
  }
}

fn loc_to_range(_file: FileId, loc: Loc) -> TextRange {
  let (range, _) = loc.to_diagnostics_range_with_note();
  TextRange::new(range.start, range.end)
}

fn parse_canonical_index_str(s: &str) -> Option<i64> {
  if s == "0" {
    return Some(0);
  }
  let bytes = s.as_bytes();
  let first = *bytes.first()?;
  if first == b'0' {
    return None;
  }
  if bytes.iter().all(|c| c.is_ascii_digit()) {
    s.parse().ok()
  } else {
    None
  }
}

fn substitute_this_type(store: &Arc<TypeStore>, ty: TypeId, receiver: TypeId) -> TypeId {
  let receiver = store.canon(receiver);
  let mut substituter =
    Substituter::new_with_this(Arc::clone(store), HashMap::new(), Some(receiver));
  substituter.substitute_type(ty)
}

fn fixed_spread_len(
  store: &TypeStore,
  ty: TypeId,
  expander: Option<&dyn types_ts_interned::RelateTypeExpander>,
  seen: &mut HashSet<TypeId>,
) -> Option<usize> {
  if !seen.insert(ty) {
    return None;
  }
  match store.type_kind(ty) {
    TypeKind::Tuple(elems) => {
      if elems.iter().any(|elem| elem.optional || elem.rest) {
        None
      } else {
        Some(elems.len())
      }
    }
    TypeKind::Ref { def, args } => expander
      .and_then(|expander| expander.expand_ref(store, def, &args))
      .and_then(|expanded| fixed_spread_len(store, expanded, expander, seen)),
    _ => None,
  }
}

fn awaited_type(
  store: &TypeStore,
  ty: TypeId,
  ref_expander: Option<&dyn types_ts_interned::RelateTypeExpander>,
) -> TypeId {
  struct AwaitedTypeCalc<'a> {
    store: &'a TypeStore,
    ref_expander: Option<&'a dyn types_ts_interned::RelateTypeExpander>,
    awaited_stack: HashSet<TypeId>,
    thenable_stack: HashSet<TypeId>,
    then_name: types_ts_interned::NameId,
  }

  impl<'a> AwaitedTypeCalc<'a> {
    const MAX_DEPTH: usize = 32;

    fn awaited(&mut self, ty: TypeId, depth: usize) -> TypeId {
      let ty = self.store.canon(ty);
      if depth > Self::MAX_DEPTH {
        return ty;
      }
      if !self.awaited_stack.insert(ty) {
        return ty;
      }
      let prim = self.store.primitive_ids();
      let out = match self.store.type_kind(ty) {
        TypeKind::Any | TypeKind::Unknown | TypeKind::Never => ty,
        TypeKind::Union(members) => {
          let mapped: Vec<_> = members
            .iter()
            .copied()
            .map(|member| self.awaited(member, depth + 1))
            .collect();
          self.store.union(mapped)
        }
        _ => match self.thenable_resolved(ty, depth + 1) {
          Some(resolved) => self.awaited(resolved, depth + 1),
          None => ty,
        },
      };
      self.awaited_stack.remove(&ty);
      if out == prim.never {
        prim.never
      } else {
        out
      }
    }

    fn thenable_resolved(&mut self, ty: TypeId, depth: usize) -> Option<TypeId> {
      let ty = self.store.canon(ty);
      if depth > Self::MAX_DEPTH {
        return None;
      }
      if !self.thenable_stack.insert(ty) {
        return None;
      }
      let out = match self.store.type_kind(ty) {
        TypeKind::Ref { def, args } => self
          .ref_expander
          .and_then(|expander| expander.expand_ref(self.store, def, &args))
          .and_then(|expanded| self.thenable_resolved(expanded, depth + 1)),
        TypeKind::Object(obj) => self.thenable_from_object(obj, depth + 1),
        TypeKind::Intersection(members) => {
          let mut resolved = Vec::new();
          for member in members.iter().copied() {
            if let Some(inner) = self.thenable_resolved(member, depth + 1) {
              resolved.push(inner);
            }
          }
          match resolved.len() {
            0 => None,
            1 => resolved.into_iter().next(),
            _ => Some(self.store.intersection(resolved)),
          }
        }
        _ => None,
      };
      self.thenable_stack.remove(&ty);
      out
    }

    fn thenable_from_object(
      &mut self,
      obj: types_ts_interned::ObjectId,
      depth: usize,
    ) -> Option<TypeId> {
      let shape = self.store.shape(self.store.object(obj).shape);
      let then_prop = shape.properties.iter().find(|prop| match prop.key {
        PropKey::String(name) => name == self.then_name,
        _ => false,
      })?;
      let then_ty = then_prop.data.ty;
      let mut then_sigs = Vec::new();
      self.collect_call_signatures(then_ty, &mut then_sigs, &mut HashSet::new(), depth + 1);
      if then_sigs.is_empty() {
        return None;
      }
      then_sigs.sort();
      then_sigs.dedup();

      let prim = self.store.primitive_ids();
      let mut resolved = Vec::new();
      for sig_id in then_sigs {
        let sig = self.store.signature(sig_id);
        let Some(onfulfilled) = sig.params.first() else {
          continue;
        };
        let mut cb_sigs = Vec::new();
        self.collect_call_signatures(onfulfilled.ty, &mut cb_sigs, &mut HashSet::new(), depth + 1);
        if cb_sigs.is_empty() {
          continue;
        }
        cb_sigs.sort();
        cb_sigs.dedup();
        let mut cb_values = Vec::new();
        for cb_sig_id in cb_sigs {
          let cb_sig = self.store.signature(cb_sig_id);
          if let Some(value) = cb_sig.params.first() {
            cb_values.push(value.ty);
          }
        }
        let value_ty = match cb_values.len() {
          0 => prim.unknown,
          1 => cb_values[0],
          _ => self.store.union(cb_values),
        };
        resolved.push(value_ty);
      }
      match resolved.len() {
        0 => None,
        1 => resolved.into_iter().next(),
        _ => Some(self.store.union(resolved)),
      }
    }

    fn collect_call_signatures(
      &self,
      ty: TypeId,
      out: &mut Vec<types_ts_interned::SignatureId>,
      seen: &mut HashSet<TypeId>,
      depth: usize,
    ) {
      let ty = self.store.canon(ty);
      if depth > Self::MAX_DEPTH {
        return;
      }
      if !seen.insert(ty) {
        return;
      }
      match self.store.type_kind(ty) {
        TypeKind::Callable { overloads } => {
          out.extend(overloads.iter().copied());
        }
        TypeKind::Object(obj) => {
          let shape = self.store.shape(self.store.object(obj).shape);
          out.extend(shape.call_signatures.iter().copied());
        }
        TypeKind::Union(members) | TypeKind::Intersection(members) => {
          for member in members.iter().copied() {
            self.collect_call_signatures(member, out, seen, depth + 1);
          }
        }
        TypeKind::Ref { def, args } => {
          if let Some(expander) = self.ref_expander {
            if let Some(expanded) = expander.expand_ref(self.store, def, &args) {
              self.collect_call_signatures(expanded, out, seen, depth + 1);
            }
          }
        }
        _ => {}
      }
    }
  }

  let then_name = store.intern_name_ref("then");
  let mut calc = AwaitedTypeCalc {
    store,
    ref_expander,
    awaited_stack: HashSet::new(),
    thenable_stack: HashSet::new(),
    then_name,
  };
  calc.awaited(ty, 0)
}

/// Flow-sensitive body checker built directly on `hir-js` bodies. This is a
/// lightweight, statement-level analysis that uses a CFG plus a simple lattice
/// of variable environments to drive narrowing.
pub fn check_body_with_env(
  body_id: BodyId,
  body: &Body,
  names: &NameInterner,
  file: FileId,
  _source: &str,
  store: Arc<TypeStore>,
  initial: &HashMap<NameId, TypeId>,
  relate: RelateCtx,
  ref_expander: Option<&dyn types_ts_interned::RelateTypeExpander>,
  this_ty: TypeId,
  super_ty: TypeId,
) -> BodyCheckResult {
  check_body_with_env_with_bindings(
    body_id,
    body,
    names,
    file,
    _source,
    store,
    initial,
    None,
    relate,
    ref_expander,
    this_ty,
    super_ty,
  )
}

pub fn check_body_with_env_with_bindings(
  body_id: BodyId,
  body: &Body,
  names: &NameInterner,
  file: FileId,
  _source: &str,
  store: Arc<TypeStore>,
  initial: &HashMap<NameId, TypeId>,
  flow_bindings: Option<&FlowBindings>,
  relate: RelateCtx,
  ref_expander: Option<&dyn types_ts_interned::RelateTypeExpander>,
  this_ty: TypeId,
  super_ty: TypeId,
) -> BodyCheckResult {
  check_body_with_env_with_bindings_strict_native(
    body_id,
    body,
    names,
    file,
    _source,
    store,
    initial,
    flow_bindings,
    relate,
    ref_expander,
    this_ty,
    super_ty,
    false,
  )
}

pub fn check_body_with_env_with_bindings_strict_native(
  body_id: BodyId,
  body: &Body,
  names: &NameInterner,
  file: FileId,
  _source: &str,
  store: Arc<TypeStore>,
  initial: &HashMap<NameId, TypeId>,
  flow_bindings: Option<&FlowBindings>,
  relate: RelateCtx,
  ref_expander: Option<&dyn types_ts_interned::RelateTypeExpander>,
  this_ty: TypeId,
  super_ty: TypeId,
  strict_native: bool,
) -> BodyCheckResult {
  let canon = |ty: TypeId| store.contains_type_id(ty).then_some(store.canon(ty));
  let this_super_context = BodyThisSuperContext {
    this_ty: canon(this_ty),
    super_ty: canon(super_ty),
    super_instance_ty: canon(super_ty),
    super_value_ty: None,
  };
  let expr_def_types = HashMap::new();
  let mut checker = FlowBodyChecker::new(
    body_id,
    body,
    names,
    Arc::clone(&store),
    None,
    file,
    this_super_context,
    initial,
    &expr_def_types,
    flow_bindings,
    relate,
    ref_expander,
    this_ty,
    strict_native,
    false,
  );
  checker.run();
  codes::normalize_diagnostics(&mut checker.diagnostics);
  checker.into_result()
}

pub(crate) struct FlowBodyCheckTables {
  pub(crate) expr_types: Vec<TypeId>,
  pub(crate) pat_types: Vec<TypeId>,
  pub(crate) return_types: Vec<TypeId>,
  pub(crate) diagnostics: Vec<Diagnostic>,
  /// Call/construct expressions evaluated by the flow checker, along with the
  /// final signature selection state.
  ///
  /// Expressions not present in this list were not evaluated by the flow checker
  /// (e.g. unreachable blocks) and should not overwrite the base checker output.
  pub(crate) call_signatures: Vec<(ExprId, CallSignatureState)>,
}

pub(crate) fn check_body_with_env_tables_with_bindings(
  body_id: BodyId,
  body: &Body,
  names: &NameInterner,
  file: FileId,
  _source: &str,
  store: Arc<TypeStore>,
  initial: &HashMap<NameId, TypeId>,
  type_resolver: Option<Arc<dyn TypeResolver>>,
  expr_def_types: &HashMap<DefId, TypeId>,
  flow_bindings: Option<&FlowBindings>,
  relate: RelateCtx,
  ref_expander: Option<&dyn types_ts_interned::RelateTypeExpander>,
  this_super_context: BodyThisSuperContext,
  this_ty: TypeId,
  strict_native: bool,
  is_derived_constructor: bool,
) -> FlowBodyCheckTables {
  let mut checker = FlowBodyChecker::new(
    body_id,
    body,
    names,
    Arc::clone(&store),
    type_resolver,
    file,
    this_super_context,
    initial,
    expr_def_types,
    flow_bindings,
    relate,
    ref_expander,
    this_ty,
    strict_native,
    is_derived_constructor,
  );
  checker.run();
  codes::normalize_diagnostics(&mut checker.diagnostics);
  checker.into_tables()
}

enum Reference {
  Ident {
    name: FlowBindingId,
    ty: TypeId,
  },
  Member {
    base: FlowBindingId,
    prop: String,
    base_ty: TypeId,
    prop_ty: TypeId,
  },
}

impl Reference {
  fn target(&self) -> FlowBindingId {
    match self {
      Reference::Ident { name, .. } => *name,
      Reference::Member { base, .. } => *base,
    }
  }

  fn target_ty(&self) -> TypeId {
    match self {
      Reference::Ident { ty, .. } => *ty,
      Reference::Member { base_ty, .. } => *base_ty,
    }
  }

  fn value_ty(&self) -> TypeId {
    match self {
      Reference::Ident { ty, .. } => *ty,
      Reference::Member { prop_ty, .. } => *prop_ty,
    }
  }
}

struct FlowBodyChecker<'a> {
  body_id: BodyId,
  body: &'a Body,
  names: &'a NameInterner,
  store: Arc<TypeStore>,
  type_resolver: Option<Arc<dyn TypeResolver>>,
  promise_def: Option<DefId>,
  promise_any: TypeId,
  file: FileId,
  this_super_context: BodyThisSuperContext,
  this_ty: TypeId,
  super_ty: TypeId,
  relate: RelateCtx<'a>,
  instantiation_cache: InstantiationCache,
  expr_types: Vec<TypeId>,
  call_signatures: HashMap<ExprId, CallSignatureState>,
  optional_chain_exec_types: Vec<Option<TypeId>>,
  pat_types: Vec<TypeId>,
  expr_spans: Vec<TextRange>,
  pat_spans: Vec<TextRange>,
  diagnostics: Vec<Diagnostic>,
  reported_unassigned: HashSet<ExprId>,
  return_types: Vec<TypeId>,
  return_indices: HashMap<StmtId, usize>,
  widen_object_literals: bool,
  is_derived_constructor: bool,
  ref_expander: Option<&'a dyn types_ts_interned::RelateTypeExpander>,
  expr_def_types: &'a HashMap<DefId, TypeId>,
  initial: HashMap<FlowBindingId, TypeId>,
  param_bindings: HashSet<BindingKey>,
  bindings: BindingTable,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CallSignatureState {
  /// No signature has been recorded yet.
  Unresolved,
  /// A single signature was selected for all evaluated paths so far.
  Resolved(SignatureId),
  /// Multiple distinct signatures were observed; treat the call-site signature
  /// as unknown and do not record a specific overload.
  Conflict,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingMode {
  Declare,
  Assign,
}

enum SwitchDiscriminant {
  Ident {
    name: FlowBindingId,
    ty: TypeId,
  },
  Member {
    name: FlowBindingId,
    path: Vec<PathSegment>,
    optional_bases: Vec<Vec<PathSegment>>,
    ty: TypeId,
  },
  Typeof {
    name: FlowBindingId,
    ty: TypeId,
  },
}

impl SwitchDiscriminant {
  fn ty(&self) -> TypeId {
    match self {
      SwitchDiscriminant::Ident { ty, .. }
      | SwitchDiscriminant::Member { ty, .. }
      | SwitchDiscriminant::Typeof { ty, .. } => *ty,
    }
  }

  fn name(&self) -> FlowBindingId {
    match self {
      SwitchDiscriminant::Ident { name, .. }
      | SwitchDiscriminant::Member { name, .. }
      | SwitchDiscriminant::Typeof { name, .. } => *name,
    }
  }
}

#[derive(Default)]
struct BindingTable {
  expr_bindings: HashMap<ExprId, BindingKey>,
  pat_bindings: HashMap<PatId, BindingKey>,
  param_bindings: HashSet<BindingKey>,
  var_bindings: HashSet<BindingKey>,
  flow_ids: HashMap<BindingKey, FlowBindingId>,
  flow_to_binding: HashMap<FlowBindingId, BindingKey>,
  next_flow_id: u64,
}

impl BindingTable {
  fn binding_key_for_expr(&self, expr: ExprId) -> Option<BindingKey> {
    self.expr_bindings.get(&expr).copied()
  }

  fn binding_key_for_pat(&self, pat: PatId) -> Option<BindingKey> {
    self.pat_bindings.get(&pat).copied()
  }

  fn binding_for_expr(&self, expr: ExprId) -> Option<FlowBindingId> {
    self.flow_binding_for_expr(expr)
  }

  fn binding_for_pat(&self, pat: PatId) -> Option<FlowBindingId> {
    self.flow_binding_for_pat(pat)
  }

  fn set_flow_binding(&mut self, binding: BindingKey, id: FlowBindingId) -> FlowBindingId {
    if let Some(existing) = self.flow_ids.get(&binding) {
      if *existing == id {
        return id;
      }
      self.flow_to_binding.remove(existing);
    }
    if let Some(previous_binding) = self.flow_to_binding.insert(id, binding) {
      if previous_binding != binding {
        self.flow_ids.remove(&previous_binding);
      }
    }
    self.flow_ids.insert(binding, id);
    id
  }

  fn ensure_flow_binding(&mut self, binding: BindingKey) -> FlowBindingId {
    if let Some(existing) = self.flow_ids.get(&binding) {
      return *existing;
    }
    let mut id = SymbolId(self.next_flow_id);
    self.next_flow_id += 1;
    while self.flow_to_binding.contains_key(&id) {
      id = SymbolId(self.next_flow_id);
      self.next_flow_id += 1;
    }
    self.set_flow_binding(binding, id)
  }

  fn flow_binding_for_key(&self, binding: BindingKey) -> Option<FlowBindingId> {
    self.flow_ids.get(&binding).copied()
  }

  fn flow_binding_for_expr(&self, expr: ExprId) -> Option<FlowBindingId> {
    self
      .expr_bindings
      .get(&expr)
      .and_then(|b| self.flow_ids.get(b))
      .copied()
  }

  fn flow_binding_for_pat(&self, pat: PatId) -> Option<FlowBindingId> {
    self
      .pat_bindings
      .get(&pat)
      .and_then(|b| self.flow_ids.get(b))
      .copied()
  }

  fn binding_for_flow(&self, id: FlowBindingId) -> Option<BindingKey> {
    self.flow_to_binding.get(&id).copied()
  }

  fn flow_binding_for_external(&mut self, name: NameId) -> FlowBindingId {
    self.ensure_flow_binding(BindingKey::External(name))
  }
}

struct BindingCollector<'a> {
  body: &'a Body,
  scopes: Vec<HashMap<NameId, BindingKey>>,
  table: BindingTable,
  visited_stmts: HashSet<StmtId>,
  flow_bindings: Option<&'a FlowBindings>,
}

impl<'a> BindingCollector<'a> {
  fn collect(body: &'a Body, flow_bindings: Option<&'a FlowBindings>) -> BindingTable {
    let mut collector = BindingCollector {
      body,
      scopes: vec![HashMap::new()],
      table: BindingTable::default(),
      visited_stmts: HashSet::new(),
      flow_bindings,
    };
    collector.collect_params();
    let roots = if !body.root_stmts.is_empty() {
      body.root_stmts.clone()
    } else {
      (0..body.stmts.len() as u32).map(StmtId).collect()
    };
    for stmt in roots {
      collector.visit_stmt(stmt);
    }
    collector.table
  }

  fn collect_params(&mut self) {
    if let Some(function) = self.body.function.as_ref() {
      for param in function.params.iter() {
        self.declare_pat(param.pat, true, false);
        if let Some(default) = param.default {
          self.visit_expr(default);
        }
      }
    }
  }

  fn insert_binding(
    &mut self,
    name: NameId,
    pat: PatId,
    is_param: bool,
    hoist: bool,
    flow_binding: Option<FlowBindingId>,
  ) {
    let target_scope = if hoist {
      self
        .scopes
        .first_mut()
        .expect("binding collector always has a root scope")
    } else {
      self
        .scopes
        .last_mut()
        .expect("binding collector always has a scope")
    };
    // Hoisted `var` declarations share the function-scoped binding with
    // parameters and other `var`s. Reuse the existing binding if present so
    // flow facts accumulate on the same symbol.
    if let Some(existing) = target_scope.get(&name).copied() {
      self.table.pat_bindings.insert(pat, existing);
      if is_param {
        self.table.param_bindings.insert(existing);
      }
      if hoist {
        self.table.var_bindings.insert(existing);
      }
      if let Some(id) = flow_binding {
        self.table.set_flow_binding(existing, id);
      } else {
        self.table.ensure_flow_binding(existing);
      }
      return;
    }

    let key = BindingKey::Local { pat, name };
    self.table.pat_bindings.insert(pat, key);
    if is_param {
      self.table.param_bindings.insert(key);
    }
    if hoist {
      self.table.var_bindings.insert(key);
    }
    if let Some(id) = flow_binding {
      self.table.set_flow_binding(key, id);
    } else {
      self.table.ensure_flow_binding(key);
    }
    target_scope.insert(name, key);
  }

  fn declare_pat(&mut self, pat_id: PatId, is_param: bool, hoist: bool) {
    let pat = &self.body.pats[pat_id.0 as usize];
    match &pat.kind {
      PatKind::Ident(name) => self.insert_binding(
        *name,
        pat_id,
        is_param,
        hoist,
        self
          .flow_bindings
          .and_then(|bindings| bindings.binding_for_pat(pat_id)),
      ),
      PatKind::Assign {
        target,
        default_value,
      } => {
        self.declare_pat(*target, is_param, hoist);
        self.visit_expr(*default_value);
      }
      PatKind::Rest(inner) => self.declare_pat(**inner, is_param, hoist),
      PatKind::Array(arr) => {
        for elem in arr.elements.iter().flatten() {
          self.declare_pat(elem.pat, is_param, hoist);
          if let Some(default) = elem.default_value {
            self.visit_expr(default);
          }
        }
        if let Some(rest) = arr.rest {
          self.declare_pat(rest, is_param, hoist);
        }
      }
      PatKind::Object(obj) => {
        for prop in obj.props.iter() {
          self.declare_pat(prop.value, is_param, hoist);
          if let Some(default) = prop.default_value {
            self.visit_expr(default);
          }
          if let ObjectKey::Computed(expr) = &prop.key {
            self.visit_expr(*expr);
          }
        }
        if let Some(rest) = obj.rest {
          self.declare_pat(rest, is_param, hoist);
        }
      }
      PatKind::AssignTarget(expr) => self.visit_expr(*expr),
    }
  }

  fn resolve_binding(&mut self, name: NameId) -> BindingKey {
    for scope in self.scopes.iter().rev() {
      if let Some(binding) = scope.get(&name) {
        let binding = *binding;
        self.table.ensure_flow_binding(binding);
        return binding;
      }
    }
    let binding = BindingKey::External(name);
    self.table.ensure_flow_binding(binding);
    binding
  }

  fn visit_stmt(&mut self, stmt_id: StmtId) {
    if !self.visited_stmts.insert(stmt_id) {
      return;
    }
    let stmt = &self.body.stmts[stmt_id.0 as usize];
    match &stmt.kind {
      StmtKind::Expr(expr) => self.visit_expr(*expr),
      StmtKind::ExportDefaultExpr(expr) => self.visit_expr(*expr),
      StmtKind::Decl(_) => {}
      StmtKind::Return(expr) => {
        if let Some(expr) = expr {
          self.visit_expr(*expr);
        }
      }
      StmtKind::Block(stmts) => {
        self.push_scope();
        for stmt in stmts.iter() {
          self.visit_stmt(*stmt);
        }
        self.pop_scope();
      }
      StmtKind::If {
        test,
        consequent,
        alternate,
      } => {
        self.visit_expr(*test);
        self.push_scope();
        self.visit_stmt(*consequent);
        self.pop_scope();
        if let Some(alt) = alternate {
          self.push_scope();
          self.visit_stmt(*alt);
          self.pop_scope();
        }
      }
      StmtKind::While { test, body } => {
        self.visit_expr(*test);
        self.push_scope();
        self.visit_stmt(*body);
        self.pop_scope();
      }
      StmtKind::DoWhile { test, body } => {
        self.push_scope();
        self.visit_stmt(*body);
        self.pop_scope();
        self.visit_expr(*test);
      }
      StmtKind::For {
        init,
        test,
        update,
        body,
      } => {
        self.push_scope();
        if let Some(init) = init {
          match init {
            ForInit::Expr(expr) => self.visit_expr(*expr),
            ForInit::Var(var) => self.visit_var_decl(var),
          }
        }
        if let Some(test) = test {
          self.visit_expr(*test);
        }
        if let Some(update) = update {
          self.visit_expr(*update);
        }
        self.visit_stmt(*body);
        self.pop_scope();
      }
      StmtKind::ForIn {
        left, right, body, ..
      } => {
        self.push_scope();
        match left {
          ForHead::Pat(pat) => self.declare_pat(*pat, false, false),
          ForHead::Var(var) => self.visit_var_decl(var),
        }
        self.visit_expr(*right);
        self.visit_stmt(*body);
        self.pop_scope();
      }
      StmtKind::Switch {
        discriminant,
        cases,
        ..
      } => {
        self.visit_expr(*discriminant);
        self.push_scope();
        for case in cases.iter() {
          if let Some(test) = case.test {
            self.visit_expr(test);
          }
          for stmt in case.consequent.iter() {
            self.visit_stmt(*stmt);
          }
        }
        self.pop_scope();
      }
      StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        self.push_scope();
        self.visit_stmt(*block);
        self.pop_scope();
        if let Some(catch) = catch {
          self.push_scope();
          if let Some(param) = catch.param {
            self.declare_pat(param, false, false);
          }
          self.visit_stmt(catch.body);
          self.pop_scope();
        }
        if let Some(finally_block) = finally_block {
          self.push_scope();
          self.visit_stmt(*finally_block);
          self.pop_scope();
        }
      }
      StmtKind::Throw(expr) => self.visit_expr(*expr),
      StmtKind::Break(_) | StmtKind::Continue(_) | StmtKind::Debugger | StmtKind::Empty => {}
      StmtKind::Var(decl) => self.visit_var_decl(decl),
      StmtKind::Labeled { body, .. } => self.visit_stmt(*body),
      StmtKind::With { object, body } => {
        self.visit_expr(*object);
        self.push_scope();
        self.visit_stmt(*body);
        self.pop_scope();
      }
    }
  }

  fn visit_var_decl(&mut self, decl: &HirVarDecl) {
    let hoist = matches!(decl.kind, hir_js::VarDeclKind::Var);
    for declarator in decl.declarators.iter() {
      self.declare_pat(declarator.pat, false, hoist);
      if let Some(init) = declarator.init {
        self.visit_expr(init);
      }
    }
  }

  fn visit_expr(&mut self, expr_id: ExprId) {
    let expr = &self.body.exprs[expr_id.0 as usize];
    match &expr.kind {
      ExprKind::Ident(name) => {
        let binding = self.resolve_binding(*name);
        self.table.expr_bindings.insert(expr_id, binding);
        if let Some(id) = self
          .flow_bindings
          .and_then(|bindings| bindings.binding_for_expr(expr_id))
        {
          self.table.set_flow_binding(binding, id);
        }
      }
      ExprKind::Unary { expr, .. } => self.visit_expr(*expr),
      ExprKind::Update { expr, .. } => self.visit_expr(*expr),
      ExprKind::Binary { left, right, .. } => {
        self.visit_expr(*left);
        self.visit_expr(*right);
      }
      ExprKind::Assignment { target, value, .. } => {
        self.visit_pat(*target);
        self.visit_expr(*value);
      }
      ExprKind::Call(call) => {
        self.visit_expr(call.callee);
        for arg in call.args.iter() {
          self.visit_expr(arg.expr);
        }
      }
      ExprKind::Member(mem) => {
        self.visit_expr(mem.object);
        if let ObjectKey::Computed(expr) = &mem.property {
          self.visit_expr(*expr);
        }
      }
      ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => {
        self.visit_expr(*test);
        self.visit_expr(*consequent);
        self.visit_expr(*alternate);
      }
      ExprKind::Array(arr) => {
        for elem in arr.elements.iter() {
          match elem {
            ArrayElement::Expr(expr) | ArrayElement::Spread(expr) => self.visit_expr(*expr),
            ArrayElement::Empty => {}
          }
        }
      }
      ExprKind::Object(obj) => {
        for prop in obj.properties.iter() {
          match prop {
            ObjectProperty::KeyValue { key, value, .. } => {
              self.visit_expr(*value);
              if let ObjectKey::Computed(expr) = key {
                self.visit_expr(*expr);
              }
            }
            ObjectProperty::Getter { body, key } | ObjectProperty::Setter { body, key } => {
              if let ObjectKey::Computed(expr) = key {
                self.visit_expr(*expr);
              }
              self.visit_body(*body);
            }
            ObjectProperty::Spread(expr) => self.visit_expr(*expr),
          }
        }
      }
      ExprKind::Template(template) => {
        for span in template.spans.iter() {
          self.visit_expr(span.expr);
        }
      }
      ExprKind::TaggedTemplate { tag, template } => {
        self.visit_expr(*tag);
        for span in template.spans.iter() {
          self.visit_expr(span.expr);
        }
      }
      ExprKind::Await { expr } => self.visit_expr(*expr),
      #[cfg(feature = "semantic-ops")]
      ExprKind::AwaitExpr { value: expr, .. } => self.visit_expr(*expr),
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayMap { array, callback }
      | ExprKind::ArrayFilter { array, callback }
      | ExprKind::ArrayFind { array, callback }
      | ExprKind::ArrayEvery { array, callback }
      | ExprKind::ArraySome { array, callback } => {
        self.visit_expr(*array);
        self.visit_expr(*callback);
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayReduce {
        array,
        callback,
        init,
      } => {
        self.visit_expr(*array);
        self.visit_expr(*callback);
        if let Some(init) = init {
          self.visit_expr(*init);
        }
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayChain { array, ops } => {
        self.visit_expr(*array);
        for op in ops {
          match op {
            hir_js::ArrayChainOp::Map(callback)
            | hir_js::ArrayChainOp::Filter(callback)
            | hir_js::ArrayChainOp::Find(callback)
            | hir_js::ArrayChainOp::Every(callback)
            | hir_js::ArrayChainOp::Some(callback) => self.visit_expr(*callback),
            hir_js::ArrayChainOp::Reduce(callback, init) => {
              self.visit_expr(*callback);
              if let Some(init) = init {
                self.visit_expr(*init);
              }
            }
          }
        }
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
        for promise in promises {
          self.visit_expr(*promise);
        }
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::KnownApiCall { args, .. } => {
        for arg in args {
          self.visit_expr(*arg);
        }
      }
      ExprKind::Yield { expr, .. } => {
        if let Some(expr) = expr {
          self.visit_expr(*expr);
        }
      }
      ExprKind::Instantiation { expr, .. }
      | ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Satisfies { expr, .. } => self.visit_expr(*expr),
      ExprKind::ImportCall {
        argument,
        attributes,
      } => {
        self.visit_expr(*argument);
        if let Some(attrs) = attributes {
          self.visit_expr(*attrs);
        }
      }
      ExprKind::Jsx(elem) => {
        for attr in elem.attributes.iter() {
          match attr {
            hir_js::JsxAttr::Named { value, .. } => {
              if let Some(value) = value {
                match value {
                  hir_js::JsxAttrValue::Expression(container) => {
                    if let Some(expr) = container.expr {
                      self.visit_expr(expr);
                    }
                  }
                  hir_js::JsxAttrValue::Element(expr) => {
                    self.visit_expr(*expr);
                  }
                  hir_js::JsxAttrValue::Text(_) => {}
                }
              }
            }
            hir_js::JsxAttr::Spread { expr } => self.visit_expr(*expr),
          }
        }
        for child in elem.children.iter() {
          match child {
            hir_js::JsxChild::Text(_) => {}
            hir_js::JsxChild::Expr(container) => {
              if let Some(expr) = container.expr {
                self.visit_expr(expr);
              }
            }
            hir_js::JsxChild::Element(expr) => self.visit_expr(*expr),
          }
        }
      }
      ExprKind::Literal(_)
      | ExprKind::Missing
      | ExprKind::This
      | ExprKind::Super
      | ExprKind::FunctionExpr { .. }
      | ExprKind::ClassExpr { .. }
      | ExprKind::ImportMeta
      | ExprKind::NewTarget => {}
    }
  }

  fn visit_body(&mut self, _body_id: BodyId) {
    // Nested bodies are checked separately; nothing to do here.
  }

  fn visit_pat(&mut self, pat_id: PatId) {
    let pat = &self.body.pats[pat_id.0 as usize];
    match &pat.kind {
      PatKind::Ident(name) => {
        let binding = self.resolve_binding(*name);
        self.table.pat_bindings.entry(pat_id).or_insert(binding);
      }
      PatKind::Assign {
        target,
        default_value,
      } => {
        self.visit_pat(*target);
        self.visit_expr(*default_value);
      }
      PatKind::Rest(inner) => self.visit_pat(**inner),
      PatKind::Array(arr) => {
        for elem in arr.elements.iter().flatten() {
          self.visit_pat(elem.pat);
          if let Some(default) = elem.default_value {
            self.visit_expr(default);
          }
        }
        if let Some(rest) = arr.rest {
          self.visit_pat(rest);
        }
      }
      PatKind::Object(obj) => {
        for prop in obj.props.iter() {
          self.visit_pat(prop.value);
          if let Some(default) = prop.default_value {
            self.visit_expr(default);
          }
          if let ObjectKey::Computed(expr) = &prop.key {
            self.visit_expr(*expr);
          }
        }
        if let Some(rest) = obj.rest {
          self.visit_pat(rest);
        }
      }
      PatKind::AssignTarget(expr) => self.visit_expr(*expr),
    }
  }

  fn push_scope(&mut self) {
    self.scopes.push(HashMap::new());
  }

  fn pop_scope(&mut self) {
    self.scopes.pop();
  }
}

impl<'a> FlowBodyChecker<'a> {
  fn new(
    body_id: BodyId,
    body: &'a Body,
    names: &'a NameInterner,
    store: Arc<TypeStore>,
    type_resolver: Option<Arc<dyn TypeResolver>>,
    file: FileId,
    this_super_context: BodyThisSuperContext,
    initial: &HashMap<NameId, TypeId>,
    expr_def_types: &'a HashMap<DefId, TypeId>,
    flow_bindings: Option<&'a FlowBindings>,
    relate: RelateCtx<'a>,
    ref_expander: Option<&'a dyn types_ts_interned::RelateTypeExpander>,
    fallback_this_ty: TypeId,
    _strict_native: bool,
    is_derived_constructor: bool,
  ) -> Self {
    let prim = store.primitive_ids();
    let mut this_ty = this_super_context.this_ty.unwrap_or(fallback_this_ty);
    if let Some(function) = body.function.as_ref() {
      if let Some(first_param) = function.params.first() {
        if let Some(pat) = body.pats.get(first_param.pat.0 as usize) {
          if let PatKind::Ident(name_id) = pat.kind {
            if names.resolve(name_id) == Some("this") && first_param.type_annotation.is_some() {
              // Explicit `this` parameters override contextual/implicit `this` types.
              this_ty = initial.get(&name_id).copied().unwrap_or(prim.unknown);
            }
          }
        }
      }
    }
    let super_ty = this_super_context
      .super_ty
      .or(this_super_context.super_instance_ty)
      .or(this_super_context.super_value_ty)
      .unwrap_or(prim.unknown);
    let expr_types = vec![prim.unknown; body.exprs.len()];
    let call_signatures = HashMap::new();
    let optional_chain_exec_types = vec![None; body.exprs.len()];
    let mut bindings = BindingCollector::collect(body, flow_bindings);
    let mut pat_types = vec![prim.unknown; body.pats.len()];
    for binding in bindings.param_bindings.iter() {
      let BindingKey::Local { pat, name } = *binding else {
        continue;
      };
      if let Some(ty) = initial.get(&name) {
        if let Some(slot) = pat_types.get_mut(pat.0 as usize) {
          *slot = *ty;
        }
      }
    }
    let mut initial_flow = HashMap::new();
    for (name, ty) in initial.iter() {
      let id = bindings
        .param_bindings
        .iter()
        .find(|b| matches!(b, BindingKey::Local { name: n, .. } if *n == *name))
        .and_then(|b| bindings.flow_binding_for_key(*b))
        .unwrap_or_else(|| bindings.flow_binding_for_external(*name));
      initial_flow.insert(id, *ty);
    }

    let mut returns: Vec<(StmtId, u32)> = body
      .stmts
      .iter()
      .enumerate()
      .filter_map(|(idx, stmt)| {
        if matches!(stmt.kind, StmtKind::Return(_)) {
          Some((StmtId(idx as u32), stmt.span.start))
        } else {
          None
        }
      })
      .collect();
    returns.sort_by_key(|(_, start)| *start);
    let mut return_indices = HashMap::new();
    let mut return_types = Vec::new();
    for (idx, (stmt_id, _)) in returns.into_iter().enumerate() {
      return_indices.insert(stmt_id, idx);
      return_types.push(prim.unknown);
    }

    let expr_spans: Vec<TextRange> = body.exprs.iter().map(|e| e.span).collect();
    let pat_spans: Vec<TextRange> = body.pats.iter().map(|p| p.span).collect();
    let this_ty = if store.contains_type_id(this_ty) {
      store.canon(this_ty)
    } else {
      prim.unknown
    };
    let super_ty = if store.contains_type_id(super_ty) {
      store.canon(super_ty)
    } else {
      prim.unknown
    };
    let this_super_context = {
      let canon = |ty: TypeId| store.contains_type_id(ty).then_some(store.canon(ty));
      BodyThisSuperContext {
        this_ty: this_super_context.this_ty.and_then(canon),
        super_ty: this_super_context.super_ty.and_then(canon),
        super_instance_ty: this_super_context.super_instance_ty.and_then(canon),
        super_value_ty: this_super_context.super_value_ty.and_then(canon),
      }
    };
    let promise_def = type_resolver
      .as_ref()
      .and_then(|resolver| resolver.resolve_type_name(&["Promise".to_string()]));
    let promise_any = promise_def
      .map(|def| {
        store.canon(store.intern_type(TypeKind::Ref {
          def,
          args: vec![prim.any],
        }))
      })
      .unwrap_or(prim.unknown);
    Self {
      body_id,
      body,
      names,
      store,
      type_resolver,
      promise_def,
      promise_any,
      file,
      this_super_context,
      this_ty,
      super_ty,
      relate,
      instantiation_cache: InstantiationCache::default(),
      expr_types,
      call_signatures,
      optional_chain_exec_types,
      pat_types,
      expr_spans,
      pat_spans,
      diagnostics: Vec::new(),
      reported_unassigned: HashSet::new(),
      return_types,
      return_indices,
      widen_object_literals: true,
      is_derived_constructor,
      ref_expander,
      expr_def_types,
      initial: initial_flow,
      param_bindings: bindings.param_bindings.clone(),
      bindings,
    }
  }

  fn into_result(self) -> BodyCheckResult {
    let mut call_signatures = vec![None; self.body.exprs.len()];
    for (expr, state) in self.call_signatures.into_iter() {
      if let CallSignatureState::Resolved(sig) = state {
        if let Some(slot) = call_signatures.get_mut(expr.0 as usize) {
          *slot = Some(sig);
        }
      }
    }
    BodyCheckResult {
      body: self.body_id,
      expr_types: self.expr_types,
      call_signatures,
      pat_types: self.pat_types,
      expr_spans: self.expr_spans,
      pat_spans: self.pat_spans,
      diagnostics: self.diagnostics,
      return_types: self.return_types,
    }
  }

  fn into_tables(self) -> FlowBodyCheckTables {
    FlowBodyCheckTables {
      expr_types: self.expr_types,
      pat_types: self.pat_types,
      return_types: self.return_types,
      diagnostics: self.diagnostics,
      call_signatures: self.call_signatures.into_iter().collect(),
    }
  }

  fn run(&mut self) {
    let cfg = ControlFlowGraph::from_body(self.body);
    let mut in_envs: Vec<Option<Env>> = vec![None; cfg.blocks.len()];
    let mut initial_env: Vec<(FlowBindingId, BindingKey, TypeId)> = Vec::new();
    for (id, ty) in self.initial.iter() {
      if let Some(key) = self.bindings.binding_for_flow(*id) {
        initial_env.push((*id, key, *ty));
      }
    }
    for binding in self.param_bindings.iter() {
      if let Some(id) = self.bindings.flow_binding_for_key(*binding) {
        initial_env.push((id, *binding, self.binding_type(*binding)));
      }
    }
    let prim = self.store.primitive_ids();
    for binding in self.bindings.var_bindings.iter() {
      if self.param_bindings.contains(binding) {
        continue;
      }
      if let Some(id) = self.bindings.flow_binding_for_key(*binding) {
        initial_env.push((id, *binding, prim.unknown));
      }
    }
    let mut entry_env = Env::with_initial(&initial_env);
    if self.is_derived_constructor {
      entry_env.this_init = InitState::Unassigned;
    }
    in_envs[cfg.entry.0] = Some(entry_env);
    let mut worklist: VecDeque<BlockId> = VecDeque::new();
    worklist.push_back(cfg.entry);

    while let Some(block_id) = worklist.pop_front() {
      let env = match in_envs[block_id.0].clone() {
        Some(env) => env,
        None => continue,
      };
      let outgoing = self.process_block(block_id, env, &cfg);
      for (succ, out_env) in outgoing {
        if let Some(existing) = in_envs[succ.0].as_mut() {
          if existing.merge_from(&out_env, &self.store) {
            worklist.push_back(succ);
          }
        } else {
          in_envs[succ.0] = Some(out_env);
          worklist.push_back(succ);
        }
      }
    }
  }

  fn binding_type(&self, binding: BindingKey) -> TypeId {
    let prim = self.store.primitive_ids();
    match binding {
      BindingKey::Local { pat, .. } => self
        .pat_types
        .get(pat.0 as usize)
        .copied()
        .unwrap_or(prim.unknown),
      BindingKey::External(_) => self
        .bindings
        .flow_binding_for_key(binding)
        .and_then(|id| self.initial.get(&id).copied())
        .unwrap_or(prim.unknown),
    }
  }

  fn process_block(
    &mut self,
    block_id: BlockId,
    mut env: Env,
    cfg: &ControlFlowGraph,
  ) -> Vec<(BlockId, Env)> {
    let block = &cfg.blocks[block_id.0];

    match &block.kind {
      BlockKind::ForInit { init } => {
        if let Some(init) = init {
          match init {
            ForInit::Expr(expr_id) => {
              let (_, facts) = self.eval_expr(*expr_id, &mut env);
              env.apply_map(&facts.assertions);
            }
            ForInit::Var(var) => {
              let mode = match var.kind {
                hir_js::VarDeclKind::Const
                | hir_js::VarDeclKind::Using
                | hir_js::VarDeclKind::AwaitUsing => BindingMode::Declare,
                _ => BindingMode::Assign,
              };
              for declarator in var.declarators.iter() {
                let init_ty = declarator
                  .init
                  .map(|id| self.eval_expr(id, &mut env).0)
                  .unwrap_or_else(|| self.store.primitive_ids().unknown);
                self.bind_pat_with_mode(declarator.pat, init_ty, &mut env, mode);
                let state = if declarator.init.is_some()
                  || declarator.definite_assignment
                  || matches!(var.kind, hir_js::VarDeclKind::Const)
                {
                  InitState::Assigned
                } else {
                  InitState::Unassigned
                };
                self.mark_pat_state(declarator.pat, &mut env, state);
              }
            }
          }
        }
        return block
          .successors
          .iter()
          .map(|succ| (*succ, env.clone()))
          .collect();
      }
      BlockKind::ForTest { test } => {
        let facts = test
          .map(|t| self.eval_expr(t, &mut env).1)
          .unwrap_or_default();
        let mut then_env = env.clone();
        then_env.apply_facts(&facts);
        let mut else_env = env.clone();
        else_env.apply_falsy(&facts);

        let mut outgoing = Vec::new();
        if let Some(succ) = block.successors.get(0) {
          outgoing.push((*succ, then_env));
        }
        if let Some(succ) = block.successors.get(1) {
          outgoing.push((*succ, else_env));
        }
        return outgoing;
      }
      BlockKind::ForUpdate { update } => {
        if let Some(expr_id) = update {
          let (_, facts) = self.eval_expr(*expr_id, &mut env);
          env.apply_map(&facts.assertions);
        }
        return block
          .successors
          .iter()
          .map(|succ| (*succ, env.clone()))
          .collect();
      }
      BlockKind::DoWhileTest { test } => {
        let facts = self.eval_expr(*test, &mut env).1;
        let mut body_env = env.clone();
        body_env.apply_facts(&facts);
        let mut after_env = env.clone();
        after_env.apply_falsy(&facts);
        let mut outgoing = Vec::new();
        if let Some(succ) = block.successors.get(0) {
          outgoing.push((*succ, body_env));
        }
        if let Some(succ) = block.successors.get(1) {
          outgoing.push((*succ, after_env));
        }
        return outgoing;
      }
      BlockKind::Normal => {}
    }

    if block.stmts.is_empty() {
      return block
        .successors
        .iter()
        .map(|succ| (*succ, env.clone()))
        .collect();
    }

    let mut outgoing = Vec::new();
    for stmt_id in block.stmts.iter() {
      let stmt = &self.body.stmts[stmt_id.0 as usize];
      match &stmt.kind {
        StmtKind::Expr(expr) => {
          let (_, facts) = self.eval_expr(*expr, &mut env);
          env.apply_map(&facts.assertions);
        }
        StmtKind::ExportDefaultExpr(expr) => {
          let (_, facts) = self.eval_expr(*expr, &mut env);
          env.apply_map(&facts.assertions);
        }
        StmtKind::Return(expr) => {
          let expr_ty = match expr {
            Some(id) => self.eval_expr(*id, &mut env).0,
            None => self.store.primitive_ids().undefined,
          };
          let ty = if self.body.function.as_ref().is_some_and(|f| f.async_) {
            awaited_type(self.store.as_ref(), expr_ty, self.ref_expander)
          } else {
            expr_ty
          };
          self.record_return(*stmt_id, ty);
          // return/throw terminate flow; no successors.
          return Vec::new();
        }
        StmtKind::Throw(expr) => {
          let _ = self.eval_expr(*expr, &mut env);
          return Vec::new();
        }
        StmtKind::Var(decl) => {
          let mode = match decl.kind {
            hir_js::VarDeclKind::Const
            | hir_js::VarDeclKind::Using
            | hir_js::VarDeclKind::AwaitUsing => BindingMode::Declare,
            _ => BindingMode::Assign,
          };
          for declarator in decl.declarators.iter() {
            let init_ty = declarator
              .init
              .map(|id| self.eval_expr(id, &mut env).0)
              .unwrap_or_else(|| self.store.primitive_ids().unknown);
            self.bind_pat_with_mode(declarator.pat, init_ty, &mut env, mode);
            let state = if declarator.init.is_some()
              || declarator.definite_assignment
              || matches!(decl.kind, hir_js::VarDeclKind::Const)
            {
              InitState::Assigned
            } else {
              InitState::Unassigned
            };
            self.mark_pat_state(declarator.pat, &mut env, state);
          }
        }
        StmtKind::If {
          test,
          consequent: _,
          alternate: _,
        } => {
          let facts = self.eval_expr(*test, &mut env).1;
          let mut then_env = env.clone();
          then_env.apply_facts(&facts);
          let mut else_env = env.clone();
          else_env.apply_falsy(&facts);

          if let Some(succ) = block.successors.get(0) {
            outgoing.push((*succ, then_env));
          }
          if let Some(succ) = block.successors.get(1) {
            outgoing.push((*succ, else_env));
          }
          return outgoing;
        }
        StmtKind::While { test, .. } => {
          let facts = self.eval_expr(*test, &mut env).1;
          let mut body_env = env.clone();
          body_env.apply_facts(&facts);
          let mut after_env = env.clone();
          after_env.apply_falsy(&facts);
          if let Some(succ) = block.successors.get(0) {
            outgoing.push((*succ, body_env));
          }
          if let Some(succ) = block.successors.get(1) {
            outgoing.push((*succ, after_env));
          }
          return outgoing;
        }
        StmtKind::DoWhile { .. } => {
          unreachable!("do...while statements are lowered into synthetic blocks");
        }
        StmtKind::For { .. } => {
          unreachable!("for statements are lowered into synthetic blocks");
        }
        StmtKind::ForIn {
          left,
          right,
          is_for_of,
          ..
        } => {
          let iter_ty = self.eval_expr(*right, &mut env).0;
          let right_ty = if *is_for_of {
            self.for_of_element_type(iter_ty)
          } else {
            self.store.primitive_ids().string
          };
          let mut loop_env = env.clone();
          match left {
            ForHead::Pat(pat) => self.assign_pat(*pat, right_ty, &mut loop_env),
            ForHead::Var(var) => {
              let mode = match var.kind {
                hir_js::VarDeclKind::Const
                | hir_js::VarDeclKind::Using
                | hir_js::VarDeclKind::AwaitUsing => BindingMode::Declare,
                _ => BindingMode::Assign,
              };
              for declarator in var.declarators.iter() {
                self.bind_pat_with_mode(declarator.pat, right_ty, &mut loop_env, mode);
                self.mark_pat_state(declarator.pat, &mut loop_env, InitState::Assigned);
              }
            }
          }
          if let Some(succ) = block.successors.get(0) {
            outgoing.push((*succ, loop_env.clone()));
          }
          if let Some(succ) = block.successors.get(1) {
            outgoing.push((*succ, env.clone()));
          }
          return outgoing;
        }
        StmtKind::Switch {
          discriminant,
          cases,
        } => {
          let discriminant_ty = self.eval_expr(*discriminant, &mut env).0;
          let target = self.switch_discriminant_target(*discriminant, discriminant_ty, &env);
          let default_remaining = target
            .as_ref()
            .and_then(|t| self.switch_default_remaining(t, cases));

          let mut case_envs = Vec::with_capacity(cases.len());
          for case in cases.iter() {
            let mut case_env = env.clone();
            if let Some(test) = case.test {
              let _ = self.eval_expr(test, &mut case_env);
              if let Some(target) = target.as_ref() {
                let _ = self.apply_switch_narrowing(target, test, &mut case_env);
              }
            } else if let (Some(target), Some(default_ty)) = (target.as_ref(), default_remaining) {
              self.apply_switch_result(target, default_ty, &mut case_env);
            }
            case_envs.push(case_env);
          }

          for (idx, case_env) in case_envs.iter().enumerate() {
            if let Some(succ) = block.successors.get(idx) {
              outgoing.push((*succ, case_env.clone()));
              if self.switch_case_falls_through(cases.get(idx)) {
                for later in (idx + 1)..cases.len() {
                  if let Some(later_succ) = block.successors.get(later) {
                    outgoing.push((*later_succ, case_env.clone()));
                  }
                }
              }
            }
          }
          // If there is an implicit default edge (no default case), use the final successor.
          if block.successors.len() > cases.len() {
            if let Some(succ) = block.successors.last() {
              let mut default_env = env.clone();
              if let (Some(target), Some(default_ty)) = (target.as_ref(), default_remaining) {
                self.apply_switch_result(target, default_ty, &mut default_env);
              }
              outgoing.push((*succ, default_env));
            }
          }
          return outgoing;
        }
        StmtKind::Try {
          block: _,
          catch,
          finally_block,
        } => {
          if let Some(succ) = block.successors.get(0) {
            outgoing.push((*succ, env.clone()));
          }
          if let Some((idx, catch_clause)) = catch.as_ref().map(|c| (1, c)) {
            let mut catch_env = env.clone();
            if let Some(param) = catch_clause.param {
              self.bind_pat(param, self.store.primitive_ids().unknown, &mut catch_env);
              self.mark_pat_state(param, &mut catch_env, InitState::Assigned);
            }
            if let Some(succ) = block.successors.get(idx) {
              outgoing.push((*succ, catch_env));
            }
          }
          if let Some(_) = finally_block {
            if let Some(pos) = catch.as_ref().map(|_| 2).or_else(|| Some(1)) {
              if let Some(succ) = block.successors.get(pos) {
                outgoing.push((*succ, env.clone()));
              }
            }
          }
          return outgoing;
        }
        _ => {}
      }
    }

    if outgoing.is_empty() {
      outgoing.extend(block.successors.iter().map(|succ| (*succ, env.clone())));
    }
    outgoing
  }

  fn record_return(&mut self, stmt: StmtId, ty: TypeId) {
    let prim = self.store.primitive_ids();
    if std::env::var("DEBUG_ASSERT_NARROW").is_ok() {
      eprintln!(
        "DEBUG record_return {:?} ty {}",
        stmt,
        TypeDisplay::new(&self.store, ty)
      );
    }
    let idx = *self.return_indices.entry(stmt).or_insert_with(|| {
      self.return_types.push(prim.unknown);
      self.return_types.len() - 1
    });
    let slot = self.return_types.get_mut(idx).unwrap();
    if *slot == prim.unknown {
      *slot = ty;
    } else {
      *slot = self.store.union(vec![*slot, ty]);
    }
  }

  fn eval_expr(&mut self, expr_id: ExprId, env: &mut Env) -> (TypeId, Facts) {
    self.eval_expr_inner(expr_id, env, false)
  }

  fn eval_expr_inner(
    &mut self,
    expr_id: ExprId,
    env: &mut Env,
    suppress_uninit: bool,
  ) -> (TypeId, Facts) {
    let prim = self.store.primitive_ids();
    let expr = &self.body.exprs[expr_id.0 as usize];
    let mut facts = Facts::default();
    let ty = match &expr.kind {
      ExprKind::Ident(name) => {
        let flow_binding = self.bindings.binding_for_expr(expr_id);
        let binding_key = self.bindings.binding_key_for_expr(expr_id);
        let ty = flow_binding
          .and_then(|id| env.get(id).or_else(|| self.initial.get(&id).copied()))
          .unwrap_or(prim.unknown);
        if std::env::var("DEBUG_ASSERT_NARROW").is_ok() {
          let name_text = self.hir_name(*name);
          eprintln!(
            "DEBUG ident {name_text} flow {:?} ty {} initial {:?}",
            flow_binding,
            TypeDisplay::new(&self.store, ty),
            flow_binding.and_then(
              |id| self
                .initial
                .get(&id)
                .copied()
                .map(|t| TypeDisplay::new(&self.store, t).to_string())
            )
          );
        }
        if let Some(binding) = binding_key {
          if !suppress_uninit && !self.param_bindings.contains(&binding) {
            let state = env.init_state(binding);
            if state != InitState::Assigned && self.reported_unassigned.insert(expr_id) {
              let span = Span {
                file: self.file,
                range: expr.span,
              };
              let name_text = self.hir_name(*name);
              self.diagnostics.push(
                codes::USE_BEFORE_ASSIGNMENT
                  .error(format!("{name_text} is used before being assigned"), span),
              );
            }
          }
        }
        let (truthy, falsy) = truthy_falsy_types(ty, &self.store);
        if let Some(id) = flow_binding {
          let key = FlowKey::root(id);
          facts.truthy.insert(key.clone(), truthy);
          facts.falsy.insert(key, falsy);
        }
        ty
      }
      ExprKind::Literal(lit) => match lit {
        hir_js::Literal::Number(num) => self.store.intern_type(TypeKind::NumberLiteral(
          num.parse::<f64>().unwrap_or(0.0).into(),
        )),
        hir_js::Literal::String(s) => self.store.intern_type(TypeKind::StringLiteral(
          self.store.intern_name_ref(&s.lossy),
        )),
        hir_js::Literal::Boolean(b) => self.store.intern_type(TypeKind::BooleanLiteral(*b)),
        hir_js::Literal::Null => prim.null,
        hir_js::Literal::Undefined => prim.undefined,
        hir_js::Literal::BigInt(v) => self.store.intern_type(TypeKind::BigIntLiteral(
          v.parse::<i128>().unwrap_or(0).into(),
        )),
        hir_js::Literal::Regex(_) => prim.unknown,
      },
      ExprKind::Super => self.super_ty,
      ExprKind::Unary { op, expr } => match op {
        UnaryOp::Not => {
          let (_, inner_facts) = self.eval_expr(*expr, env);
          facts.truthy = inner_facts.falsy;
          facts.falsy = inner_facts.truthy;
          facts.assertions = inner_facts.assertions;
          prim.boolean
        }
        UnaryOp::Typeof => {
          let _ = self.eval_expr_inner(*expr, env, true);
          prim.string
        }
        UnaryOp::Void => {
          let _ = self.eval_expr(*expr, env);
          prim.undefined
        }
        UnaryOp::Delete => {
          let _ = self.eval_expr(*expr, env);
          prim.boolean
        }
        UnaryOp::Plus | UnaryOp::Minus | UnaryOp::BitNot => {
          let _ = self.eval_expr(*expr, env);
          prim.number
        }
        _ => prim.unknown,
      },
      ExprKind::Update { expr, .. } => {
        let operand_ty = self.eval_expr(*expr, env).0;
        let result_ty = if self.is_bigint_like(self.base_type(operand_ty)) {
          prim.bigint
        } else {
          prim.number
        };
        self.write_assign_target_expr(*expr, result_ty, env, BindingMode::Assign);
        self.mark_expr_state(*expr, env, InitState::Assigned);
        if let Some(root) = self.assignment_target_root_expr(*expr) {
          self.record_assignment_facts(Some(root), result_ty, &mut facts);
        }
        result_ty
      }
      ExprKind::Binary { op, left, right } => match op {
        BinaryOp::LogicalAnd | BinaryOp::LogicalOr | BinaryOp::NullishCoalescing => {
          self.eval_logical(*op, *left, *right, env, &mut facts)
        }
        BinaryOp::Equality
        | BinaryOp::Inequality
        | BinaryOp::StrictEquality
        | BinaryOp::StrictInequality => {
          self.eval_equality(*op, *left, *right, env, &mut facts);
          prim.boolean
        }
        BinaryOp::LessThan
        | BinaryOp::LessEqual
        | BinaryOp::GreaterThan
        | BinaryOp::GreaterEqual => {
          let _ = self.eval_expr(*left, env);
          let _ = self.eval_expr(*right, env);
          prim.boolean
        }
        BinaryOp::Add => {
          let (l_ty, _) = self.eval_expr(*left, env);
          let (r_ty, _) = self.eval_expr(*right, env);
          match (self.store.type_kind(l_ty), self.store.type_kind(r_ty)) {
            (TypeKind::String | TypeKind::StringLiteral(_), _)
            | (_, TypeKind::String | TypeKind::StringLiteral(_)) => prim.string,
            (
              TypeKind::Number | TypeKind::NumberLiteral(_),
              TypeKind::Number | TypeKind::NumberLiteral(_),
            ) => prim.number,
            _ => self.store.union(vec![l_ty, r_ty]),
          }
        }
        BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Remainder
        | BinaryOp::Exponent
        | BinaryOp::ShiftLeft
        | BinaryOp::ShiftRight
        | BinaryOp::ShiftRightUnsigned
        | BinaryOp::BitOr
        | BinaryOp::BitAnd
        | BinaryOp::BitXor => {
          let _ = self.eval_expr(*left, env);
          let _ = self.eval_expr(*right, env);
          prim.number
        }
        BinaryOp::Instanceof => {
          let left_expr = *left;
          let left_ty = self.eval_expr(left_expr, env).0;
          let right_ty = self.eval_expr(*right, env).0;
          if let Some(binding) = self.ident_binding(left_expr) {
            let (yes, no) = narrow_by_instanceof_rhs(
              left_ty,
              right_ty,
              &self.store,
              &self.relate,
              self.ref_expander,
            );
            let key = FlowKey::root(binding);
            facts.truthy.insert(key.clone(), yes);
            facts.falsy.insert(key, no);
          }
          prim.boolean
        }
        BinaryOp::In => {
          let _ = self.eval_expr(*left, env);
          let right_ty = self.eval_expr(*right, env).0;
          if let (Some(prop), Some(binding)) =
            (self.literal_prop(*left), self.ident_binding(*right))
          {
            let (yes, no) = narrow_by_in_check(right_ty, &prop, &self.store, self.ref_expander);
            let key = FlowKey::root(binding);
            facts.truthy.insert(key.clone(), yes);
            facts.falsy.insert(key, no);
          }
          prim.boolean
        }
        BinaryOp::Comma => {
          let _ = self.eval_expr(*left, env);
          self.eval_expr(*right, env).0
        }
      },
      ExprKind::Assignment { op, target, value } => {
        let (left_ty, root, _) = self.assignment_target_info(*target, env);
        match op {
          AssignOp::Assign => {
            let val_ty = self.eval_expr(*value, env).0;
            self.assign_pat(*target, val_ty, env);
            let assigned_ty = self.apply_binding_mode(val_ty, BindingMode::Assign);
            self.record_assignment_facts(root, assigned_ty, &mut facts);
            assigned_ty
          }
          AssignOp::AddAssign => {
            let val_ty = self.eval_expr(*value, env).0;
            let result_ty = self.add_assign_result(left_ty, val_ty);
            self.assign_pat(*target, result_ty, env);
            self.record_assignment_facts(root, result_ty, &mut facts);
            result_ty
          }
          AssignOp::LogicalAndAssign => {
            self.logical_and_assign(*target, left_ty, *value, root, env, &mut facts)
          }
          AssignOp::LogicalOrAssign => {
            self.logical_or_assign(*target, left_ty, *value, root, env, &mut facts)
          }
          AssignOp::NullishAssign => {
            self.nullish_assign(*target, left_ty, *value, root, env, &mut facts)
          }
          _ => {
            let val_ty = self.eval_expr(*value, env).0;
            let result_ty = self.numeric_assign_result(left_ty, val_ty);
            self.assign_pat(*target, result_ty, env);
            self.record_assignment_facts(root, result_ty, &mut facts);
            result_ty
          }
        }
      }
      ExprKind::Call(call) => {
        let is_direct_super_call = self.is_derived_constructor
          && !call.is_new
          && matches!(self.body.exprs.get(call.callee.0 as usize).map(|e| &e.kind), Some(ExprKind::Super));
        let (ret_ty, chain_short_circuit) = self.eval_call(expr_id, call, env, &mut facts);
        if is_direct_super_call {
          env.this_init = InitState::Assigned;
        }
        if call.optional || chain_short_circuit {
          self.record_optional_chain_exec_type(expr_id, ret_ty);
        }
        if chain_short_circuit {
          self.store.union(vec![ret_ty, prim.undefined])
        } else {
          ret_ty
        }
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayMap { array, callback } => {
        self.eval_known_member_call_on_expr(expr_id, *array, "map", &[*callback], env, &mut facts)
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayFilter { array, callback } => self.eval_known_member_call_on_expr(
        expr_id,
        *array,
        "filter",
        &[*callback],
        env,
        &mut facts,
      ),
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayReduce {
        array,
        callback,
        init,
      } => {
        let mut args = vec![*callback];
        if let Some(init) = init {
          args.push(*init);
        }
        self.eval_known_member_call_on_expr(expr_id, *array, "reduce", &args, env, &mut facts)
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayFind { array, callback } => {
        self.eval_known_member_call_on_expr(expr_id, *array, "find", &[*callback], env, &mut facts)
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayEvery { array, callback } => {
        self.eval_known_member_call_on_expr(expr_id, *array, "every", &[*callback], env, &mut facts)
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArraySome { array, callback } => {
        self.eval_known_member_call_on_expr(expr_id, *array, "some", &[*callback], env, &mut facts)
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayChain { array, ops } => {
        let _ = self.eval_expr(*array, env);
        let mut current = self.expand_ref(self.expr_types[array.0 as usize]);
        self.expr_types[array.0 as usize] = current;
        for op in ops {
          match op {
            hir_js::ArrayChainOp::Map(callback) => {
              current = self.resolve_known_member_call(
                expr_id,
                None,
                current,
                "map",
                &[*callback],
                env,
                &mut facts,
              );
            }
            hir_js::ArrayChainOp::Filter(callback) => {
              current = self.resolve_known_member_call(
                expr_id,
                None,
                current,
                "filter",
                &[*callback],
                env,
                &mut facts,
              );
            }
            hir_js::ArrayChainOp::Reduce(callback, init) => {
              let mut args = vec![*callback];
              if let Some(init) = init {
                args.push(*init);
              }
              current = self.resolve_known_member_call(
                expr_id, None, current, "reduce", &args, env, &mut facts,
              );
            }
            hir_js::ArrayChainOp::Find(callback) => {
              current = self.resolve_known_member_call(
                expr_id,
                None,
                current,
                "find",
                &[*callback],
                env,
                &mut facts,
              );
            }
            hir_js::ArrayChainOp::Every(callback) => {
              current = self.resolve_known_member_call(
                expr_id,
                None,
                current,
                "every",
                &[*callback],
                env,
                &mut facts,
              );
            }
            hir_js::ArrayChainOp::Some(callback) => {
              current = self.resolve_known_member_call(
                expr_id,
                None,
                current,
                "some",
                &[*callback],
                env,
                &mut facts,
              );
            }
          }
        }
        current
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
        for promise in promises.iter() {
          let _ = self.eval_expr(*promise, env);
        }
        prim.unknown
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::KnownApiCall { args, .. } => {
        for arg in args {
          let _ = self.eval_expr(*arg, env);
        }
        prim.unknown
      }
      ExprKind::Member(mem) => {
        if self.is_derived_constructor
          && env.this_init != InitState::Assigned
          && matches!(
            self.body.exprs.get(mem.object.0 as usize).map(|e| &e.kind),
            Some(ExprKind::Super)
          )
        {
          self.diagnostics.push(
            codes::SUPER_MUST_BE_CALLED_BEFORE_THIS.error(
              "'super' must be called before accessing 'this' in the constructor of a derived class.",
              Span::new(self.file, expr.span),
            ),
          );
        }
        let obj_ty = self.eval_expr(mem.object, env).0;
        let chain_short_circuit = self.optional_chain_short_circuits(expr_id, env);
        if !mem.optional && !chain_short_circuit {
          let obj_exec_ty = self.expand_ref(obj_ty);
          if self.type_contains_undefined(obj_exec_ty) {
            let span = *self
              .expr_spans
              .get(mem.object.0 as usize)
              .unwrap_or(&TextRange::new(0, 0));
            self.diagnostics.push(codes::POSSIBLY_UNDEFINED.error(
              "identifier is possibly undefined",
              Span::new(self.file, span),
            ));
          }
        }
        let obj_exec_ty = if chain_short_circuit {
          self.optional_chain_exec_type_for(mem.object, obj_ty)
        } else {
          obj_ty
        };

        let (obj_non_nullish, obj_nullish) = if mem.optional {
          narrow_non_nullish(obj_ty, &self.store)
        } else {
          (obj_exec_ty, prim.never)
        };

        if mem.optional && obj_non_nullish == prim.never {
          self.record_optional_chain_exec_type(expr_id, prim.never);
          // Optional chaining (`x?.y`) short-circuits on a nullish base; if
          // the base is always nullish, the whole expression is `undefined`
          // and the property expression is not evaluated.
          prim.undefined
        } else if chain_short_circuit && !mem.optional && obj_exec_ty == prim.never {
          self.record_optional_chain_exec_type(expr_id, prim.never);
          prim.undefined
        } else {
          if let ObjectKey::Computed(expr) = &mem.property {
            let _ = self.eval_expr(*expr, env);
          }
          let obj_ty_for_member = if mem.optional {
            obj_non_nullish
          } else if chain_short_circuit {
            obj_exec_ty
          } else {
            obj_ty
          };
          let prop_ty = match (
            self.ident_binding(mem.object),
            self.member_path_segment(&mem.property),
          ) {
            (Some(binding), Some(segment)) => {
              let key = FlowKey::root(binding).with_segment(segment);
              let derived = self.member_type(obj_ty_for_member, mem);
              match env.get_path(&key) {
                Some(stored) => {
                  let (overlap, _) =
                    narrow_by_assignability(stored, derived, &self.store, &self.relate);
                  if overlap == prim.never {
                    derived
                  } else {
                    overlap
                  }
                }
                None => derived,
              }
            }
            _ => self.member_type(obj_ty_for_member, mem),
          };

          if mem.optional || chain_short_circuit {
            self.record_optional_chain_exec_type(expr_id, prop_ty);
          }
          if chain_short_circuit {
            self.insert_optional_chain_truthy_facts(expr_id, env, &mut facts);
          }

          if mem.optional {
            if let Some(key) = self.flow_key_for_expr(mem.object) {
              if obj_non_nullish != prim.never {
                facts.truthy.insert(key, obj_non_nullish);
              }
            }
            if obj_nullish != prim.never {
              self.store.union(vec![prop_ty, prim.undefined])
            } else {
              prop_ty
            }
          } else if chain_short_circuit {
            self.store.union(vec![prop_ty, prim.undefined])
          } else {
            prop_ty
          }
        }
      }
      ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => {
        let cond_facts = self.eval_expr(*test, env).1;
        let mut then_env = env.clone();
        then_env.apply_facts(&cond_facts);
        let mut else_env = env.clone();
        else_env.apply_falsy(&cond_facts);
        let then_ty = self.eval_expr(*consequent, &mut then_env).0;
        let else_ty = self.eval_expr(*alternate, &mut else_env).0;
        self.store.union(vec![then_ty, else_ty])
      }
      ExprKind::Array(arr) => {
        let mut elem_tys = Vec::new();
        for elem in arr.elements.iter() {
          match elem {
            ArrayElement::Expr(id) | ArrayElement::Spread(id) => {
              let elem_ty = self.eval_expr(*id, env).0;
              let widened = match self.store.type_kind(elem_ty) {
                TypeKind::NumberLiteral(_) => prim.number,
                TypeKind::StringLiteral(_) => prim.string,
                TypeKind::BooleanLiteral(_) => prim.boolean,
                _ => elem_ty,
              };
              elem_tys.push(widened);
            }
            ArrayElement::Empty => {}
          }
        }
        let elem_ty = if elem_tys.is_empty() {
          prim.any
        } else {
          self.store.union(elem_tys)
        };
        self.store.intern_type(TypeKind::Array {
          ty: elem_ty,
          readonly: false,
        })
      }
      ExprKind::Object(obj) => self.object_type(obj, env),
      ExprKind::FunctionExpr { def, .. } => self
        .expr_def_types
        .get(def)
        .copied()
        .unwrap_or(prim.unknown),
      ExprKind::ClassExpr { def, .. } => self
        .expr_def_types
        .get(def)
        .copied()
        .unwrap_or(prim.unknown),
      ExprKind::Template(template) => {
        for span in template.spans.iter() {
          let _ = self.eval_expr(span.expr, env);
        }
        prim.string
      }
      ExprKind::TaggedTemplate { tag, template } => {
        let _ = self.eval_expr(*tag, env);
        for span in template.spans.iter() {
          let _ = self.eval_expr(span.expr, env);
        }
        prim.unknown
      }
      ExprKind::ImportCall {
        argument,
        attributes,
      } => {
        let _ = self.eval_expr(*argument, env);
        if let Some(attrs) = attributes {
          let _ = self.eval_expr(*attrs, env);
        }

        let module_ty = match &self.body.exprs[argument.0 as usize].kind {
          ExprKind::Literal(hir_js::Literal::String(s)) => self
            .type_resolver
            .as_ref()
            .and_then(|resolver| resolver.resolve_import_typeof(s.lossy.as_str(), None))
            .map(|def| self.store.intern_type(TypeKind::Ref {
              def,
              args: Vec::new(),
            })),
          _ => None,
        };

        match module_ty {
          Some(module_ty) => self.promise_type(module_ty),
          None => self.promise_any,
        }
      }
      ExprKind::Await { expr } => {
        let inner = self.eval_expr(*expr, env).0;
        awaited_type(self.store.as_ref(), inner, self.ref_expander)
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::AwaitExpr { value: expr, .. } => {
        let inner = self.eval_expr(*expr, env).0;
        awaited_type(self.store.as_ref(), inner, self.ref_expander)
      }
      ExprKind::Yield { expr, .. } => expr
        .map(|id| self.eval_expr(id, env).0)
        .unwrap_or(prim.undefined),
      ExprKind::Instantiation { expr, .. } => self.eval_expr(*expr, env).0,
      ExprKind::TypeAssertion {
        expr,
        const_assertion,
        ..
      } => {
        let inner = self.eval_expr(*expr, env).0;
        if *const_assertion {
          inner
        } else {
          // The flow checker intentionally does not attempt to fully lower the
          // type annotation (it would require access to declaration/type arenas).
          // However, it must not treat `x as number` / `x as string` as a
          // narrowing operation by propagating literal types through the graph.
          //
          // Widen primitive literals so that `1 as number` behaves like `number`
          // during flow-based type recomputation, keeping results aligned with
          // the main checker (and `tsc`).
          match self.store.type_kind(inner) {
            TypeKind::NumberLiteral(_) => prim.number,
            TypeKind::StringLiteral(_) | TypeKind::TemplateLiteral(_) => prim.string,
            TypeKind::BooleanLiteral(_) => prim.boolean,
            _ => inner,
          }
        }
      }
      ExprKind::NonNull { expr: inner_expr } => {
        let inner_ty = self.eval_expr(*inner_expr, env).0;
        let (_, nonnull) = narrow_by_nullish_equality(
          inner_ty,
          BinaryOp::Equality,
          &LiteralValue::Null,
          &self.store,
        );
        nonnull
      }
      ExprKind::Satisfies { expr, .. } => {
        let prev = self.widen_object_literals;
        self.widen_object_literals = false;
        let ty = self.eval_expr(*expr, env).0;
        self.widen_object_literals = prev;
        ty
      }
      ExprKind::This => {
        if self.is_derived_constructor && env.this_init != InitState::Assigned {
          self.diagnostics.push(
            codes::SUPER_MUST_BE_CALLED_BEFORE_THIS.error(
              "'super' must be called before accessing 'this' in the constructor of a derived class.",
              Span::new(self.file, expr.span),
            ),
          );
          prim.unknown
        } else {
          self.this_ty
        }
      }
      ExprKind::Jsx(elem) => {
        for attr in elem.attributes.iter() {
          match attr {
            hir_js::JsxAttr::Named { value, .. } => {
              if let Some(value) = value {
                match value {
                  hir_js::JsxAttrValue::Expression(container) => {
                    if let Some(expr) = container.expr {
                      let _ = self.eval_expr(expr, env);
                    }
                  }
                  hir_js::JsxAttrValue::Element(expr) => {
                    let _ = self.eval_expr(*expr, env);
                  }
                  hir_js::JsxAttrValue::Text(_) => {}
                }
              }
            }
            hir_js::JsxAttr::Spread { expr } => {
              let _ = self.eval_expr(*expr, env);
            }
          }
        }
        for child in elem.children.iter() {
          match child {
            hir_js::JsxChild::Text(_) => {}
            hir_js::JsxChild::Expr(container) => {
              if let Some(expr) = container.expr {
                let _ = self.eval_expr(expr, env);
              }
            }
            hir_js::JsxChild::Element(expr) => {
              let _ = self.eval_expr(*expr, env);
            }
          }
        }
        prim.unknown
      }
      _ => prim.unknown,
    };

    let slot = &mut self.expr_types[expr_id.0 as usize];
    *slot = if *slot == prim.unknown {
      ty
    } else {
      self.store.union(vec![*slot, ty])
    };
    (ty, facts)
  }

  fn eval_logical(
    &mut self,
    op: BinaryOp,
    left: ExprId,
    right: ExprId,
    env: &mut Env,
    out: &mut Facts,
  ) -> TypeId {
    let (left_ty, mut left_facts) = self.eval_expr(left, env);
    match op {
      BinaryOp::LogicalAnd => {
        let mut right_env = env.clone();
        right_env.apply_facts(&left_facts);
        let (right_ty, right_facts) = self.eval_expr(right, &mut right_env);
        *out = and_facts(left_facts, right_facts, &self.store);
        self.store.union(vec![left_ty, right_ty])
      }
      BinaryOp::LogicalOr => {
        let mut right_env = env.clone();
        right_env.apply_falsy(&left_facts);
        let (right_ty, right_facts) = self.eval_expr(right, &mut right_env);
        let mut combined = or_facts(left_facts.clone(), right_facts, &self.store);
        for (key, ty) in combined.truthy.iter_mut() {
          if !left_facts.truthy.contains_key(key) {
            if let Some(orig) = env
              .get(key.root)
              .or_else(|| self.initial.get(&key.root).copied())
            {
              *ty = self.store.union(vec![*ty, orig]);
            }
          }
        }
        *out = combined;
        self.store.union(vec![left_ty, right_ty])
      }
      BinaryOp::NullishCoalescing => {
        let prim = self.store.primitive_ids();
        let (nonnullish, nullish) = narrow_non_nullish(left_ty, &self.store);

        // `tsc` currently does not propagate narrowing derived from optional chaining across
        // nullish coalescing (`x?.y ?? z`). This keeps our flow facts aligned with the baseline
        // diagnostics (notably TS18048 for `x?.y ?? false`).
        let mut optional_bases = Vec::new();
        self.optional_chain_base_keys(left, &mut optional_bases);
        for base in optional_bases {
          left_facts.truthy.remove(&base);
          left_facts.falsy.remove(&base);
        }

        let mut right_env = env.clone();
        right_env.apply_map(&left_facts.assertions);

        let mut left_nullish = HashMap::new();
        if let Some(binding) = self.nullish_coalesce_binding(left) {
          let key = FlowKey::root(binding);
          left_nullish.insert(key.clone(), nullish);
          right_env.set(binding, nullish);
        }

        let (right_ty, right_facts) = self.eval_expr(right, &mut right_env);
        if nullish == prim.never {
          // The RHS is unreachable at runtime; still type-check it, but do not
          // incorporate its narrowing facts into the current environment.
          *out = left_facts;
          nonnullish
        } else {
          *out = nullish_coalesce_facts(left_facts, right_facts, left_nullish, &self.store);
          self.store.union(vec![nonnullish, right_ty])
        }
      }
      _ => {
        let right_ty = self.eval_expr(right, env).0;
        self.store.union(vec![left_ty, right_ty])
      }
    }
  }

  fn eval_equality(
    &mut self,
    op: BinaryOp,
    left: ExprId,
    right: ExprId,
    env: &mut Env,
    out: &mut Facts,
  ) {
    let left_ty = self.eval_expr(left, env).0;
    let right_ty = self.eval_expr(right, env).0;
    let negate = matches!(op, BinaryOp::Inequality | BinaryOp::StrictInequality);

    fn apply_narrowing(
      out: &mut Facts,
      negate: bool,
      target: FlowBindingId,
      yes: TypeId,
      no: TypeId,
    ) {
      let key = FlowKey::root(target);
      if negate {
        out.truthy.insert(key.clone(), no);
        out.falsy.insert(key, yes);
      } else {
        out.truthy.insert(key.clone(), yes);
        out.falsy.insert(key, no);
      }
    }

    let mut apply_literal_narrow =
      |target: FlowBindingId, target_ty: TypeId, lit: &LiteralValue| {
        if matches!(lit, LiteralValue::Null | LiteralValue::Undefined) {
          let (yes, no) = narrow_by_nullish_equality(target_ty, op, lit, &self.store);
          apply_narrowing(out, negate, target, yes, no);
        } else {
          let (yes, no) = narrow_by_literal(target_ty, lit, &self.store);
          apply_narrowing(out, negate, target, yes, no);
        }
      };

    if let Some(target) = self.ident_binding(left) {
      if let Some(lit) = self.literal_value(right) {
        apply_literal_narrow(target, left_ty, &lit);
        return;
      }
    }
    if let Some(target) = self.ident_binding(right) {
      if let Some(lit) = self.literal_value(left) {
        apply_literal_narrow(target, right_ty, &lit);
        return;
      }
    }

    if let Some((target, path, object_expr, optional_bases)) = self.discriminant_member(left) {
      if let Some(lit) = self.literal_value(right) {
        self.optional_chain_equality_facts(left, right, right_ty, op, env, out);
        self.optional_chain_equality_facts(right, left, left_ty, op, env, out);

        let target_ty = env
          .get(target)
          .or_else(|| self.initial.get(&target).copied())
          .unwrap_or_else(|| self.expr_types[object_expr.0 as usize]);
        let has_nested_optional = !optional_bases.is_empty();
        if let Some(prop_ty) = self.object_prop_type_path(target_ty, &path) {
          let (prop_yes, prop_no) = match lit {
            LiteralValue::Null | LiteralValue::Undefined => {
              narrow_by_nullish_equality(prop_ty, op, &lit, &self.store)
            }
            _ => narrow_by_literal(prop_ty, &lit, &self.store),
          };
          let mut flow_key = FlowKey::root(target);
          for seg in path.iter() {
            flow_key = flow_key.with_segment(seg.clone());
          }
          if has_nested_optional && !matches!(lit, LiteralValue::Null | LiteralValue::Undefined) {
            if negate {
              out.falsy.insert(flow_key, prop_yes);
            } else {
              out.truthy.insert(flow_key, prop_yes);
            }
          } else if negate {
            out.truthy.insert(flow_key.clone(), prop_no);
            out.falsy.insert(flow_key, prop_yes);
          } else {
            out.truthy.insert(flow_key.clone(), prop_yes);
            out.falsy.insert(flow_key, prop_no);
          }
        }
        match lit {
          LiteralValue::Null | LiteralValue::Undefined => {
            if let Some(prop_ty) = self.object_prop_type_path(target_ty, &path) {
              let (yes_prop, no_prop) = narrow_by_nullish_equality(prop_ty, op, &lit, &self.store);
              let yes = self.narrow_object_by_path_assignability(target_ty, &path, yes_prop);
              let no = self.narrow_object_by_path_assignability(target_ty, &path, no_prop);
              apply_narrowing(out, negate, target, yes, no);
              return;
            }
          }
          _ => {
            let (yes, no) =
              narrow_by_discriminant_path(target_ty, &path, &lit, &self.store, self.ref_expander);
            if has_nested_optional {
              let key = FlowKey::root(target);
              if negate {
                out.falsy.insert(key, yes);
              } else {
                out.truthy.insert(key, yes);
              }
            } else {
              apply_narrowing(out, negate, target, yes, no);
            }
            return;
          }
        }
      }
    }
    if let Some((target, path, object_expr, optional_bases)) = self.discriminant_member(right) {
      if let Some(lit) = self.literal_value(left) {
        self.optional_chain_equality_facts(left, right, right_ty, op, env, out);
        self.optional_chain_equality_facts(right, left, left_ty, op, env, out);

        let target_ty = env
          .get(target)
          .or_else(|| self.initial.get(&target).copied())
          .unwrap_or_else(|| self.expr_types[object_expr.0 as usize]);
        let has_nested_optional = !optional_bases.is_empty();
        if let Some(prop_ty) = self.object_prop_type_path(target_ty, &path) {
          let (prop_yes, prop_no) = match lit {
            LiteralValue::Null | LiteralValue::Undefined => {
              narrow_by_nullish_equality(prop_ty, op, &lit, &self.store)
            }
            _ => narrow_by_literal(prop_ty, &lit, &self.store),
          };
          let mut flow_key = FlowKey::root(target);
          for seg in path.iter() {
            flow_key = flow_key.with_segment(seg.clone());
          }
          if has_nested_optional && !matches!(lit, LiteralValue::Null | LiteralValue::Undefined) {
            if negate {
              out.falsy.insert(flow_key, prop_yes);
            } else {
              out.truthy.insert(flow_key, prop_yes);
            }
          } else if negate {
            out.truthy.insert(flow_key.clone(), prop_no);
            out.falsy.insert(flow_key, prop_yes);
          } else {
            out.truthy.insert(flow_key.clone(), prop_yes);
            out.falsy.insert(flow_key, prop_no);
          }
        }
        match lit {
          LiteralValue::Null | LiteralValue::Undefined => {
            if let Some(prop_ty) = self.object_prop_type_path(target_ty, &path) {
              let (yes_prop, no_prop) = narrow_by_nullish_equality(prop_ty, op, &lit, &self.store);
              let yes = self.narrow_object_by_path_assignability(target_ty, &path, yes_prop);
              let no = self.narrow_object_by_path_assignability(target_ty, &path, no_prop);
              apply_narrowing(out, negate, target, yes, no);
              return;
            }
          }
          _ => {
            let (yes, no) =
              narrow_by_discriminant_path(target_ty, &path, &lit, &self.store, self.ref_expander);
            if has_nested_optional {
              let key = FlowKey::root(target);
              if negate {
                out.falsy.insert(key, yes);
              } else {
                out.truthy.insert(key, yes);
              }
            } else {
              apply_narrowing(out, negate, target, yes, no);
            }
            return;
          }
        }
      }
    }

    if !negate {
      if let (Some(left_ref), Some(right_ref)) = (
        self.reference_from_expr(left, left_ty),
        self.reference_from_expr(right, right_ty),
      ) {
        let left_yes = self.narrow_reference_against(&left_ref, right_ref.value_ty());
        let right_yes = self.narrow_reference_against(&right_ref, left_ref.value_ty());
        if left_ref.target() == right_ref.target() {
          let combined = self.store.intersection(vec![left_yes, right_yes]);
          apply_narrowing(
            out,
            negate,
            left_ref.target(),
            combined,
            left_ref.target_ty(),
          );
        } else {
          apply_narrowing(
            out,
            negate,
            left_ref.target(),
            left_yes,
            left_ref.target_ty(),
          );
          apply_narrowing(
            out,
            negate,
            right_ref.target(),
            right_yes,
            right_ref.target_ty(),
          );
        }
        return;
      }
    }

    if let Some((target, target_ty, lit)) = self.typeof_comparison(left, right) {
      let (yes, no) = narrow_by_typeof(target_ty, &lit, &self.store);
      apply_narrowing(out, negate, target, yes, no);
    }

    self.optional_chain_equality_facts(left, right, right_ty, op, env, out);
    self.optional_chain_equality_facts(right, left, left_ty, op, env, out);
  }

  #[cfg(feature = "semantic-ops")]
  fn eval_known_member_call_on_expr(
    &mut self,
    expr_id: ExprId,
    receiver_expr: ExprId,
    method: &str,
    args: &[ExprId],
    env: &mut Env,
    out: &mut Facts,
  ) -> TypeId {
    let receiver_ty = self.eval_expr(receiver_expr, env).0;
    let receiver_base = self.expand_ref(receiver_ty);
    self.expr_types[receiver_expr.0 as usize] = receiver_base;
    self.resolve_known_member_call(
      expr_id,
      Some(receiver_expr),
      receiver_base,
      method,
      args,
      env,
      out,
    )
  }

  #[cfg(feature = "semantic-ops")]
  fn resolve_known_member_call(
    &mut self,
    expr_id: ExprId,
    receiver_expr: Option<ExprId>,
    receiver_ty: TypeId,
    method: &str,
    args: &[ExprId],
    env: &mut Env,
    out: &mut Facts,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    let receiver_ty = self.expand_ref(receiver_ty);
    let callee_ty = self.member_type_for_known_key(receiver_ty, method);

    let mut arg_bases: Vec<CallArgType> = Vec::new();
    for arg in args.iter() {
      let _ = self.eval_expr(*arg, env);
      let expanded = self.expand_ref(self.expr_types[arg.0 as usize]);
      self.expr_types[arg.0 as usize] = expanded;
      arg_bases.push(CallArgType::new(expanded));
    }

    let span = Span::new(
      self.file,
      *self
        .expr_spans
        .get(expr_id.0 as usize)
        .unwrap_or(&TextRange::new(0, 0)),
    );
    let resolution = resolve_call(
      &self.store,
      &self.relate,
      &self.instantiation_cache,
      callee_ty,
      &arg_bases,
      None,
      Some(receiver_ty),
      None,
      span,
      self.ref_expander,
    );

    let ret_ty = resolution.return_type;
    if let Some(sig_id) = resolution.signature {
      let sig = self.store.signature(sig_id);
      if let TypeKind::Predicate {
        asserted,
        asserts,
        parameter,
      } = self.store.type_kind(sig.ret)
      {
        if let Some(asserted) = asserted {
          match parameter.unwrap_or(PredicateParam::Param(0)) {
            PredicateParam::Param(target_idx) => {
              let target_idx = target_idx as usize;
              if let Some(arg_expr) = args.get(target_idx).copied() {
                if let Some(binding) = self.ident_binding(arg_expr) {
                  let arg_ty = arg_bases
                    .get(target_idx)
                    .map(|arg| arg.ty)
                    .unwrap_or(prim.unknown);
                  let (yes, no) =
                    narrow_by_assignability(arg_ty, asserted, &self.store, &self.relate);
                  if asserts {
                    env.set(binding, yes);
                    out.assertions.insert(FlowKey::root(binding), yes);
                  } else {
                    let key = FlowKey::root(binding);
                    out.truthy.insert(key.clone(), yes);
                    out.falsy.insert(key, no);
                  }
                }
              }
            }
            PredicateParam::This => {
              if let Some(this_expr) = receiver_expr {
                if let Some(binding) = self.ident_binding(this_expr) {
                  let arg_ty = receiver_ty;
                  let (yes, no) =
                    narrow_by_assignability(arg_ty, asserted, &self.store, &self.relate);
                  if asserts {
                    env.set(binding, yes);
                    out.assertions.insert(FlowKey::root(binding), yes);
                  } else {
                    let key = FlowKey::root(binding);
                    out.truthy.insert(key.clone(), yes);
                    out.falsy.insert(key, no);
                  }
                }
              }
            }
          }
        }
      }
    }

    ret_ty
  }

  fn eval_call(
    &mut self,
    expr_id: ExprId,
    call: &hir_js::CallExpr,
    env: &mut Env,
    out: &mut Facts,
  ) -> (TypeId, bool) {
    let prim = self.store.primitive_ids();
    let _ = self.eval_expr(call.callee, env);
    let callee_base = self.expand_ref(self.expr_types[call.callee.0 as usize]);
    self.expr_types[call.callee.0 as usize] = callee_base;

    let chain_short_circuit = self.optional_chain_short_circuits(expr_id, env);
    if chain_short_circuit {
      self.insert_optional_chain_truthy_facts(expr_id, env, out);
    } else if call.optional {
      if let Some(key) = self.flow_key_for_expr(call.callee) {
        let (non_nullish, _) = narrow_non_nullish(self.flow_key_type(env, &key), &self.store);
        if non_nullish != prim.never {
          out.truthy.insert(key, non_nullish);
        }
      }
    }
    let callee_exec_ty = if chain_short_circuit {
      self.optional_chain_exec_type_for(call.callee, callee_base)
    } else {
      callee_base
    };
    if chain_short_circuit && callee_exec_ty == prim.never {
      return (prim.never, chain_short_circuit);
    }

    let (callee_non_nullish, _) = if call.optional {
      narrow_non_nullish(callee_exec_ty, &self.store)
    } else {
      (callee_exec_ty, prim.never)
    };
    if call.optional && callee_non_nullish == prim.never {
      return (prim.never, chain_short_circuit);
    }

    let mut arg_bases: Vec<CallArgType> = Vec::new();
    for arg in call.args.iter() {
      let _ = self.eval_expr(arg.expr, env);
      let expanded = self.expand_ref(self.expr_types[arg.expr.0 as usize]);
      self.expr_types[arg.expr.0 as usize] = expanded;
      arg_bases.push(if arg.spread {
        CallArgType::spread(expanded)
      } else {
        CallArgType::new(expanded)
      });
    }

    let mut callee_for_this = call.callee;
    loop {
      let Some(expr) = self.body.exprs.get(callee_for_this.0 as usize) else {
        break;
      };
      match &expr.kind {
        // TypeScript-only wrappers; preserve the runtime call target for `this` typing.
        ExprKind::Instantiation { expr, .. }
        | ExprKind::TypeAssertion { expr, .. }
        | ExprKind::NonNull { expr }
        | ExprKind::Satisfies { expr, .. } => {
          callee_for_this = *expr;
        }
        _ => break,
      }
    }

    let this_arg = match &self.body.exprs[callee_for_this.0 as usize].kind {
      ExprKind::Member(MemberExpr { object, .. }) => {
        let mut ty = if matches!(self.body.exprs[object.0 as usize].kind, ExprKind::Super) {
          self.this_ty
        } else {
          self.expr_types[object.0 as usize]
        };
        if chain_short_circuit {
          ty = self.optional_chain_exec_type_for(*object, ty);
        }
        Some(ty)
      }
      _ => None,
    };

    let span = Span::new(
      self.file,
      *self
        .expr_spans
        .get(expr_id.0 as usize)
        .unwrap_or(&TextRange::new(0, 0)),
    );
    let is_super_call = matches!(
      self.body.exprs[call.callee.0 as usize].kind,
      ExprKind::Super
    );
    let resolution = if call.is_new || is_super_call {
      let construct_target = if is_super_call {
        self.this_super_context.super_value_ty.unwrap_or(prim.unknown)
      } else {
        callee_non_nullish
      };
      resolve_construct(
        &self.store,
        &self.relate,
        &self.instantiation_cache,
        construct_target,
        &arg_bases,
        None,
        None,
        None,
        span,
        self.ref_expander,
      )
    } else {
      resolve_call(
        &self.store,
        &self.relate,
        &self.instantiation_cache,
        callee_non_nullish,
        &arg_bases,
        None,
        this_arg,
        None,
        span,
        self.ref_expander,
      )
    };
    if std::env::var("DEBUG_ASSERT_NARROW").is_ok() {
      eprintln!(
        "DEBUG call resolution sig {:?} ret {}",
        resolution.signature,
        TypeDisplay::new(&self.store, resolution.return_type)
      );
    }

    if let Some(sig_id) = resolution.signature.or(resolution.contextual_signature) {
      let entry = self
        .call_signatures
        .entry(expr_id)
        .or_insert(CallSignatureState::Unresolved);
      match entry {
        CallSignatureState::Unresolved => {
          *entry = CallSignatureState::Resolved(sig_id);
        }
        CallSignatureState::Resolved(existing) => {
          if *existing != sig_id {
            *entry = CallSignatureState::Conflict;
          }
        }
        CallSignatureState::Conflict => {}
      }
    } else {
      self
        .call_signatures
        .entry(expr_id)
        .or_insert(CallSignatureState::Unresolved);
    }

    let mut ret_ty = resolution.return_type;
    if !call.is_new && !is_super_call {
      if let Some(sig_id) = resolution.signature {
        let sig = self.store.signature(sig_id);
        if let TypeKind::Predicate {
          asserted,
          asserts,
          parameter,
        } = self.store.type_kind(sig.ret)
        {
          if let Some(asserted) = asserted {
            match parameter.unwrap_or(PredicateParam::Param(0)) {
              PredicateParam::Param(target_idx) => {
                let target_idx = target_idx as usize;
                if let Some(arg_expr) = call.args.get(target_idx).map(|a| a.expr) {
                  if let Some(binding) = self.ident_binding(arg_expr) {
                    let arg_ty = arg_bases
                      .get(target_idx)
                      .map(|arg| arg.ty)
                      .unwrap_or(prim.unknown);
                    let (yes, no) =
                      narrow_by_assignability(arg_ty, asserted, &self.store, &self.relate);
                    if asserts {
                      if std::env::var("DEBUG_ASSERT_NARROW").is_ok() {
                        eprintln!(
                          "DEBUG asserts narrowing arg {} to {} (no {}) in file {:?}",
                          TypeDisplay::new(&self.store, arg_ty),
                          TypeDisplay::new(&self.store, yes),
                          TypeDisplay::new(&self.store, no),
                          self.file
                        );
                      }
                      env.set(binding, yes);
                      out.assertions.insert(FlowKey::root(binding), yes);
                    } else {
                      let key = FlowKey::root(binding);
                      out.truthy.insert(key.clone(), yes);
                      out.falsy.insert(key, no);
                      if std::env::var("DEBUG_ASSERT_NARROW").is_ok() {
                        eprintln!(
                          "DEBUG predicate narrowing arg {} to {} (no {}) in file {:?}",
                          TypeDisplay::new(&self.store, arg_ty),
                          TypeDisplay::new(&self.store, yes),
                          TypeDisplay::new(&self.store, no),
                          self.file
                        );
                      }
                    }
                  }
                }
              }
              PredicateParam::This => {
                if let Some(this_expr) = match &self.body.exprs[callee_for_this.0 as usize].kind {
                  ExprKind::Member(MemberExpr { object, .. }) => Some(*object),
                  _ => None,
                } {
                  if let Some(binding) = self.ident_binding(this_expr) {
                    let arg_ty = this_arg.unwrap_or(prim.unknown);
                    let (yes, no) =
                      narrow_by_assignability(arg_ty, asserted, &self.store, &self.relate);
                    if asserts {
                      env.set(binding, yes);
                      out.assertions.insert(FlowKey::root(binding), yes);
                    } else {
                      let key = FlowKey::root(binding);
                      out.truthy.insert(key.clone(), yes);
                      out.falsy.insert(key, no);
                    }
                  }
                }
              }
            }
          }
          ret_ty = if asserts {
            prim.undefined
          } else {
            prim.boolean
          };
        } else {
          ret_ty = sig.ret;
        }
      }
    }

    (ret_ty, chain_short_circuit)
  }

  fn optional_chain_equality_facts(
    &mut self,
    expr: ExprId,
    other: ExprId,
    other_ty: TypeId,
    op: BinaryOp,
    env: &Env,
    out: &mut Facts,
  ) {
    let prim = self.store.primitive_ids();
    let mut bases = Vec::new();
    self.optional_chain_base_keys(expr, &mut bases);
    if bases.is_empty() {
      return;
    }

    let effective_other_ty = match self.literal_value(other) {
      Some(LiteralValue::Undefined) => {
        let binding = self.ident_binding(other);
        let binding_key = binding.and_then(|id| self.bindings.binding_for_flow(id));
        if binding_key.is_none() || matches!(binding_key, Some(BindingKey::External(_))) {
          prim.undefined
        } else {
          other_ty
        }
      }
      Some(LiteralValue::Null) => prim.null,
      _ => other_ty,
    };

    fn insert_fact(
      store: &TypeStore,
      target: &mut HashMap<FlowKey, TypeId>,
      key: FlowKey,
      ty: TypeId,
    ) {
      target
        .entry(key)
        .and_modify(|existing| *existing = store.intersection(vec![*existing, ty]))
        .or_insert(ty);
    }

    let negate = matches!(op, BinaryOp::Inequality | BinaryOp::StrictInequality);
    let (eq_target, neq_target) = if negate {
      (&mut out.falsy, &mut out.truthy)
    } else {
      (&mut out.truthy, &mut out.falsy)
    };

    if self.excludes_nullish(effective_other_ty) {
      for base in bases.iter() {
        let base_ty = self.flow_key_type(env, base);
        let (non_nullish, _) = narrow_non_nullish(base_ty, &self.store);
        if non_nullish != prim.never {
          insert_fact(self.store.as_ref(), eq_target, base.clone(), non_nullish);
        }
      }
      return;
    }

    if self.is_nullish_only(effective_other_ty) {
      let op_for_equality = match op {
        BinaryOp::Inequality => BinaryOp::Equality,
        BinaryOp::StrictInequality => BinaryOp::StrictEquality,
        _ => op,
      };
      let (other_undefined, _) = narrow_by_nullish_equality(
        effective_other_ty,
        op_for_equality,
        &LiteralValue::Undefined,
        &self.store,
      );
      if other_undefined != prim.never {
        for base in bases.iter() {
          let base_ty = self.flow_key_type(env, base);
          let (non_nullish, _) = narrow_non_nullish(base_ty, &self.store);
          if non_nullish != prim.never {
            insert_fact(self.store.as_ref(), neq_target, base.clone(), non_nullish);
          }
        }
        if bases.len() == 1 {
          if let Some(exec_ty) = self.optional_chain_exec_type(expr) {
            let (_, exec_nullish) = narrow_non_nullish(exec_ty, &self.store);
            if exec_nullish == prim.never {
              let base = &bases[0];
              let base_ty = self.flow_key_type(env, base);
              let (_, base_nullish) = narrow_non_nullish(base_ty, &self.store);
              if base_nullish != prim.never {
                insert_fact(self.store.as_ref(), eq_target, base.clone(), base_nullish);
              }
            }
          }
        }
      }
    }
  }

  fn flow_key_for_expr(&self, expr_id: ExprId) -> Option<FlowKey> {
    if let Some(binding) = self.ident_binding(expr_id) {
      return Some(FlowKey::root(binding));
    }
    if let ExprKind::Member(mem) = &self.body.exprs[expr_id.0 as usize].kind {
      let (binding, segments, _) = self.member_path_target(mem.object, &mem.property)?;
      let mut key = FlowKey::root(binding);
      for seg in segments {
        key = key.with_segment(seg);
      }
      return Some(key);
    }
    None
  }

  fn optional_chain_exec_type(&self, expr_id: ExprId) -> Option<TypeId> {
    self
      .optional_chain_exec_types
      .get(expr_id.0 as usize)
      .copied()
      .flatten()
  }

  fn optional_chain_exec_type_for(&self, expr_id: ExprId, fallback: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    let Some(exec) = self.optional_chain_exec_type(expr_id) else {
      return fallback;
    };
    let (overlap, _) = narrow_by_assignability(exec, fallback, &self.store, &self.relate);
    if overlap != prim.never {
      overlap
    } else {
      fallback
    }
  }

  fn record_optional_chain_exec_type(&mut self, expr_id: ExprId, ty: TypeId) {
    let prim = self.store.primitive_ids();
    let slot = self
      .optional_chain_exec_types
      .get_mut(expr_id.0 as usize)
      .unwrap();
    let merged = match slot {
      None => ty,
      Some(prev) if *prev == prim.unknown => ty,
      Some(prev) => self.store.union(vec![*prev, ty]),
    };
    *slot = Some(merged);
  }

  fn optional_chain_base_exprs(&self, expr_id: ExprId, out: &mut Vec<ExprId>) {
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Member(mem) => {
        self.optional_chain_base_exprs(mem.object, out);
        if mem.optional {
          out.push(mem.object);
        }
      }
      ExprKind::Call(call) => {
        self.optional_chain_base_exprs(call.callee, out);
        if call.optional {
          out.push(call.callee);
        }
      }
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. } => {
        self.optional_chain_base_exprs(*expr, out);
      }
      _ => {}
    }
  }

  fn optional_chain_base_keys(&self, expr_id: ExprId, out: &mut Vec<FlowKey>) {
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Member(mem) => {
        self.optional_chain_base_keys(mem.object, out);
        if mem.optional {
          if let Some(key) = self.flow_key_for_expr(mem.object) {
            out.push(key);
          }
        }
      }
      ExprKind::Call(call) => {
        self.optional_chain_base_keys(call.callee, out);
        if call.optional {
          if let Some(key) = self.flow_key_for_expr(call.callee) {
            out.push(key);
          }
        }
      }
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. } => {
        self.optional_chain_base_keys(*expr, out);
      }
      _ => {}
    }
  }

  fn flow_key_type(&self, env: &Env, key: &FlowKey) -> TypeId {
    let prim = self.store.primitive_ids();
    if let Some(ty) = env.get_path(key) {
      return ty;
    }
    let root_ty = env
      .get(key.root)
      .or_else(|| self.initial.get(&key.root).copied())
      .unwrap_or(prim.unknown);
    if key.segments.is_empty() {
      return root_ty;
    }
    self
      .object_prop_type_path(root_ty, &key.segments)
      .unwrap_or(prim.unknown)
  }

  fn optional_chain_short_circuits(&self, expr_id: ExprId, env: &Env) -> bool {
    let prim = self.store.primitive_ids();
    let mut bases = Vec::new();
    self.optional_chain_base_exprs(expr_id, &mut bases);
    for base in bases {
      let base_ty = if let Some(key) = self.flow_key_for_expr(base) {
        self.flow_key_type(env, &key)
      } else {
        self
          .expr_types
          .get(base.0 as usize)
          .copied()
          .unwrap_or(prim.unknown)
      };
      let (_, nullish) = narrow_non_nullish(base_ty, &self.store);
      if nullish != prim.never {
        return true;
      }
    }
    false
  }

  fn insert_optional_chain_truthy_facts(&self, expr_id: ExprId, env: &Env, facts: &mut Facts) {
    let prim = self.store.primitive_ids();
    let mut bases = Vec::new();
    self.optional_chain_base_keys(expr_id, &mut bases);
    for base in bases {
      let base_ty = self.flow_key_type(env, &base);
      let (non_nullish, _) = narrow_non_nullish(base_ty, &self.store);
      if non_nullish != prim.never {
        facts.truthy.insert(base, non_nullish);
      }
    }
  }

  fn excludes_nullish(&self, ty: TypeId) -> bool {
    let prim = self.store.primitive_ids();
    let (_, nullish) = narrow_non_nullish(ty, &self.store);
    nullish == prim.never
  }

  fn is_nullish_only(&self, ty: TypeId) -> bool {
    let prim = self.store.primitive_ids();
    let (non_nullish, nullish) = narrow_non_nullish(ty, &self.store);
    non_nullish == prim.never && nullish != prim.never
  }

  fn type_contains_undefined(&self, ty: TypeId) -> bool {
    fn inner(checker: &FlowBodyChecker<'_>, ty: TypeId, seen: &mut HashSet<TypeId>) -> bool {
      let ty = checker.store.canon(ty);
      if !seen.insert(ty) {
        return false;
      }
      let ty = checker.expand_ref(ty);
      match checker.store.type_kind(ty) {
        TypeKind::Any | TypeKind::Unknown => false,
        TypeKind::Undefined => true,
        TypeKind::Union(members) | TypeKind::Intersection(members) => members
          .iter()
          .copied()
          .any(|member| inner(checker, member, seen)),
        _ => false,
      }
    }

    let mut seen = HashSet::new();
    inner(self, ty, &mut seen)
  }

  fn ident_binding(&self, expr_id: ExprId) -> Option<FlowBindingId> {
    match self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Ident(_) => self.bindings.binding_for_expr(expr_id),
      _ => None,
    }
  }

  fn nullish_coalesce_binding(&self, expr_id: ExprId) -> Option<FlowBindingId> {
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Ident(_) => self.ident_binding(expr_id),
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. } => self.nullish_coalesce_binding(*expr),
      _ => None,
    }
  }

  fn literal_value(&self, expr_id: ExprId) -> Option<LiteralValue> {
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Ident(name) if self.hir_name(*name) == "undefined" => {
        let binding = self.ident_binding(expr_id);
        let binding_key = binding.and_then(|id| self.bindings.binding_for_flow(id));
        match binding_key {
          Some(BindingKey::External(_)) | None => Some(LiteralValue::Undefined),
          _ => None,
        }
      }
      ExprKind::Literal(lit) => match lit {
        hir_js::Literal::String(s) => Some(LiteralValue::String(s.lossy.clone())),
        hir_js::Literal::Number(n) => Some(LiteralValue::Number(n.clone())),
        hir_js::Literal::Boolean(b) => Some(LiteralValue::Boolean(*b)),
        hir_js::Literal::Null => Some(LiteralValue::Null),
        hir_js::Literal::Undefined => Some(LiteralValue::Undefined),
        _ => None,
      },
      _ => None,
    }
  }

  fn literal_prop(&self, expr_id: ExprId) -> Option<String> {
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Literal(hir_js::Literal::String(s)) => Some(s.lossy.clone()),
      _ => None,
    }
  }

  fn assignment_target_root_expr(&self, expr_id: ExprId) -> Option<FlowBindingId> {
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Ident(_) => self.ident_binding(expr_id),
      ExprKind::Member(mem) => self.assignment_target_root_expr(mem.object),
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. }
      | ExprKind::Await { expr }
      | ExprKind::Yield {
        expr: Some(expr), ..
      } => self.assignment_target_root_expr(*expr),
      #[cfg(feature = "semantic-ops")]
      ExprKind::AwaitExpr { value: expr, .. } => self.assignment_target_root_expr(*expr),
      _ => None,
    }
  }

  fn record_assignment_facts(&self, root: Option<FlowBindingId>, ty: TypeId, facts: &mut Facts) {
    if let Some(binding) = root {
      let (truthy, falsy) = truthy_falsy_types(ty, &self.store);
      let key = FlowKey::root(binding);
      facts.truthy.insert(key.clone(), truthy);
      facts.falsy.insert(key, falsy);
    }
  }

  fn apply_binding_mode(&self, ty: TypeId, mode: BindingMode) -> TypeId {
    match mode {
      BindingMode::Declare => ty,
      BindingMode::Assign => self.base_type(ty),
    }
  }

  fn expand_ref(&self, ty: TypeId) -> TypeId {
    let mut current = self.store.canon(ty);
    if let Some(expander) = self.ref_expander {
      let mut seen = HashSet::new();
      while seen.insert(current) {
        match self.store.type_kind(current) {
          TypeKind::Ref { def, args } => {
            if let Some(expanded) = expander.expand_ref(&self.store, def, &args) {
              current = self.store.canon(expanded);
              continue;
            }
          }
          _ => {}
        }
        break;
      }
    }
    current
  }

  fn base_type(&self, ty: TypeId) -> TypeId {
    match self.store.type_kind(ty) {
      TypeKind::BooleanLiteral(_) => self.store.primitive_ids().boolean,
      TypeKind::NumberLiteral(_) => self.store.primitive_ids().number,
      TypeKind::StringLiteral(_) => self.store.primitive_ids().string,
      TypeKind::BigIntLiteral(_) => self.store.primitive_ids().bigint,
      TypeKind::Union(members) => {
        let mapped: Vec<_> = members.into_iter().map(|m| self.base_type(m)).collect();
        self.store.union(mapped)
      }
      TypeKind::Intersection(members) => {
        let mapped: Vec<_> = members.into_iter().map(|m| self.base_type(m)).collect();
        self.store.intersection(mapped)
      }
      _ => ty,
    }
  }

  fn promise_type(&self, inner: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    let Some(def) = self.promise_def else {
      return prim.unknown;
    };
    self
      .store
      .canon(self.store.intern_type(TypeKind::Ref { def, args: vec![inner] }))
  }

  fn for_of_element_type(&self, ty: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    match self.store.type_kind(ty) {
      TypeKind::Union(members) => {
        let mut elems = Vec::new();
        for member in members {
          elems.push(self.for_of_element_type(member));
        }
        self.store.union(elems)
      }
      TypeKind::Intersection(members) => {
        let mut elems = Vec::new();
        for member in members {
          elems.push(self.for_of_element_type(member));
        }
        self.store.intersection(elems)
      }
      TypeKind::Array { ty, .. } => ty,
      TypeKind::Tuple(elems) => {
        let mut members = Vec::new();
        for elem in elems {
          members.push(elem.ty);
        }
        self.store.union(members)
      }
      TypeKind::String | TypeKind::StringLiteral(_) => prim.string,
      _ => prim.unknown,
    }
  }

  fn is_bigint_like(&self, ty: TypeId) -> bool {
    match self.store.type_kind(ty) {
      TypeKind::BigInt | TypeKind::BigIntLiteral(_) => true,
      TypeKind::Union(members) => members.iter().all(|m| self.is_bigint_like(*m)),
      TypeKind::Intersection(members) => members.iter().all(|m| self.is_bigint_like(*m)),
      _ => false,
    }
  }

  fn maybe_string(&self, ty: TypeId) -> bool {
    match self.store.type_kind(ty) {
      TypeKind::String | TypeKind::StringLiteral(_) => true,
      TypeKind::Union(members) | TypeKind::Intersection(members) => {
        members.iter().any(|m| self.maybe_string(*m))
      }
      _ => false,
    }
  }

  fn split_nullish(&self, ty: TypeId) -> (TypeId, TypeId) {
    let prim = self.store.primitive_ids();
    match self.store.type_kind(ty) {
      TypeKind::Union(members) => {
        let mut non_nullish = Vec::new();
        let mut nullish = Vec::new();
        for member in members {
          let (nonnull, nulls) = self.split_nullish(member);
          if nonnull != prim.never {
            non_nullish.push(nonnull);
          }
          if nulls != prim.never {
            nullish.push(nulls);
          }
        }
        (self.store.union(non_nullish), self.store.union(nullish))
      }
      TypeKind::Null | TypeKind::Undefined => (prim.never, ty),
      _ => (ty, prim.never),
    }
  }

  fn hir_name(&self, id: NameId) -> String {
    self
      .names
      .resolve(id)
      .map(|s| s.to_owned())
      .unwrap_or_default()
  }

  fn member_property_name(&self, property: &ObjectKey) -> Option<String> {
    match property {
      ObjectKey::Ident(id) => Some(self.hir_name(*id)),
      ObjectKey::String(s) => Some(s.clone()),
      ObjectKey::Number(n) => Some(n.clone()),
      ObjectKey::Computed(expr) => self.literal_value(*expr).and_then(|lit| match lit {
        LiteralValue::String(s) | LiteralValue::Number(s) => Some(s),
        _ => None,
      }),
    }
  }

  fn member_path_segment(&self, property: &ObjectKey) -> Option<PathSegment> {
    match property {
      ObjectKey::Ident(id) => Some(PathSegment::String(self.hir_name(*id))),
      ObjectKey::String(s) => Some(PathSegment::String(s.clone())),
      ObjectKey::Number(n) => Some(PathSegment::Number(n.clone())),
      ObjectKey::Computed(expr) => self.literal_value(*expr).and_then(|lit| match lit {
        LiteralValue::String(s) => Some(PathSegment::String(s)),
        LiteralValue::Number(s) => Some(PathSegment::Number(s)),
        _ => None,
      }),
    }
  }

  fn member_path_target(
    &self,
    object: ExprId,
    property: &ObjectKey,
  ) -> Option<(FlowBindingId, Vec<PathSegment>, ExprId)> {
    let segment = self.member_path_segment(property)?;
    if let Some(binding) = self.ident_binding(object) {
      return Some((binding, vec![segment], object));
    }
    if let ExprKind::Member(MemberExpr {
      object, property, ..
    }) = &self.body.exprs[object.0 as usize].kind
    {
      let (binding, mut segments, root_expr) = self.member_path_target(*object, property)?;
      segments.push(segment);
      return Some((binding, segments, root_expr));
    }
    None
  }

  fn object_prop_type_path(&self, obj: TypeId, path: &[PathSegment]) -> Option<TypeId> {
    let mut ty = obj;
    for seg in path {
      let key = match seg {
        PathSegment::String(s) | PathSegment::Number(s) => s.as_str(),
      };
      ty = self.object_prop_type(ty, key)?;
    }
    Some(ty)
  }

  fn narrow_object_by_path_assignability(
    &self,
    obj_ty: TypeId,
    path: &[PathSegment],
    required_prop_ty: TypeId,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    if required_prop_ty == prim.never {
      return prim.never;
    }
    match self.store.type_kind(obj_ty) {
      TypeKind::Union(members) => {
        let mut narrowed = Vec::new();
        for member in members {
          let filtered = self.narrow_object_by_path_assignability(member, path, required_prop_ty);
          if filtered != prim.never {
            narrowed.push(filtered);
          }
        }
        self.store.union(narrowed)
      }
      _ => {
        if let Some(prop_ty) = self.object_prop_type_path(obj_ty, path) {
          let (overlap, _) =
            narrow_by_assignability(prop_ty, required_prop_ty, &self.store, &self.relate);
          if overlap != prim.never {
            return obj_ty;
          }
          return prim.never;
        }
        obj_ty
      }
    }
  }

  fn member_chain_target(
    &self,
    expr_id: ExprId,
  ) -> Option<(FlowBindingId, Vec<(PathSegment, bool)>, ExprId)> {
    let ExprKind::Member(MemberExpr {
      object,
      property,
      optional,
    }) = &self.body.exprs[expr_id.0 as usize].kind
    else {
      return None;
    };
    let segment = self.member_path_segment(property)?;
    if let Some(binding) = self.ident_binding(*object) {
      return Some((binding, vec![(segment, *optional)], *object));
    }
    if matches!(
      &self.body.exprs[object.0 as usize].kind,
      ExprKind::Member(_)
    ) {
      let (binding, mut segments, root_expr) = self.member_chain_target(*object)?;
      segments.push((segment, *optional));
      return Some((binding, segments, root_expr));
    }
    None
  }

  fn discriminant_member(
    &self,
    expr_id: ExprId,
  ) -> Option<(
    FlowBindingId,
    Vec<PathSegment>,
    ExprId,
    Vec<Vec<PathSegment>>,
  )> {
    let (binding, segments, root_expr) = self.member_chain_target(expr_id)?;
    let mut path = Vec::with_capacity(segments.len());
    let mut optional_bases = Vec::new();
    let mut prefix: Vec<PathSegment> = Vec::new();
    for (idx, (segment, optional)) in segments.iter().enumerate() {
      if *optional && idx > 0 {
        optional_bases.push(prefix.clone());
      }
      path.push(segment.clone());
      prefix.push(segment.clone());
    }
    Some((binding, path, root_expr, optional_bases))
  }

  fn typeof_comparison(
    &self,
    left: ExprId,
    right: ExprId,
  ) -> Option<(FlowBindingId, TypeId, String)> {
    let left_expr = &self.body.exprs[left.0 as usize].kind;
    let right_expr = &self.body.exprs[right.0 as usize].kind;
    match (left_expr, right_expr) {
      (
        ExprKind::Unary {
          op: UnaryOp::Typeof,
          expr,
        },
        ExprKind::Literal(hir_js::Literal::String(s)),
      ) => {
        if let Some(binding) = self.ident_binding(*expr) {
          return Some((binding, self.expr_types[expr.0 as usize], s.lossy.clone()));
        }
      }
      (
        ExprKind::Literal(hir_js::Literal::String(s)),
        ExprKind::Unary {
          op: UnaryOp::Typeof,
          expr,
        },
      ) => {
        if let Some(binding) = self.ident_binding(*expr) {
          return Some((binding, self.expr_types[expr.0 as usize], s.lossy.clone()));
        }
      }
      _ => {}
    }
    None
  }
  fn assignment_expr_info(
    &mut self,
    expr_id: ExprId,
    env: &mut Env,
  ) -> (TypeId, Option<FlowBindingId>, bool) {
    let prim = self.store.primitive_ids();
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Ident(_) => {
        let binding = self.ident_binding(expr_id);
        let ty = binding
          .and_then(|id| env.get(id).or_else(|| self.initial.get(&id).copied()))
          .unwrap_or(prim.unknown);
        (ty, binding, false)
      }
      ExprKind::Member(mem) => {
        let obj_ty = self.eval_expr(mem.object, env).0;
        if let ObjectKey::Computed(prop) = &mem.property {
          let _ = self.eval_expr(*prop, env);
        }
        let prop_ty = self.member_type(obj_ty, mem);
        let root = self.assignment_target_root_expr(mem.object);
        (
          prop_ty,
          root,
          matches!(mem.property, ObjectKey::Computed(_)),
        )
      }
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. } => self.assignment_expr_info(*expr, env),
      _ => (prim.unknown, None, false),
    }
  }

  fn assignment_target_info(
    &mut self,
    pat_id: PatId,
    env: &mut Env,
  ) -> (TypeId, Option<FlowBindingId>, bool) {
    let pat = &self.body.pats[pat_id.0 as usize];
    let prim = self.store.primitive_ids();
    match &pat.kind {
      PatKind::Ident(_) => {
        let binding = self.bindings.binding_for_pat(pat_id);
        let ty = binding
          .and_then(|id| env.get(id).or_else(|| self.initial.get(&id).copied()))
          .unwrap_or(prim.unknown);
        (ty, binding, false)
      }
      PatKind::Assign { target, .. } => self.assignment_target_info(*target, env),
      PatKind::Rest(inner) => self.assignment_target_info(**inner, env),
      PatKind::AssignTarget(expr) => self.assignment_expr_info(*expr, env),
      _ => (prim.unknown, None, false),
    }
  }

  fn reference_from_expr(&self, expr_id: ExprId, expr_ty: TypeId) -> Option<Reference> {
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Ident(_) => self.ident_binding(expr_id).map(|id| Reference::Ident {
        name: id,
        ty: expr_ty,
      }),
      ExprKind::Member(mem) => {
        let base = self.ident_binding(mem.object)?;
        let prop = match &mem.property {
          ObjectKey::Ident(id) => self.hir_name(*id),
          ObjectKey::String(s) => s.clone(),
          ObjectKey::Number(n) => n.clone(),
          ObjectKey::Computed(_) => return None,
        };
        let base_ty = self.expr_types[mem.object.0 as usize];
        Some(Reference::Member {
          base,
          prop,
          base_ty,
          prop_ty: expr_ty,
        })
      }
      _ => None,
    }
  }

  fn narrow_reference_against(&self, reference: &Reference, other_value_ty: TypeId) -> TypeId {
    match reference {
      Reference::Ident { ty, .. } => {
        let (yes, _) = narrow_by_assignability(*ty, other_value_ty, &self.store, &self.relate);
        yes
      }
      Reference::Member { base_ty, prop, .. } => {
        self.narrow_object_by_prop_assignability(*base_ty, prop, other_value_ty)
      }
    }
  }

  fn narrow_object_by_prop_assignability(
    &self,
    obj_ty: TypeId,
    prop: &str,
    required_prop_ty: TypeId,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    if required_prop_ty == prim.never {
      return prim.never;
    }
    match self.store.type_kind(obj_ty) {
      TypeKind::Union(members) => {
        let mut narrowed = Vec::new();
        for member in members {
          let filtered = self.narrow_object_by_prop_assignability(member, prop, required_prop_ty);
          if filtered != prim.never {
            narrowed.push(filtered);
          }
        }
        self.store.union(narrowed)
      }
      _ => {
        if let Some(prop_ty) = self.object_prop_type(obj_ty, prop) {
          let (overlap, _) =
            narrow_by_assignability(prop_ty, required_prop_ty, &self.store, &self.relate);
          if overlap != prim.never {
            return obj_ty;
          }
          return prim.never;
        }
        obj_ty
      }
    }
  }

  fn numeric_assign_result(&self, left: TypeId, right: TypeId) -> TypeId {
    let left_base = self.base_type(left);
    let right_base = self.base_type(right);
    if self.is_bigint_like(left_base) && self.is_bigint_like(right_base) {
      self.store.primitive_ids().bigint
    } else {
      self.store.primitive_ids().number
    }
  }

  fn add_assign_result(&self, left: TypeId, right: TypeId) -> TypeId {
    let left_base = self.base_type(left);
    let right_base = self.base_type(right);
    let prim = self.store.primitive_ids();
    if self.is_bigint_like(left_base) && self.is_bigint_like(right_base) {
      return prim.bigint;
    }
    if self.maybe_string(left_base) || self.maybe_string(right_base) {
      self.store.union(vec![prim.string, prim.number])
    } else {
      prim.number
    }
  }

  fn logical_and_assign(
    &mut self,
    target: PatId,
    left: TypeId,
    value: ExprId,
    root: Option<FlowBindingId>,
    env: &mut Env,
    facts: &mut Facts,
  ) -> TypeId {
    let left_base = self.base_type(left);
    let (left_truthy, left_falsy) = truthy_falsy_types(left_base, &self.store);
    let mut right_env = env.clone();
    if let Some(binding) = root {
      right_env.set(binding, left_truthy);
    }
    let right_ty = self.eval_expr(value, &mut right_env).0;
    let result_ty = self.store.union(vec![left_falsy, self.base_type(right_ty)]);
    self.assign_pat(target, result_ty, env);
    self.record_assignment_facts(root, result_ty, facts);
    result_ty
  }

  fn logical_or_assign(
    &mut self,
    target: PatId,
    left: TypeId,
    value: ExprId,
    root: Option<FlowBindingId>,
    env: &mut Env,
    facts: &mut Facts,
  ) -> TypeId {
    let left_base = self.base_type(left);
    let (left_truthy, left_falsy) = truthy_falsy_types(left_base, &self.store);
    let mut right_env = env.clone();
    if let Some(binding) = root {
      right_env.set(binding, left_falsy);
    }
    let right_ty = self.eval_expr(value, &mut right_env).0;
    let result_ty = self
      .store
      .union(vec![left_truthy, self.base_type(right_ty)]);
    self.assign_pat(target, result_ty, env);
    self.record_assignment_facts(root, result_ty, facts);
    result_ty
  }

  fn nullish_assign(
    &mut self,
    target: PatId,
    left: TypeId,
    value: ExprId,
    root: Option<FlowBindingId>,
    env: &mut Env,
    facts: &mut Facts,
  ) -> TypeId {
    let left_base = self.base_type(left);
    let (nonnullish, nullish) = self.split_nullish(left_base);
    let mut right_env = env.clone();
    if let Some(binding) = root {
      right_env.set(binding, nullish);
    }
    let right_ty = self.eval_expr(value, &mut right_env).0;
    let result_ty = self.store.union(vec![nonnullish, self.base_type(right_ty)]);
    self.assign_pat(target, result_ty, env);
    self.record_assignment_facts(root, result_ty, facts);
    result_ty
  }

  fn assign_pat(&mut self, pat_id: PatId, value_ty: TypeId, env: &mut Env) {
    self.bind_pat_with_mode(pat_id, value_ty, env, BindingMode::Assign);
    self.mark_pat_state(pat_id, env, InitState::Assigned);
  }

  fn bind_pat(&mut self, pat_id: PatId, value_ty: TypeId, env: &mut Env) {
    self.bind_pat_with_mode(pat_id, value_ty, env, BindingMode::Declare);
  }

  fn bind_pat_with_mode(
    &mut self,
    pat_id: PatId,
    value_ty: TypeId,
    env: &mut Env,
    mode: BindingMode,
  ) {
    let pat = &self.body.pats[pat_id.0 as usize];
    let prim = self.store.primitive_ids();
    let write_ty = self.apply_binding_mode(value_ty, mode);
    let slot = &mut self.pat_types[pat_id.0 as usize];
    *slot = if *slot == prim.unknown {
      write_ty
    } else {
      self.store.union(vec![*slot, write_ty])
    };
    match &pat.kind {
      PatKind::Ident(_) => {
        if let Some(id) = self.bindings.binding_for_pat(pat_id) {
          if matches!(mode, BindingMode::Assign) {
            env.invalidate(id);
          }
          env.set(id, write_ty);
        }
      }
      PatKind::Assign {
        target,
        default_value,
      } => {
        let default_eval = self.eval_expr(*default_value, env).0;
        let default_ty = self.apply_binding_mode(default_eval, mode);
        let combined = self.store.union(vec![write_ty, default_ty]);
        self.bind_pat_with_mode(*target, combined, env, mode);
      }
      PatKind::Rest(inner) => self.bind_pat_with_mode(**inner, write_ty, env, mode),
      PatKind::Array(arr) => {
        let element_ty = match self.store.type_kind(value_ty) {
          TypeKind::Array { ty, .. } => ty,
          TypeKind::Tuple(elems) => elems.first().map(|e| e.ty).unwrap_or(prim.unknown),
          _ => prim.unknown,
        };
        for (idx, elem) in arr.elements.iter().enumerate() {
          if let Some(elem) = elem {
            let mut ty = element_ty;
            if let TypeKind::Tuple(elems) = self.store.type_kind(value_ty) {
              if let Some(specific) = elems.get(idx) {
                ty = specific.ty;
              }
            }
            ty = self.apply_binding_mode(ty, mode);
            if let Some(default) = elem.default_value {
              let default_eval = self.eval_expr(default, env).0;
              let default_ty = self.apply_binding_mode(default_eval, mode);
              ty = self.store.union(vec![ty, default_ty]);
            }
            self.bind_pat_with_mode(elem.pat, ty, env, mode);
          }
        }
        if let Some(rest) = arr.rest {
          let rest_ty = self.apply_binding_mode(value_ty, mode);
          self.bind_pat_with_mode(rest, rest_ty, env, mode);
        }
      }
      PatKind::Object(obj) => {
        for prop in obj.props.iter() {
          let mut prop_ty = prim.unknown;
          match &prop.key {
            ObjectKey::Ident(id) => {
              let name = self.hir_name(*id);
              if let Some(found) = self.object_prop_type(value_ty, &name) {
                prop_ty = found;
              }
            }
            ObjectKey::String(s) => {
              if let Some(found) = self.object_prop_type(value_ty, s) {
                prop_ty = found;
              }
            }
            ObjectKey::Number(n) => {
              if let Some(found) = self.object_prop_type(value_ty, n) {
                prop_ty = found;
              }
            }
            ObjectKey::Computed(expr) => {
              let key_ty = self.eval_expr(*expr, env).0;

              let literal_key = self
                .literal_value(*expr)
                .and_then(|lit| match lit {
                  LiteralValue::String(s) | LiteralValue::Number(s) => Some(s),
                  _ => None,
                })
                .or_else(|| match self.store.type_kind(key_ty) {
                  TypeKind::StringLiteral(id) => Some(self.store.name(id).to_string()),
                  TypeKind::NumberLiteral(num) => Some(num.0.to_string()),
                  _ => None,
                });

              prop_ty = if let Some(key) = literal_key {
                self.member_type_for_known_key(value_ty, &key)
              } else {
                self.member_type_for_index_key(value_ty, key_ty)
              };

              if self.relate.options.no_unchecked_indexed_access {
                prop_ty = self.store.union(vec![prop_ty, prim.undefined]);
              }
            }
          }
          prop_ty = self.apply_binding_mode(prop_ty, mode);
          if let Some(default) = prop.default_value {
            let default_eval = self.eval_expr(default, env).0;
            let default_ty = self.apply_binding_mode(default_eval, mode);
            prop_ty = self.store.union(vec![prop_ty, default_ty]);
          }
          self.bind_pat_with_mode(prop.value, prop_ty, env, mode);
        }
        if let Some(rest) = obj.rest {
          let rest_ty = self.apply_binding_mode(value_ty, mode);
          self.bind_pat_with_mode(rest, rest_ty, env, mode);
        }
      }
      PatKind::AssignTarget(expr) => {
        self.write_assign_target_expr(*expr, write_ty, env, mode);
      }
    }
  }

  fn mark_expr_state(&self, expr_id: ExprId, env: &mut Env, state: InitState) {
    if let Some(binding) = self.bindings.binding_key_for_expr(expr_id) {
      env.set_init_state(binding, state);
    }
  }

  fn mark_pat_state(&self, pat_id: PatId, env: &mut Env, state: InitState) {
    let pat = &self.body.pats[pat_id.0 as usize];
    match &pat.kind {
      PatKind::Ident(_) => {
        if let Some(binding) = self.bindings.binding_key_for_pat(pat_id) {
          env.set_init_state(binding, state);
        }
      }
      PatKind::Assign { target, .. } => self.mark_pat_state(*target, env, state),
      PatKind::Rest(inner) => self.mark_pat_state(**inner, env, state),
      PatKind::Array(arr) => {
        for elem in arr.elements.iter().flatten() {
          self.mark_pat_state(elem.pat, env, state);
        }
        if let Some(rest) = arr.rest {
          self.mark_pat_state(rest, env, state);
        }
      }
      PatKind::Object(obj) => {
        for prop in obj.props.iter() {
          self.mark_pat_state(prop.value, env, state);
        }
        if let Some(rest) = obj.rest {
          self.mark_pat_state(rest, env, state);
        }
      }
      PatKind::AssignTarget(expr) => self.mark_expr_state(*expr, env, state),
    }
  }

  fn write_assign_target_expr(
    &mut self,
    expr_id: ExprId,
    value_ty: TypeId,
    env: &mut Env,
    mode: BindingMode,
  ) {
    match &self.body.exprs[expr_id.0 as usize].kind {
      ExprKind::Ident(_) => {
        if let Some(id) = self.ident_binding(expr_id) {
          if matches!(mode, BindingMode::Assign) {
            env.invalidate(id);
          }
          env.set(id, value_ty);
        }
      }
      ExprKind::Member(mem) => {
        if let Some(root) = self.assignment_target_root_expr(mem.object) {
          let root_ty = env
            .get(root)
            .or_else(|| self.initial.get(&root).copied())
            .unwrap_or(self.store.primitive_ids().unknown);
          let widened = self.widen_object_prop(root_ty);
          env.invalidate(root);
          env.set(root, self.base_type(widened));
        } else if matches!(mem.property, ObjectKey::Computed(_)) {
          env.clear_all_properties();
        }
      }
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. } => {
        self.write_assign_target_expr(*expr, value_ty, env, mode);
      }
      _ => {}
    }
  }

  fn object_type(&mut self, obj: &ObjectLiteral, env: &mut Env) -> TypeId {
    if obj.properties.is_empty() {
      // Keep the flow checker consistent with the AST checker: `{}` expression
      // literals infer the `{}` (empty object) type, not the `object` keyword.
      return self.store.intern_type(TypeKind::EmptyObject);
    }
    let mut shape = Shape::new();
    for prop in obj.properties.iter() {
      match prop {
        ObjectProperty::KeyValue { key, value, .. } => {
          let prop_key = match key {
            ObjectKey::Ident(id) => PropKey::String(
              self
                .store
                .intern_name_ref(self.names.resolve(*id).unwrap_or("")),
            ),
            ObjectKey::String(s) => PropKey::String(self.store.intern_name_ref(s)),
            ObjectKey::Number(n) => PropKey::Number(n.parse::<i64>().unwrap_or(0)),
            ObjectKey::Computed(expr) => {
              let key_ty = self.eval_expr(*expr, env).0;
              let ty = self.eval_expr(*value, env).0;
              let ty = if self.widen_object_literals {
                self.widen_object_prop(ty)
              } else {
                ty
              };
              let prop_key = match self.store.type_kind(key_ty) {
                TypeKind::StringLiteral(id) => Some(PropKey::String(id)),
                TypeKind::NumberLiteral(num) => {
                  Some(PropKey::String(self.store.intern_name(num.0.to_string())))
                }
                _ => None,
              };
              if let Some(prop_key) = prop_key {
                shape.properties.push(types_ts_interned::Property {
                  key: prop_key,
                  data: PropData {
                    ty,
                    optional: false,
                    readonly: false,
                    accessibility: None,
                    is_method: false,
                    origin: None,
                    declared_on: None,
                  },
                });
              }
              continue;
            }
          };
          let ty = self.eval_expr(*value, env).0;
          let ty = if self.widen_object_literals {
            self.widen_object_prop(ty)
          } else {
            ty
          };
          shape.properties.push(types_ts_interned::Property {
            key: prop_key,
            data: PropData {
              ty,
              optional: false,
              readonly: false,
              accessibility: None,
              is_method: false,
              origin: None,
              declared_on: None,
            },
          });
        }
        ObjectProperty::Getter { key, .. } | ObjectProperty::Setter { key, .. } => {
          let prop_key = match key {
            ObjectKey::Ident(id) => PropKey::String(
              self
                .store
                .intern_name_ref(self.names.resolve(*id).unwrap_or("")),
            ),
            ObjectKey::String(s) => PropKey::String(self.store.intern_name_ref(s)),
            ObjectKey::Number(n) => PropKey::Number(n.parse::<i64>().unwrap_or(0)),
            ObjectKey::Computed(expr) => {
              let _ = self.eval_expr(*expr, env);
              continue;
            }
          };
          shape.properties.push(types_ts_interned::Property {
            key: prop_key,
            data: PropData {
              ty: self.store.primitive_ids().unknown,
              optional: false,
              readonly: false,
              accessibility: None,
              is_method: true,
              origin: None,
              declared_on: None,
            },
          });
        }
        ObjectProperty::Spread(expr) => {
          let _ = self.eval_expr(*expr, env);
        }
      }
    }
    let shape_id = self.store.intern_shape(shape);
    let obj_id = self.store.intern_object(ObjectType { shape: shape_id });
    self.store.intern_type(TypeKind::Object(obj_id))
  }

  fn widen_object_prop(&self, ty: TypeId) -> TypeId {
    let prim = self.store.primitive_ids();
    match self.store.type_kind(ty) {
      TypeKind::NumberLiteral(_) => prim.number,
      TypeKind::StringLiteral(_) => prim.string,
      TypeKind::BooleanLiteral(_) => prim.boolean,
      TypeKind::Union(members) => {
        let mapped: Vec<_> = members
          .into_iter()
          .map(|m| self.widen_object_prop(m))
          .collect();
        self.store.union(mapped)
      }
      TypeKind::Intersection(members) => {
        let mapped: Vec<_> = members
          .into_iter()
          .map(|m| self.widen_object_prop(m))
          .collect();
        self.store.intersection(mapped)
      }
      _ => ty,
    }
  }

  fn member_type(&mut self, obj: TypeId, mem: &MemberExpr) -> TypeId {
    let prim = self.store.primitive_ids();

    let mut ty = match &mem.property {
      ObjectKey::Computed(expr) => {
        let key_ty = self.expr_types[expr.0 as usize];
        let literal_key = self
          .literal_value(*expr)
          .and_then(|lit| match lit {
            LiteralValue::String(s) | LiteralValue::Number(s) => Some(s),
            _ => None,
          })
          .or_else(|| match self.store.type_kind(key_ty) {
            TypeKind::StringLiteral(id) => Some(self.store.name(id)),
            TypeKind::NumberLiteral(num) => Some(num.0.to_string()),
            _ => None,
          });

        if let Some(key) = literal_key {
          self.member_type_for_known_key(obj, &key)
        } else {
          self.member_type_for_index_key(obj, key_ty)
        }
      }
      _ => {
        let Some(key) = self.member_property_name(&mem.property) else {
          return prim.unknown;
        };
        self.member_type_for_known_key(obj, &key)
      }
    };

    if matches!(
      mem.property,
      ObjectKey::Computed(_) | ObjectKey::String(_) | ObjectKey::Number(_)
    ) && self.relate.options.no_unchecked_indexed_access
    {
      ty = self.store.union(vec![ty, prim.undefined]);
    }

    ty
  }

  fn member_type_for_known_key(&self, obj: TypeId, key: &str) -> TypeId {
    let receiver = self.store.canon(obj);
    self.member_type_for_known_key_with_receiver(obj, key, receiver)
  }

  fn member_type_for_known_key_with_receiver(
    &self,
    obj: TypeId,
    key: &str,
    receiver: TypeId,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    let expanded = self.expand_ref(obj);
    let ty = match self.store.type_kind(expanded) {
      TypeKind::Union(members) => {
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_known_key_with_receiver(member, key, receiver));
        }
        self.store.union(collected)
      }
      TypeKind::Intersection(members) => {
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_known_key_with_receiver(member, key, receiver));
        }
        self.store.intersection(collected)
      }
      TypeKind::Ref { .. } => prim.unknown,
      TypeKind::Tuple(elems) => match key.parse::<usize>() {
        Ok(idx) => {
          let options = self.relate.options;
          if let Some(elem) = elems.get(idx) {
            let mut ty = elem.ty;
            if elem.optional && !options.exact_optional_property_types {
              ty = self.store.union(vec![ty, prim.undefined]);
            }
            ty
          } else {
            prim.undefined
          }
        }
        Err(_) => {
          let Ok(parsed) = key.parse::<f64>() else {
            return prim.unknown;
          };
          if parsed.fract() != 0.0 || parsed < 0.0 {
            return prim.unknown;
          }
          let idx = parsed as usize;
          let options = self.relate.options;
          if let Some(elem) = elems.get(idx) {
            let mut ty = elem.ty;
            if elem.optional && !options.exact_optional_property_types {
              ty = self.store.union(vec![ty, prim.undefined]);
            }
            ty
          } else {
            prim.undefined
          }
        }
      },
      _ => self
        .object_prop_type_with_receiver(obj, key, receiver)
        .unwrap_or(prim.unknown),
    };

    substitute_this_type(&self.store, ty, receiver)
  }

  fn member_type_for_index_key(&self, obj: TypeId, key_ty: TypeId) -> TypeId {
    let receiver = self.store.canon(obj);
    self.member_type_for_index_key_with_receiver(obj, key_ty, receiver)
  }

  fn member_type_for_index_key_with_receiver(
    &self,
    obj: TypeId,
    key_ty: TypeId,
    receiver: TypeId,
  ) -> TypeId {
    let prim = self.store.primitive_ids();
    let key_ty = self.store.canon(key_ty);
    match self.store.type_kind(key_ty) {
      TypeKind::Union(members) => {
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_index_key_with_receiver(obj, member, receiver));
        }
        return substitute_this_type(&self.store, self.store.union(collected), receiver);
      }
      TypeKind::Intersection(members) => {
        // Keep this conservative: treat intersections of key types similarly to unions.
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_index_key_with_receiver(obj, member, receiver));
        }
        return substitute_this_type(&self.store, self.store.union(collected), receiver);
      }
      _ => {}
    }

    let obj = self.expand_ref(obj);
    let ty = match self.store.type_kind(obj) {
      TypeKind::Union(members) => {
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_index_key_with_receiver(member, key_ty, receiver));
        }
        self.store.union(collected)
      }
      TypeKind::Intersection(members) => {
        let mut collected = Vec::new();
        for member in members {
          collected.push(self.member_type_for_index_key_with_receiver(member, key_ty, receiver));
        }
        self.store.intersection(collected)
      }
      TypeKind::Ref { .. } => prim.unknown,
      TypeKind::Object(obj_id) => {
        let shape = self.store.shape(self.store.object(obj_id).shape);
        let mut matches = Vec::new();
        for idx in shape.indexers.iter() {
          if self.indexer_key_matches(idx.key_type, key_ty) {
            matches.push(idx.value_type);
          }
        }
        if matches.is_empty() {
          prim.unknown
        } else if matches.len() == 1 {
          matches[0]
        } else {
          matches.sort_by(|a, b| self.store.type_cmp(*a, *b));
          matches.dedup();
          self.store.union(matches)
        }
      }
      TypeKind::Array { ty, .. } => {
        if self.relate.is_assignable(key_ty, prim.number) {
          ty
        } else {
          prim.unknown
        }
      }
      TypeKind::Tuple(elems) => match self.store.type_kind(key_ty) {
        TypeKind::NumberLiteral(num) => {
          let raw = num.0;
          if raw.fract() != 0.0 || raw < 0.0 {
            return prim.unknown;
          }
          let idx = raw as usize;
          if let Some(elem) = elems.get(idx) {
            let mut ty = if elem.rest {
              self.relate.spread_element_type(elem.ty)
            } else {
              elem.ty
            };
            if elem.optional && !self.relate.options.exact_optional_property_types {
              ty = self.store.union(vec![ty, prim.undefined]);
            }
            ty
          } else if let Some(rest) = elems.iter().find(|elem| elem.rest) {
            self.relate.spread_element_type(rest.ty)
          } else {
            prim.undefined
          }
        }
        _ => {
          if !self.relate.is_assignable(key_ty, prim.number) {
            return prim.unknown;
          }
          let mut members = Vec::new();
          for elem in elems {
            let mut ty = if elem.rest {
              self.relate.spread_element_type(elem.ty)
            } else {
              elem.ty
            };
            if elem.optional && !self.relate.options.exact_optional_property_types {
              ty = self.store.union(vec![ty, prim.undefined]);
            }
            members.push(ty);
          }
          self.store.union(members)
        }
      },
      _ => prim.unknown,
    };

    substitute_this_type(&self.store, ty, receiver)
  }

  fn indexer_key_matches(&self, indexer_key: TypeId, key_ty: TypeId) -> bool {
    let prim = self.store.primitive_ids();
    let key_ty = self.store.canon(key_ty);

    // Index signatures are keyed by JS property keys: string, number, and symbol.
    // For computed member access, model key matching in terms of those key kinds
    // rather than generic type assignability (which can be overly permissive for
    // intersection key types).
    let dummy_name = self.store.intern_name_ref("<index>");

    let mut candidates = Vec::new();
    match self.store.type_kind(key_ty) {
      TypeKind::String | TypeKind::StringLiteral(_) => {
        candidates.push(PropKey::String(dummy_name));
      }
      TypeKind::Number | TypeKind::NumberLiteral(_) => {
        candidates.push(PropKey::Number(0));
      }
      TypeKind::Symbol | TypeKind::UniqueSymbol => {
        candidates.push(PropKey::Symbol(dummy_name));
      }
      TypeKind::Any => {
        candidates.push(PropKey::String(dummy_name));
        candidates.push(PropKey::Number(0));
        candidates.push(PropKey::Symbol(dummy_name));
      }
      _ => {
        // Fall back to probing key kinds via assignability for non-primitive key
        // types (e.g. type parameters).
        if self.relate.is_assignable(key_ty, prim.string) {
          candidates.push(PropKey::String(dummy_name));
        }
        if self.relate.is_assignable(key_ty, prim.number) {
          candidates.push(PropKey::Number(0));
        }
        if self.relate.is_assignable(key_ty, prim.symbol) {
          candidates.push(PropKey::Symbol(dummy_name));
        }
      }
    }

    if candidates.is_empty() {
      return false;
    }

    candidates
      .into_iter()
      .any(|key| crate::type_queries::indexer_accepts_key(&key, indexer_key, &self.store))
  }

  fn object_prop_type(&self, obj: TypeId, key: &str) -> Option<TypeId> {
    let receiver = self.store.canon(obj);
    self.object_prop_type_with_receiver(obj, key, receiver)
  }

  fn object_prop_type_with_receiver(
    &self,
    obj: TypeId,
    key: &str,
    receiver: TypeId,
  ) -> Option<TypeId> {
    let prim = self.store.primitive_ids();
    let obj = self.expand_ref(obj);
    let ty = match self.store.type_kind(obj) {
      TypeKind::Union(members) => {
        let mut tys = Vec::new();
        for member in members {
          let prop_ty = self.object_prop_type_with_receiver(member, key, receiver)?;
          tys.push(prop_ty);
        }
        Some(self.store.union(tys))
      }
      TypeKind::Intersection(members) => {
        let mut tys = Vec::new();
        for member in members {
          if let Some(prop_ty) = self.object_prop_type_with_receiver(member, key, receiver) {
            tys.push(prop_ty);
          }
        }
        if tys.is_empty() {
          None
        } else {
          Some(self.store.intersection(tys))
        }
      }
      TypeKind::Ref { .. } => None,
      TypeKind::Callable { .. } => self.callable_prop_type(obj, key),
      TypeKind::Object(obj_id) => {
        let shape = self.store.shape(self.store.object(obj_id).shape);
        for prop in shape.properties.iter() {
          let matches = match prop.key {
            PropKey::String(name) => self.store.name(name) == key,
            PropKey::Number(num) => num.to_string() == key,
            _ => false,
          };
          if matches {
            let mut ty = prop.data.ty;
            if prop.data.optional {
              ty = self.store.union(vec![ty, prim.undefined]);
            }
            return Some(ty);
          }
        }
        if key == "call" && !shape.call_signatures.is_empty() {
          return Some(self.build_call_method_type(shape.call_signatures.clone()));
        }
        if matches!(key, "apply" | "bind") && !shape.call_signatures.is_empty() {
          return Some(prim.any);
        }
        let key_prop = if let Some(idx) = parse_canonical_index_str(key) {
          PropKey::Number(idx)
        } else {
          PropKey::String(self.store.intern_name_ref(key))
        };
        let mut matches = Vec::new();
        for idxer in shape.indexers.iter() {
          if crate::type_queries::indexer_accepts_key(&key_prop, idxer.key_type, &self.store) {
            matches.push(idxer.value_type);
          }
        }
        if matches.is_empty() {
          None
        } else {
          Some(self.store.union(matches))
        }
      }
      TypeKind::Array { .. } if key == "length" => Some(prim.number),
      TypeKind::Array { ty, .. } => Some(ty),
      _ => None,
    };

    ty.map(|ty| substitute_this_type(&self.store, ty, receiver))
  }

  fn callable_prop_type(&self, obj: TypeId, key: &str) -> Option<TypeId> {
    let prim = self.store.primitive_ids();
    match key {
      "call" => {
        let sigs = callable_signatures(&self.store, obj);
        if sigs.is_empty() {
          None
        } else {
          Some(self.build_call_method_type(sigs))
        }
      }
      "apply" | "bind" => Some(prim.any),
      _ => None,
    }
  }

  fn build_call_method_type(&self, sigs: Vec<SignatureId>) -> TypeId {
    let prim = self.store.primitive_ids();
    let mut overloads = Vec::new();
    for sig_id in sigs {
      let sig = self.store.signature(sig_id);
      let this_arg = sig.this_param.unwrap_or(prim.any);
      let mut params = Vec::with_capacity(sig.params.len() + 1);
      params.push(SigParam {
        name: None,
        ty: this_arg,
        optional: false,
        rest: false,
      });
      params.extend(sig.params.clone());
      let call_sig = Signature {
        params,
        ret: sig.ret,
        type_params: sig.type_params.clone(),
        this_param: None,
      };
      overloads.push(self.store.intern_signature(call_sig));
    }
    overloads.sort();
    overloads.dedup();
    self.store.intern_type(TypeKind::Callable { overloads })
  }

  fn switch_case_falls_through(&self, case: Option<&SwitchCase>) -> bool {
    let Some(case) = case else {
      return false;
    };
    match case.consequent.last() {
      None => true,
      Some(stmt) => match &self.body.stmts[stmt.0 as usize].kind {
        StmtKind::Return(_) | StmtKind::Throw(_) | StmtKind::Break(_) => false,
        _ => true,
      },
    }
  }

  fn apply_switch_narrowing(
    &mut self,
    target: &SwitchDiscriminant,
    test: ExprId,
    env: &mut Env,
  ) -> Option<(TypeId, TypeId)> {
    let (yes, no) = self.switch_case_narrowing_with_type(target, target.ty(), test)?;
    self.apply_switch_result(target, yes, env);
    Some((yes, no))
  }

  fn switch_default_remaining(
    &self,
    target: &SwitchDiscriminant,
    cases: &[SwitchCase],
  ) -> Option<TypeId> {
    let mut remaining = target.ty();
    for case in cases.iter() {
      if let Some(test) = case.test {
        let (_, no) = self.switch_case_narrowing_with_type(target, remaining, test)?;
        remaining = no;
      }
    }
    Some(remaining)
  }

  fn switch_case_narrowing_with_type(
    &self,
    target: &SwitchDiscriminant,
    ty: TypeId,
    test: ExprId,
  ) -> Option<(TypeId, TypeId)> {
    match target {
      SwitchDiscriminant::Ident { .. } => {
        let lit = self.literal_value(test)?;
        Some(narrow_by_literal(ty, &lit, &self.store))
      }
      SwitchDiscriminant::Member {
        path,
        optional_bases,
        ..
      } => {
        let lit = self.literal_value(test)?;
        let (yes, no) = narrow_by_discriminant_path(ty, path, &lit, &self.store, self.ref_expander);
        if optional_bases.is_empty() {
          Some((yes, no))
        } else {
          Some((yes, ty))
        }
      }
      SwitchDiscriminant::Typeof { .. } => match self.literal_value(test) {
        Some(LiteralValue::String(value)) => Some(narrow_by_typeof(ty, &value, &self.store)),
        _ => None,
      },
    }
  }

  fn switch_discriminant_target(
    &self,
    discriminant: ExprId,
    discriminant_ty: TypeId,
    env: &Env,
  ) -> Option<SwitchDiscriminant> {
    match &self.body.exprs[discriminant.0 as usize].kind {
      ExprKind::Unary {
        op: UnaryOp::Typeof,
        expr,
      } => {
        if let Some(binding) = self.ident_binding(*expr) {
          let operand_ty = env
            .get(binding)
            .unwrap_or_else(|| self.expr_types[expr.0 as usize]);
          return Some(SwitchDiscriminant::Typeof {
            name: binding,
            ty: operand_ty,
          });
        }
        None
      }
      ExprKind::Member(_) => self.switch_member_target(discriminant, env),
      ExprKind::Ident(_) => {
        self
          .ident_binding(discriminant)
          .map(|binding| SwitchDiscriminant::Ident {
            name: binding,
            ty: env.get(binding).unwrap_or(discriminant_ty),
          })
      }
      _ => None,
    }
  }

  fn switch_member_target(&self, expr: ExprId, env: &Env) -> Option<SwitchDiscriminant> {
    let (binding, path, root_expr, optional_bases) = self.discriminant_member(expr)?;
    let obj_ty = env
      .get(binding)
      .unwrap_or_else(|| self.expr_types[root_expr.0 as usize]);
    Some(SwitchDiscriminant::Member {
      name: binding,
      path,
      optional_bases,
      ty: obj_ty,
    })
  }

  fn apply_switch_result(&mut self, target: &SwitchDiscriminant, narrowed: TypeId, env: &mut Env) {
    env.set(target.name(), narrowed);
    let prim = self.store.primitive_ids();
    if let SwitchDiscriminant::Member {
      name,
      optional_bases,
      ..
    } = target
    {
      for base_path in optional_bases.iter() {
        if base_path.is_empty() {
          continue;
        }
        let Some(base_ty) = self.object_prop_type_path(narrowed, base_path) else {
          continue;
        };
        let (non_nullish, _) = narrow_non_nullish(base_ty, &self.store);
        if non_nullish == prim.never {
          continue;
        }
        let mut key = FlowKey::root(*name);
        for seg in base_path.iter() {
          key = key.with_segment(seg.clone());
        }
        env.set_path(key, non_nullish);
      }
    }
  }
}
