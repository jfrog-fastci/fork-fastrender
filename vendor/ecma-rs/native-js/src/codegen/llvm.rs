use crate::CompileOptions;
use parse_js::ast::expr::pat::Pat;
use parse_js::ast::expr::{CallArg, Expr};
use parse_js::ast::func::FuncBody;
use parse_js::ast::node::Node;
use parse_js::ast::stmt::decl::{FuncDecl, VarDecl};
use parse_js::ast::stmt::Stmt;
use parse_js::ast::stx::TopLevel;
use parse_js::ast::type_expr::TypeExpr;
use parse_js::loc::Loc;
use parse_js::operator::OperatorName;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::builtins::{recognize_builtin, BuiltinCall};
use super::CodegenError;

const SYNTHETIC_INPUT_FILE: &str = "<input>.ts";

fn llvm_escape_metadata_string(s: &str) -> String {
  // LLVM IR uses a C-like string literal format in metadata nodes.
  s.replace('\\', "\\\\").replace('\"', "\\\"")
}

fn compute_line_starts(source: &str) -> Vec<u32> {
  // `parse-js` locations are UTF-8 byte offsets; keep the mapping in bytes too.
  let mut starts = vec![0u32];
  for (idx, &b) in source.as_bytes().iter().enumerate() {
    if b == b'\n' {
      let next = idx.saturating_add(1);
      let next = next.min(u32::MAX as usize) as u32;
      starts.push(next);
    }
  }
  starts
}

fn line_col_for_offset(line_starts: &[u32], offset: u32) -> (u32, u32) {
  if line_starts.is_empty() {
    return (1, 1);
  }
  // Find the last line start <= offset.
  let idx = line_starts
    .partition_point(|&start| start <= offset)
    .saturating_sub(1);
  let line_start = line_starts[idx];
  let line = (idx as u32).saturating_add(1);
  let col = offset.saturating_sub(line_start).saturating_add(1);
  (line, col)
}

fn split_file_key(file_key: &str) -> (String, Option<String>) {
  let last_sep = file_key.rfind(|c| c == '/' || c == '\\');
  let Some(last_sep) = last_sep else {
    return (file_key.to_string(), None);
  };

  if last_sep + 1 >= file_key.len() {
    return (file_key.to_string(), None);
  }

  let filename = file_key[(last_sep + 1)..].to_string();
  let mut directory = file_key[..last_sep].to_string();
  if directory.is_empty() {
    directory = file_key[..=last_sep].to_string();
  }

  (filename, Some(directory))
}

fn remap_prefix(path: &Path, map: &[(PathBuf, PathBuf)]) -> PathBuf {
  for (from, to) in map {
    if let Ok(rest) = path.strip_prefix(from) {
      return to.join(rest);
    }
  }
  path.to_path_buf()
}

#[derive(Clone, Copy, Debug)]
struct DebugPos {
  line: u32,
  col: u32,
}

struct DebugFile {
  di_file: u32,
  line_starts: Vec<u32>,
  len: u32,
}

struct DebugInfo {
  next_id: u32,
  defs: Vec<String>,

  // Common metadata nodes.
  empty_list: u32,
  dwarf_version_flag: u32,
  debug_info_version_flag: u32,

  // Module-level compilation unit.
  cu: Option<u32>,

  files: HashMap<String, DebugFile>,
  subroutine_types: HashMap<usize, u32>,
}

impl DebugInfo {
  fn new() -> Self {
    let mut this = Self {
      next_id: 0,
      defs: Vec::new(),
      empty_list: 0,
      dwarf_version_flag: 0,
      debug_info_version_flag: 0,
      cu: None,
      files: HashMap::new(),
      subroutine_types: HashMap::new(),
    };

    // Common metadata nodes that we always reference when debug is enabled.
    this.empty_list = this.alloc();
    this.defs.push(format!("!{} = !{{}}", this.empty_list));

    this.dwarf_version_flag = this.alloc();
    this.defs.push(format!(
      "!{} = !{{i32 2, !\"Dwarf Version\", i32 5}}",
      this.dwarf_version_flag
    ));

    this.debug_info_version_flag = this.alloc();
    this.defs.push(format!(
      "!{} = !{{i32 2, !\"Debug Info Version\", i32 3}}",
      this.debug_info_version_flag
    ));

    this
  }

  fn alloc(&mut self) -> u32 {
    let id = self.next_id;
    self.next_id += 1;
    id
  }

  fn ensure_file(&mut self, file_key: &str, di_filename: &str, di_directory: &str, source: &str) -> u32 {
    if let Some(info) = self.files.get(file_key) {
      return info.di_file;
    }

    let di_file = self.alloc();
    let escaped_filename = llvm_escape_metadata_string(di_filename);
    let escaped_directory = llvm_escape_metadata_string(di_directory);
    self.defs.push(format!(
      "!{di_file} = !DIFile(filename: \"{escaped_filename}\", directory: \"{escaped_directory}\")"
    ));

    let line_starts = compute_line_starts(source);
    let len = source.len().min(u32::MAX as usize) as u32;
    self.files.insert(
      file_key.to_string(),
      DebugFile {
        di_file,
        line_starts,
        len,
      },
    );

    di_file
  }

  fn ensure_compile_unit(&mut self, main_file: u32) -> u32 {
    if let Some(cu) = self.cu {
      return cu;
    }

    let cu = self.alloc();
    // The language is mostly cosmetic; pick a DWARF language that debuggers understand well.
    self.defs.push(format!(
      "!{cu} = distinct !DICompileUnit(language: DW_LANG_C_plus_plus, file: !{main_file}, producer: \"native-js\", isOptimized: false, runtimeVersion: 0, emissionKind: LineTablesOnly, enums: !{}, globals: !{}, splitDebugInlining: false, nameTableKind: None)",
      self.empty_list, self.empty_list
    ));
    self.cu = Some(cu);
    cu
  }

  fn set_main_file(&mut self, file_key: &str, di_filename: &str, di_directory: &str, source: &str) {
    let file_id = self.ensure_file(file_key, di_filename, di_directory, source);
    self.ensure_compile_unit(file_id);
  }

  fn file_info(&self, filename: &str) -> Option<&DebugFile> {
    self.files.get(filename)
  }

  fn subroutine_type(&mut self, param_count: usize) -> u32 {
    if let Some(existing) = self.subroutine_types.get(&param_count) {
      return *existing;
    }

    // Debug line tables only need function names/locations, but LLVM's verifier expects a
    // `DISubprogram` to reference a `DISubroutineType`. We use `null` for all types and only match
    // the parameter *arity*.
    let list_id = self.alloc();
    let mut tys = Vec::with_capacity(param_count + 1);
    for _ in 0..=param_count {
      tys.push("null");
    }
    self
      .defs
      .push(format!("!{list_id} = !{{{}}}", tys.join(", ")));

    let ty_id = self.alloc();
    self
      .defs
      .push(format!("!{ty_id} = !DISubroutineType(types: !{list_id})"));

    self.subroutine_types.insert(param_count, ty_id);
    ty_id
  }

  fn new_subprogram(
    &mut self,
    name: &str,
    linkage_name: &str,
    file: u32,
    line: u32,
    param_count: usize,
  ) -> u32 {
    let cu = self.ensure_compile_unit(file);

    let subprogram = self.alloc();
    let name = llvm_escape_metadata_string(name);
    let linkage_name = llvm_escape_metadata_string(linkage_name);
    let ty = self.subroutine_type(param_count);
    self.defs.push(format!(
      "!{subprogram} = distinct !DISubprogram(name: \"{name}\", linkageName: \"{linkage_name}\", scope: !{file}, file: !{file}, line: {line}, type: !{ty}, scopeLine: {line}, flags: DIFlagPrototyped, spFlags: DISPFlagDefinition, unit: !{cu}, retainedNodes: !{})",
      self.empty_list
    ));
    subprogram
  }

  fn new_location(&mut self, line: u32, col: u32, scope: u32) -> u32 {
    let loc = self.alloc();
    self.defs.push(format!(
      "!{loc} = !DILocation(line: {line}, column: {col}, scope: !{scope})"
    ));
    loc
  }

  fn render(&self) -> String {
    let Some(cu) = self.cu else {
      return String::new();
    };

    let mut out = String::new();
    out.push_str("\n!llvm.dbg.cu = !{!");
    out.push_str(&cu.to_string());
    out.push_str("}\n");
    out.push_str("!llvm.module.flags = !{!");
    out.push_str(&self.dwarf_version_flag.to_string());
    out.push_str(", !");
    out.push_str(&self.debug_info_version_flag.to_string());
    out.push_str("}\n");

    for def in &self.defs {
      out.push_str(def);
      out.push('\n');
    }

    out
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ty {
  Number,
  Bool,
  String,
  Null,
  Undefined,
  Void,
}

#[derive(Clone, Debug)]
struct Value {
  ty: Ty,
  ir: String,
}

impl Value {
  fn void() -> Self {
    Self {
      ty: Ty::Void,
      ir: String::new(),
    }
  }
}

fn f64_to_llvm_const(value: f64) -> String {
  format!("0x{:016X}", value.to_bits())
}

/// Signature information for a user-defined function that can be called from generated code.
#[derive(Clone, Debug)]
pub(crate) struct UserFunctionSig {
  /// LLVM symbol name (including the leading `@`).
  pub llvm_name: String,
  pub ret: Ty,
  pub params: Vec<Ty>,
}

#[derive(Default)]
struct StringPool {
  next_id: usize,
  // Map from raw bytes (without null terminator) to (global name, array length including terminator).
  interned: HashMap<Vec<u8>, (String, usize)>,
  defs: Vec<String>,
}

fn llvm_escape_bytes(bytes: &[u8]) -> String {
  let mut out = String::new();
  for &b in bytes {
    if (0x20..=0x7e).contains(&b) && b != b'"' && b != b'\\' {
      out.push(b as char);
    } else {
      out.push('\\');
      out.push_str(&format!("{b:02X}"));
    }
  }
  out
}

impl StringPool {
  fn intern(&mut self, bytes: &[u8]) -> (String, usize) {
    if let Some(existing) = self.interned.get(bytes) {
      return existing.clone();
    }

    let name = format!("@.str{}", self.next_id);
    self.next_id += 1;

    let mut with_null = bytes.to_vec();
    with_null.push(0);
    let len = with_null.len();
    let escaped = llvm_escape_bytes(&with_null);
    self.defs.push(format!(
      "{name} = private unnamed_addr constant [{len} x i8] c\"{escaped}\", align 1"
    ));

    self.interned.insert(bytes.to_vec(), (name.clone(), len));

    (name, len)
  }
}

struct Codegen {
  opts: CompileOptions,
  strings: StringPool,
  function_sigs: HashMap<String, FunctionSig>,
  function_llvm_names: HashMap<String, String>,
  function_defs: Vec<String>,
  dbg: Option<DebugInfo>,
  dbg_current_file: Option<String>,
  dbg_current_scope: Option<u32>,
  dbg_current_pos: Option<DebugPos>,
  dbg_main_subprogram: Option<u32>,
  tmp_counter: usize,
  block_counter: usize,
  loop_stack: Vec<LoopContext>,
  vars: HashMap<String, (Ty, String)>,
  body: Vec<String>,
  /// If `Some`, we're compiling a user-defined function body and this is its return type.
  current_return_ty: Option<Ty>,
  /// Whether the current basic block is terminated (e.g. by `br`, `ret`, or `unreachable`).
  block_terminated: bool,
}

#[derive(Clone, Debug)]
struct LoopContext {
  break_label: String,
  continue_label: String,
}

impl Codegen {
  fn new(opts: CompileOptions) -> Self {
    Self {
      dbg: opts.debug.then_some(DebugInfo::new()),
      opts,
      strings: StringPool::default(),
      function_sigs: HashMap::new(),
      function_llvm_names: HashMap::new(),
      function_defs: Vec::new(),
      dbg_current_file: None,
      dbg_current_scope: None,
      dbg_current_pos: None,
      dbg_main_subprogram: None,
      tmp_counter: 0,
      block_counter: 0,
      loop_stack: Vec::new(),
      vars: HashMap::new(),
      body: Vec::new(),
      current_return_ty: None,
      block_terminated: false,
    }
  }

  fn debug_file_key_fields(&self, file_key: &str) -> (String, String, String) {
    let remapped = remap_prefix(Path::new(file_key), &self.opts.debug_path_prefix_map)
      .to_string_lossy()
      .into_owned();

    if remapped == SYNTHETIC_INPUT_FILE {
      return (remapped.clone(), remapped, String::new());
    }

    let (filename, parent) = split_file_key(&remapped);
    let directory = match parent {
      Some(dir) => dir,
      None => std::env::current_dir()
        .map(|cwd| remap_prefix(&cwd, &self.opts.debug_path_prefix_map))
        .map(|cwd| cwd.to_string_lossy().into_owned())
        .unwrap_or_default(),
    };
    (remapped, filename, directory)
  }

  fn tmp(&mut self) -> String {
    let name = format!("%t{}", self.tmp_counter);
    self.tmp_counter += 1;
    name
  }

  fn fresh_block(&mut self, prefix: &str) -> String {
    let name = format!("{prefix}{}", self.block_counter);
    self.block_counter += 1;
    name
  }

  fn emit(&mut self, line: impl Into<String>) {
    let mut line = line.into();
    let trimmed = line.trim();

    // Attach debug locations to executable instructions (not basic block labels).
    if !trimmed.is_empty() && !trimmed.ends_with(':') {
      if let (Some(dbg), Some(scope), Some(pos)) = (
        self.dbg.as_mut(),
        self.dbg_current_scope,
        self.dbg_current_pos,
      ) {
        let loc = dbg.new_location(pos.line, pos.col, scope);
        line.push_str(&format!(", !dbg !{loc}"));
      }
    }

    let trimmed = line.trim();
    // Basic block labels always end with `:`.
    if trimmed.ends_with(':') {
      self.block_terminated = false;
    } else {
      let inst = trimmed.trim_start();
      if inst.starts_with("br ") || inst.starts_with("ret ") || inst.starts_with("unreachable") {
        self.block_terminated = true;
      }
    }
    self.body.push(line);
  }

  fn set_debug_main_file(&mut self, filename: &str, source: &str) {
    if self.dbg.is_none() {
      return;
    };
    let (key, di_filename, di_directory) = self.debug_file_key_fields(filename);
    let dbg = self.dbg.as_mut().expect("checked dbg above");
    dbg.set_main_file(&key, &di_filename, &di_directory, source);
    self.dbg_current_file = Some(key);
  }

  fn set_debug_current_file(&mut self, filename: &str, source: &str) {
    if self.dbg.is_none() {
      return;
    };
    let (key, di_filename, di_directory) = self.debug_file_key_fields(filename);
    let dbg = self.dbg.as_mut().expect("checked dbg above");
    dbg.ensure_file(&key, &di_filename, &di_directory, source);
    self.dbg_current_file = Some(key);
  }

  fn dbg_current_file_id(&self) -> Option<u32> {
    let dbg = self.dbg.as_ref()?;
    let name = self.dbg_current_file.as_deref()?;
    Some(dbg.file_info(name)?.di_file)
  }

  fn dbg_pos_for_loc(&self, loc: Loc) -> Option<DebugPos> {
    let dbg = self.dbg.as_ref()?;
    let name = self.dbg_current_file.as_deref()?;
    let info = dbg.file_info(name)?;

    let offset = loc.0.min(info.len as usize) as u32;
    let (line, col) = line_col_for_offset(&info.line_starts, offset);
    Some(DebugPos { line, col })
  }

  fn llvm_type_of(ty: Ty) -> &'static str {
    match ty {
      Ty::Number => "double",
      Ty::Bool => "i1",
      Ty::String => "ptr",
      Ty::Null | Ty::Undefined => "i1",
      Ty::Void => "void",
    }
  }

  fn llvm_align_of(ty: Ty) -> u32 {
    match ty {
      Ty::Number => 8,
      Ty::Bool => 1,
      Ty::String => 8,
      Ty::Null | Ty::Undefined => 1,
      Ty::Void => 1,
    }
  }

  fn emit_alloca(&mut self, ty: Ty, loc: Loc) -> Result<String, CodegenError> {
    if ty == Ty::Void {
      return Err(CodegenError::TypeError {
        message: "cannot allocate storage for void".to_string(),
        loc,
      });
    }
    let llvm_ty = Self::llvm_type_of(ty);
    let align = Self::llvm_align_of(ty);
    let out = self.tmp();
    self.emit(format!("  {out} = alloca {llvm_ty}, align {align}"));
    Ok(out)
  }

  fn emit_store(&mut self, ty: Ty, value_ir: &str, ptr_ir: &str) -> Result<(), CodegenError> {
    if ty == Ty::Void {
      return Err(CodegenError::TypeError {
        message: "cannot store a void value".to_string(),
        loc: Loc(0, 0),
      });
    }
    let llvm_ty = Self::llvm_type_of(ty);
    let align = Self::llvm_align_of(ty);
    self.emit(format!(
      "  store {llvm_ty} {value_ir}, ptr {ptr_ir}, align {align}"
    ));
    Ok(())
  }

  fn emit_load(&mut self, ty: Ty, ptr_ir: &str) -> Result<String, CodegenError> {
    if ty == Ty::Void {
      return Err(CodegenError::TypeError {
        message: "cannot load a void value".to_string(),
        loc: Loc(0, 0),
      });
    }
    let llvm_ty = Self::llvm_type_of(ty);
    let align = Self::llvm_align_of(ty);
    let out = self.tmp();
    self.emit(format!(
      "  {out} = load {llvm_ty}, ptr {ptr_ir}, align {align}"
    ));
    Ok(out)
  }

  fn emit_string_ptr(&mut self, bytes: &[u8]) -> String {
    let (global, len) = self.strings.intern(bytes);
    let tmp = self.tmp();
    self.emit(format!(
      "  {tmp} = getelementptr inbounds [{len} x i8], ptr {global}, i64 0, i64 0"
    ));
    tmp
  }

  fn emit_print_value(&mut self, value: Value) -> Result<(), CodegenError> {
    match value.ty {
      Ty::Number => {
        self.emit_print_number_inline(&value.ir)?;
        let empty = self.emit_string_ptr(b"");
        self.emit(format!("  notail call i32 @puts(ptr {empty})"));
        Ok(())
      }
      Ty::Bool => {
        let true_ptr = self.emit_string_ptr(b"true");
        let false_ptr = self.emit_string_ptr(b"false");
        let sel = self.tmp();
        self.emit(format!(
          "  {sel} = select i1 {}, ptr {true_ptr}, ptr {false_ptr}",
          value.ir
        ));
        self.emit(format!("  notail call i32 @puts(ptr {sel})"));
        Ok(())
      }
      Ty::String => {
        self.emit(format!("  notail call i32 @puts(ptr {})", value.ir));
        Ok(())
      }
      Ty::Null => {
        let null_ptr = self.emit_string_ptr(b"null");
        self.emit(format!("  notail call i32 @puts(ptr {null_ptr})"));
        Ok(())
      }
      Ty::Undefined => {
        let undef_ptr = self.emit_string_ptr(b"undefined");
        self.emit(format!("  notail call i32 @puts(ptr {undef_ptr})"));
        Ok(())
      }
      Ty::Void => Err(CodegenError::TypeError {
        message: "cannot print a void expression".to_string(),
        loc: Loc(0, 0),
      }),
    }
  }

  fn emit_print_value_inline(&mut self, value: Value) -> Result<(), CodegenError> {
    match value.ty {
      Ty::Number => self.emit_print_number_inline(&value.ir),
      Ty::Bool => {
        let true_ptr = self.emit_string_ptr(b"true");
        let false_ptr = self.emit_string_ptr(b"false");
        let sel = self.tmp();
        self.emit(format!(
          "  {sel} = select i1 {}, ptr {true_ptr}, ptr {false_ptr}",
          value.ir
        ));
        let fmt = self.emit_string_ptr(b"%s");
        self.emit(format!(
          "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {sel})"
        ));
        Ok(())
      }
      Ty::String => {
        let fmt = self.emit_string_ptr(b"%s");
        self.emit(format!(
          "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {})",
          value.ir
        ));
        Ok(())
      }
      Ty::Null => {
        let fmt = self.emit_string_ptr(b"%s");
        let null_ptr = self.emit_string_ptr(b"null");
        self.emit(format!(
          "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {null_ptr})"
        ));
        Ok(())
      }
      Ty::Undefined => {
        let fmt = self.emit_string_ptr(b"%s");
        let undef_ptr = self.emit_string_ptr(b"undefined");
        self.emit(format!(
          "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {undef_ptr})"
        ));
        Ok(())
      }
      Ty::Void => Err(CodegenError::TypeError {
        message: "cannot print a void expression".to_string(),
        loc: Loc(0, 0),
      }),
    }
  }

  fn emit_print_number_inline(&mut self, value_ir: &str) -> Result<(), CodegenError> {
    let is_nan = self.tmp();
    self.emit(format!(
      "  {is_nan} = fcmp uno double {value_ir}, {value_ir}"
    ));

    let nan = self.fresh_block("print.nan");
    let not_nan = self.fresh_block("print.not_nan");
    self.emit(format!("  br i1 {is_nan}, label %{nan}, label %{not_nan}"));

    let cont = self.fresh_block("print.num.cont");

    self.emit(format!("{nan}:"));
    {
      let fmt = self.emit_string_ptr(b"%s");
      let nan_ptr = self.emit_string_ptr(b"NaN");
      self.emit(format!(
        "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {nan_ptr})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{not_nan}:"));
    let is_pos_inf = self.tmp();
    self.emit(format!(
      "  {is_pos_inf} = fcmp oeq double {value_ir}, {}",
      f64_to_llvm_const(f64::INFINITY)
    ));

    let pos_inf = self.fresh_block("print.pos_inf");
    let not_pos_inf = self.fresh_block("print.not_pos_inf");
    self.emit(format!(
      "  br i1 {is_pos_inf}, label %{pos_inf}, label %{not_pos_inf}"
    ));

    self.emit(format!("{pos_inf}:"));
    {
      let fmt = self.emit_string_ptr(b"%s");
      let inf_ptr = self.emit_string_ptr(b"Infinity");
      self.emit(format!(
        "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {inf_ptr})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{not_pos_inf}:"));
    let is_neg_inf = self.tmp();
    self.emit(format!(
      "  {is_neg_inf} = fcmp oeq double {value_ir}, {}",
      f64_to_llvm_const(f64::NEG_INFINITY)
    ));

    let neg_inf = self.fresh_block("print.neg_inf");
    let finite = self.fresh_block("print.finite");
    self.emit(format!(
      "  br i1 {is_neg_inf}, label %{neg_inf}, label %{finite}"
    ));

    self.emit(format!("{neg_inf}:"));
    {
      let fmt = self.emit_string_ptr(b"%s");
      let inf_ptr = self.emit_string_ptr(b"-Infinity");
      self.emit(format!(
        "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {inf_ptr})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{finite}:"));
    {
      // `%.15g` matches JS `Number#toString` reasonably well for debugging: it avoids the very
      // low default precision of `%g` while still keeping common values like `0.1` and `0.3`
      // readable.
      let fmt = self.emit_string_ptr(b"%.15g");
      self.emit(format!(
        "  notail call i32 (ptr, ...) @printf(ptr {fmt}, double {value_ir})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{cont}:"));
    Ok(())
  }

  fn emit_strcmp_eq(&mut self, left: &str, right: &str) -> Result<String, CodegenError> {
    let cmp = self.tmp();
    self.emit(format!(
      "  {cmp} = notail call i32 @strcmp(ptr {left}, ptr {right})"
    ));
    let out = self.tmp();
    self.emit(format!("  {out} = icmp eq i32 {cmp}, 0"));
    Ok(out)
  }

  fn emit_print_log_call(&mut self, args: &[Node<CallArg>]) -> Result<(), CodegenError> {
    if args.is_empty() {
      let empty = self.emit_string_ptr(b"");
      self.emit(format!("  notail call i32 @puts(ptr {empty})"));
      return Ok(());
    }

    for (idx, arg) in args.iter().enumerate() {
      if arg.stx.spread {
        return Err(CodegenError::UnsupportedExpr { loc: arg.loc });
      }
      let v = self.compile_expr(&arg.stx.value)?;
      self.emit_print_value_inline(v)?;
      if idx + 1 != args.len() {
        let space = self.emit_string_ptr(b" ");
        self.emit(format!("  notail call i32 (ptr, ...) @printf(ptr {space})"));
      }
    }

    let empty = self.emit_string_ptr(b"");
    self.emit(format!("  notail call i32 @puts(ptr {empty})"));
    Ok(())
  }

  fn emit_truthy_to_bool(&mut self, value: Value, loc: Loc) -> Result<String, CodegenError> {
    match value.ty {
      Ty::Bool => Ok(value.ir),
      Ty::Number => {
        // JS truthiness: `0`, `-0`, and `NaN` are falsy; other numbers are truthy.
        let out = self.tmp();
        self.emit(format!(
          "  {out} = fcmp one double {}, {}",
          value.ir,
          f64_to_llvm_const(0.0)
        ));
        Ok(out)
      }
      Ty::String => {
        // JS truthiness: the empty string is falsy; all other strings are truthy.
        let first = self.tmp();
        self.emit(format!("  {first} = load i8, ptr {}, align 1", value.ir));
        let out = self.tmp();
        self.emit(format!("  {out} = icmp ne i8 {first}, 0"));
        Ok(out)
      }
      Ty::Null | Ty::Undefined => Ok("0".to_string()),
      Ty::Void => Err(CodegenError::TypeError {
        message: "cannot use a void expression as a condition".to_string(),
        loc,
      }),
    }
  }

  fn compile_var_decl(&mut self, decl: &Node<VarDecl>) -> Result<(), CodegenError> {
    for declarator in &decl.stx.declarators {
      let name = match declarator.pattern.stx.pat.stx.as_ref() {
        Pat::Id(id) => id.stx.name.clone(),
        _ => {
          return Err(CodegenError::UnsupportedStmt {
            loc: declarator.pattern.loc,
          });
        }
      };

      let value = if let Some(init) = declarator.initializer.as_ref() {
        self.compile_expr(init)?
      } else {
        Value {
          ty: Ty::Undefined,
          ir: "0".to_string(),
        }
      };

      let slot = self.emit_alloca(value.ty, declarator.pattern.loc)?;
      let store_val = match value.ty {
        Ty::Null | Ty::Undefined => "0",
        _ => value.ir.as_str(),
      };
      self.emit_store(value.ty, store_val, &slot)?;
      self.vars.insert(name, (value.ty, slot));
    }
    Ok(())
  }

  fn infer_expr_ty(&self, expr: &Node<Expr>) -> Result<Ty, CodegenError> {
    match expr.stx.as_ref() {
      Expr::LitNum(_) => Ok(Ty::Number),
      Expr::LitBool(_) => Ok(Ty::Bool),
      Expr::LitNull(_) => Ok(Ty::Null),
      Expr::LitStr(_) => Ok(Ty::String),
      Expr::Id(id) => {
        if let Some((ty, _)) = self.vars.get(id.stx.name.as_str()) {
          return Ok(*ty);
        }
        match id.stx.name.as_str() {
          "undefined" => Ok(Ty::Undefined),
          "NaN" | "Infinity" => Ok(Ty::Number),
          _ => Err(CodegenError::UnsupportedExpr { loc: expr.loc }),
        }
      }
      Expr::Binary(bin) => match bin.stx.operator {
        OperatorName::Assignment => self.infer_expr_ty(&bin.stx.right),
        OperatorName::AssignmentAddition
        | OperatorName::Addition
        | OperatorName::Subtraction
        | OperatorName::Multiplication
        | OperatorName::Division
        | OperatorName::Remainder => Ok(Ty::Number),
        OperatorName::StrictEquality
        | OperatorName::StrictInequality
        | OperatorName::LessThan
        | OperatorName::LessThanOrEqual
        | OperatorName::GreaterThan
        | OperatorName::GreaterThanOrEqual
        | OperatorName::LogicalAnd
        | OperatorName::LogicalOr => Ok(Ty::Bool),
        other => Err(CodegenError::UnsupportedOperator {
          op: other,
          loc: bin.loc,
        }),
      },
      Expr::Unary(unary) => match unary.stx.operator {
        OperatorName::UnaryNegation | OperatorName::UnaryPlus => Ok(Ty::Number),
        OperatorName::LogicalNot => Ok(Ty::Bool),
        other => Err(CodegenError::UnsupportedOperator {
          op: other,
          loc: unary.loc,
        }),
      },
      Expr::Call(call) => {
        if call.stx.optional_chaining {
          return Err(CodegenError::UnsupportedExpr { loc: call.loc });
        }

        if let Some(builtin) = recognize_builtin(call) {
          match builtin {
            BuiltinCall::Print { .. }
            | BuiltinCall::Assert { .. }
            | BuiltinCall::Panic { .. }
            | BuiltinCall::Trap => Ok(Ty::Void),
          }
        } else {
          let callee = match call.stx.callee.stx.as_ref() {
            Expr::Id(id) => id.stx.name.as_str(),
            _ => return Err(CodegenError::UnsupportedExpr { loc: call.stx.callee.loc }),
          };
          let sig = self
            .function_sigs
            .get(callee)
            .ok_or_else(|| CodegenError::TypeError {
              message: format!("call to unknown function `{callee}`"),
              loc: call.loc,
            })?;
          Ok(sig.ret)
        }
      }
      Expr::Cond(cond) => {
        let cons = self.infer_expr_ty(&cond.stx.consequent)?;
        let alt = self.infer_expr_ty(&cond.stx.alternate)?;
        if cons != alt {
          return Err(CodegenError::TypeError {
            message: format!(
              "conditional operator type mismatch: expected {cons:?}, got {alt:?}"
            ),
            loc: expr.loc,
          });
        }
        Ok(cons)
      }
      _ => Err(CodegenError::UnsupportedExpr { loc: expr.loc }),
    }
  }

  fn compile_stmt(&mut self, stmt: &Node<Stmt>) -> Result<(), CodegenError> {
    let saved_dbg_pos = self.dbg_current_pos;
    self.dbg_current_pos = self.dbg_pos_for_loc(stmt.loc);

    let result = (|| {
      // Never emit instructions after a terminator. If we do, LLVM will reject the IR.
      // Instead, start a fresh (unreachable) basic block.
      if self.block_terminated {
        let cont = self.fresh_block("after.term");
        self.emit(format!("{cont}:"));
      }

      match stmt.stx.as_ref() {
        Stmt::Block(block) => {
          for stmt in &block.stx.body {
            self.compile_stmt(stmt)?;
          }
          Ok(())
        }
        Stmt::Empty(_) => Ok(()),
        Stmt::Expr(expr_stmt) => {
          let _ = self.compile_expr(&expr_stmt.stx.expr)?;
          Ok(())
        }
        Stmt::If(if_stmt) => {
          let cond = self.compile_expr(&if_stmt.stx.test)?;
          let cond_bool = self.emit_truthy_to_bool(cond, if_stmt.stx.test.loc)?;

          let then_label = self.fresh_block("if.then");
          let else_label = self.fresh_block("if.else");
          let end_label = self.fresh_block("if.end");

          let false_label = if if_stmt.stx.alternate.is_some() {
            else_label.as_str()
          } else {
            end_label.as_str()
          };

          self.emit(format!(
            "  br i1 {}, label %{then_label}, label %{false_label}",
            cond_bool
          ));

          self.emit(format!("{then_label}:"));
          self.compile_stmt(&if_stmt.stx.consequent)?;
          if !self.block_terminated {
            self.emit(format!("  br label %{end_label}"));
          }

          if let Some(alt) = if_stmt.stx.alternate.as_ref() {
            self.emit(format!("{else_label}:"));
            self.compile_stmt(alt)?;
            if !self.block_terminated {
              self.emit(format!("  br label %{end_label}"));
            }
          }

          self.emit(format!("{end_label}:"));
          Ok(())
        }
        Stmt::While(while_stmt) => {
          let cond_label = self.fresh_block("while.cond");
          let body_label = self.fresh_block("while.body");
          let end_label = self.fresh_block("while.end");

          self.emit(format!("  br label %{cond_label}"));

          self.emit(format!("{cond_label}:"));
          let cond = self.compile_expr(&while_stmt.stx.condition)?;
          let cond_bool = self.emit_truthy_to_bool(cond, while_stmt.stx.condition.loc)?;
          self.emit(format!(
            "  br i1 {}, label %{body_label}, label %{end_label}",
            cond_bool
          ));

          self.emit(format!("{body_label}:"));
          self.loop_stack.push(LoopContext {
            break_label: end_label.clone(),
            continue_label: cond_label.clone(),
          });
          self.compile_stmt(&while_stmt.stx.body)?;
          self.loop_stack.pop();
          if !self.block_terminated {
            self.emit(format!("  br label %{cond_label}"));
          }

          self.emit(format!("{end_label}:"));
          Ok(())
        }
        Stmt::DoWhile(do_while_stmt) => {
          // Minimal `do { body } while (cond);` support.
          let body_label = self.fresh_block("do.body");
          let cond_label = self.fresh_block("do.cond");
          let end_label = self.fresh_block("do.end");

          self.emit(format!("  br label %{body_label}"));

          self.emit(format!("{body_label}:"));
          self.loop_stack.push(LoopContext {
            break_label: end_label.clone(),
            continue_label: cond_label.clone(),
          });
          self.compile_stmt(&do_while_stmt.stx.body)?;
          self.loop_stack.pop();
          if !self.block_terminated {
            self.emit(format!("  br label %{cond_label}"));
          }

          self.emit(format!("{cond_label}:"));
          let cond = self.compile_expr(&do_while_stmt.stx.condition)?;
          let cond_bool = self.emit_truthy_to_bool(cond, do_while_stmt.stx.condition.loc)?;
          self.emit(format!(
            "  br i1 {cond_bool}, label %{body_label}, label %{end_label}"
          ));

          self.emit(format!("{end_label}:"));
          Ok(())
        }
        Stmt::Break(brk) => {
          if brk.stx.label.is_some() {
            return Err(CodegenError::UnsupportedStmt { loc: brk.loc });
          }
          let Some(ctx) = self.loop_stack.last() else {
            return Err(CodegenError::TypeError {
              message: "`break` is only supported inside loops in this backend".to_string(),
              loc: brk.loc,
            });
          };
          self.emit(format!("  br label %{}", ctx.break_label));
          Ok(())
        }
        Stmt::Continue(cont) => {
          if cont.stx.label.is_some() {
            return Err(CodegenError::UnsupportedStmt { loc: cont.loc });
          }
          let Some(ctx) = self.loop_stack.last() else {
            return Err(CodegenError::TypeError {
              message: "`continue` is only supported inside loops in this backend".to_string(),
              loc: cont.loc,
            });
          };
          self.emit(format!("  br label %{}", ctx.continue_label));
          Ok(())
        }
        Stmt::ForTriple(for_stmt) => {
          // Minimal `for(init; cond; post) { body }` support.
          //
          // Note: this emitter does not model lexical scoping differences between `var`/`let`/`const`;
          // bindings declared in the loop initializer will be visible after the loop as well.
          match &for_stmt.stx.init {
            parse_js::ast::stmt::ForTripleStmtInit::None => {}
            parse_js::ast::stmt::ForTripleStmtInit::Expr(expr) => {
              let _ = self.compile_expr(expr)?;
            }
            parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) => {
              self.compile_var_decl(decl)?;
            }
          }

          let cond_label = self.fresh_block("for.cond");
          let body_label = self.fresh_block("for.body");
          let post_label = self.fresh_block("for.post");
          let end_label = self.fresh_block("for.end");

          self.emit(format!("  br label %{cond_label}"));

          self.emit(format!("{cond_label}:"));
          if let Some(cond) = for_stmt.stx.cond.as_ref() {
            let cond_v = self.compile_expr(cond)?;
            let cond_bool = self.emit_truthy_to_bool(cond_v, cond.loc)?;
            self.emit(format!(
              "  br i1 {cond_bool}, label %{body_label}, label %{end_label}"
            ));
          } else {
            // `for (;;)` is an infinite loop.
            self.emit(format!("  br label %{body_label}"));
          }

          self.emit(format!("{body_label}:"));
          self.loop_stack.push(LoopContext {
            break_label: end_label.clone(),
            continue_label: post_label.clone(),
          });
          for stmt in &for_stmt.stx.body.stx.body {
            self.compile_stmt(stmt)?;
          }
          self.loop_stack.pop();
          if !self.block_terminated {
            self.emit(format!("  br label %{post_label}"));
          }

          self.emit(format!("{post_label}:"));
          if let Some(post) = for_stmt.stx.post.as_ref() {
            let _ = self.compile_expr(post)?;
          }
          if !self.block_terminated {
            self.emit(format!("  br label %{cond_label}"));
          }

          self.emit(format!("{end_label}:"));
          Ok(())
        }
        Stmt::Return(ret) => {
          let Some(expected) = self.current_return_ty else {
            return Err(CodegenError::TypeError {
              message: "`return` is not allowed at the top level".to_string(),
              loc: ret.loc,
            });
          };

          match (expected, ret.stx.value.as_ref()) {
            (Ty::Void, None) => {
              self.emit("  ret void".to_string());
              Ok(())
            }
            (Ty::Void, Some(_)) => Err(CodegenError::TypeError {
              message: "cannot return a value from a `void` function".to_string(),
              loc: ret.loc,
            }),
            (expected, Some(expr)) => {
              let value = self.compile_expr(expr)?;
              if value.ty == Ty::Void {
                return Err(CodegenError::TypeError {
                  message: "cannot return a void expression".to_string(),
                  loc: expr.loc,
                });
              }
              if value.ty != expected {
                return Err(CodegenError::TypeError {
                  message: format!(
                    "return type mismatch: expected {expected:?}, got {got:?}",
                    got = value.ty
                  ),
                  loc: expr.loc,
                });
              }

              let llvm_ty = Self::llvm_type_of(expected);
              let value_ir = match expected {
                Ty::Null | Ty::Undefined => "0".to_string(),
                _ => value.ir,
              };
              self.emit(format!("  ret {llvm_ty} {value_ir}"));
              Ok(())
            }
            (expected, None) => Err(CodegenError::TypeError {
              message: format!("missing return value for function returning {expected:?}"),
              loc: ret.loc,
            }),
          }
        }
        // `export { ... }` without a `from` clause is a runtime no-op. Allow it so callers can add
        // `export {};` as a module marker without requiring project compilation.
        Stmt::ExportList(export) => {
          if export.stx.from.is_some() {
            Err(CodegenError::UnsupportedStmt { loc: stmt.loc })
          } else {
            Ok(())
          }
        }
        // Top-level function declarations are compiled separately (hoisted). We don't model nested
        // function declarations in the minimal emitter.
        Stmt::FunctionDecl(_) => Ok(()),
        Stmt::VarDecl(decl) => {
          self.compile_var_decl(decl)?;
          Ok(())
        }
        _ => Err(CodegenError::UnsupportedStmt { loc: stmt.loc }),
      }
    })();

    self.dbg_current_pos = saved_dbg_pos;
    result
  }

  fn compile_expr(&mut self, expr: &Node<Expr>) -> Result<Value, CodegenError> {
    let saved_dbg_pos = self.dbg_current_pos;
    self.dbg_current_pos = self.dbg_pos_for_loc(expr.loc);

    let result = (|| match expr.stx.as_ref() {
      Expr::LitNum(num) => Ok(Value {
        ty: Ty::Number,
        ir: f64_to_llvm_const(num.stx.value.0),
      }),
      Expr::LitBool(b) => Ok(Value {
        ty: Ty::Bool,
        ir: if b.stx.value { "1" } else { "0" }.to_string(),
      }),
      Expr::LitNull(_) => Ok(Value {
        ty: Ty::Null,
        ir: String::new(),
      }),
      Expr::LitStr(s) => {
        let ptr = self.emit_string_ptr(s.stx.value.as_bytes());
        Ok(Value {
          ty: Ty::String,
          ir: ptr,
        })
      }
      Expr::Id(id) => match id.stx.name.as_str() {
        name => {
          if let Some((ty, slot)) = self.vars.get(name).cloned() {
            match ty {
              Ty::Null | Ty::Undefined => {
                return Ok(Value {
                  ty,
                  ir: "0".to_string(),
                });
              }
              _ => {
                let loaded = self.emit_load(ty, &slot)?;
                return Ok(Value { ty, ir: loaded });
              }
            }
          }

          match name {
            "undefined" => Ok(Value {
              ty: Ty::Undefined,
              ir: String::new(),
            }),
            "NaN" => Ok(Value {
              ty: Ty::Number,
              ir: f64_to_llvm_const(f64::NAN),
            }),
            "Infinity" => Ok(Value {
              ty: Ty::Number,
              ir: f64_to_llvm_const(f64::INFINITY),
            }),
            _ => Err(CodegenError::UnsupportedExpr { loc: expr.loc }),
          }
        }
      },
      Expr::Cond(cond) => {
        // `test ? consequent : alternate`
        //
        // Keep this implementation minimal but semantically correct (only the chosen branch is
        // evaluated).
        let out_ty = self.infer_expr_ty(expr)?;
        let test = self.compile_expr(&cond.stx.test)?;
        let test_bool = self.emit_truthy_to_bool(test, cond.stx.test.loc)?;

        let then_label = self.fresh_block("cond.then");
        let else_label = self.fresh_block("cond.else");
        let cont_label = self.fresh_block("cond.cont");

        // Allocate the output slot *before* emitting the branch terminator, otherwise the
        // instruction would appear after a terminator and the LLVM parser would reject the IR.
        let result_slot = if out_ty == Ty::Void {
          None
        } else {
          Some(self.emit_alloca(out_ty, cond.loc)?)
        };

        self.emit(format!(
          "  br i1 {test_bool}, label %{then_label}, label %{else_label}"
        ));

        if out_ty == Ty::Void {
          self.emit(format!("{then_label}:"));
          let _ = self.compile_expr(&cond.stx.consequent)?;
          if !self.block_terminated {
            self.emit(format!("  br label %{cont_label}"));
          }

          self.emit(format!("{else_label}:"));
          let _ = self.compile_expr(&cond.stx.alternate)?;
          if !self.block_terminated {
            self.emit(format!("  br label %{cont_label}"));
          }

          self.emit(format!("{cont_label}:"));
          Ok(Value::void())
        } else {
          let result_slot = result_slot.expect("non-void conditional allocates a slot");

          self.emit(format!("{then_label}:"));
          let then_v = self.compile_expr(&cond.stx.consequent)?;
          if then_v.ty != out_ty {
            return Err(CodegenError::TypeError {
              message: format!(
                "conditional consequent type mismatch: expected {out_ty:?}, got {got:?}",
                got = then_v.ty
              ),
              loc: cond.stx.consequent.loc,
            });
          }
          let then_store = match then_v.ty {
            Ty::Null | Ty::Undefined => "0".to_string(),
            _ => then_v.ir,
          };
          self.emit_store(out_ty, &then_store, &result_slot)?;
          if !self.block_terminated {
            self.emit(format!("  br label %{cont_label}"));
          }

          self.emit(format!("{else_label}:"));
          let else_v = self.compile_expr(&cond.stx.alternate)?;
          if else_v.ty != out_ty {
            return Err(CodegenError::TypeError {
              message: format!(
                "conditional alternate type mismatch: expected {out_ty:?}, got {got:?}",
                got = else_v.ty
              ),
              loc: cond.stx.alternate.loc,
            });
          }
          let else_store = match else_v.ty {
            Ty::Null | Ty::Undefined => "0".to_string(),
            _ => else_v.ir,
          };
          self.emit_store(out_ty, &else_store, &result_slot)?;
          if !self.block_terminated {
            self.emit(format!("  br label %{cont_label}"));
          }

          self.emit(format!("{cont_label}:"));
          let loaded = self.emit_load(out_ty, &result_slot)?;
          Ok(Value { ty: out_ty, ir: loaded })
        }
      }

      Expr::Binary(bin) => {
        match bin.stx.operator {
          OperatorName::Assignment => {
            let target = match bin.stx.left.stx.as_ref() {
              Expr::IdPat(id) => id.stx.name.as_str(),
              _ => {
                return Err(CodegenError::TypeError {
                  message: "invalid assignment target".to_string(),
                  loc: bin.stx.left.loc,
                });
              }
            };

            let rhs = self.compile_expr(&bin.stx.right)?;
            if rhs.ty == Ty::Void {
              return Err(CodegenError::TypeError {
                message: "cannot assign a void expression".to_string(),
                loc: bin.stx.right.loc,
              });
            }

            if let Some((existing_ty, existing_slot)) = self.vars.get(target).cloned() {
              if existing_ty == rhs.ty {
                let store_val = match rhs.ty {
                  Ty::Null | Ty::Undefined => "0",
                  _ => rhs.ir.as_str(),
                };
                self.emit_store(rhs.ty, store_val, &existing_slot)?;
              } else {
                // The minimal `parse-js`-driven emitter doesn't typecheck; allow the binding's
                // type to change by allocating a fresh slot and updating the map.
                let new_slot = self.emit_alloca(rhs.ty, bin.loc)?;
                let store_val = match rhs.ty {
                  Ty::Null | Ty::Undefined => "0",
                  _ => rhs.ir.as_str(),
                };
                self.emit_store(rhs.ty, store_val, &new_slot)?;
                self.vars.insert(target.to_string(), (rhs.ty, new_slot));
              }
            } else {
              return Err(CodegenError::TypeError {
                message: format!("assignment to undeclared variable `{target}`"),
                loc: bin.loc,
              });
            }

            Ok(rhs)
          }
          OperatorName::AssignmentAddition => {
            let target = match bin.stx.left.stx.as_ref() {
              Expr::IdPat(id) => id.stx.name.as_str(),
              _ => {
                return Err(CodegenError::TypeError {
                  message: "invalid assignment target".to_string(),
                  loc: bin.stx.left.loc,
                });
              }
            };

            let (lhs_ty, lhs_slot) = self.vars.get(target).cloned().ok_or_else(|| {
              CodegenError::TypeError {
                message: format!("assignment to undeclared variable `{target}`"),
                loc: bin.loc,
              }
            })?;

            if lhs_ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "operator `+=` currently only supports number variables".to_string(),
                loc: bin.loc,
              });
            }

            let rhs = self.compile_expr(&bin.stx.right)?;
            if rhs.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "operator `+=` currently only supports number RHS".to_string(),
                loc: bin.stx.right.loc,
              });
            }

            let lhs_val = self.emit_load(Ty::Number, &lhs_slot)?;
            let out = self.tmp();
            self.emit(format!("  {out} = fadd double {lhs_val}, {}", rhs.ir));
            self.emit_store(Ty::Number, &out, &lhs_slot)?;

            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Addition => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "binary `+` currently only supports numbers".to_string(),
                loc: bin.loc,
              });
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fadd double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Subtraction => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "binary `-` currently only supports numbers".to_string(),
                loc: bin.loc,
              });
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fsub double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Multiplication => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "binary `*` currently only supports numbers".to_string(),
                loc: bin.loc,
              });
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fmul double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Division => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "binary `/` currently only supports numbers".to_string(),
                loc: bin.loc,
              });
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fdiv double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Remainder => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "binary `%` currently only supports numbers".to_string(),
                loc: bin.loc,
              });
            }
            let out = self.tmp();
            self.emit(format!("  {out} = frem double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::StrictEquality => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty == Ty::Void || right.ty == Ty::Void {
              return Err(CodegenError::TypeError {
                message: "cannot compare a void expression".to_string(),
                loc: bin.loc,
              });
            }
            if left.ty != right.ty {
              // JS semantics: different types are always strictly not equal.
              return Ok(Value {
                ty: Ty::Bool,
                ir: "0".to_string(),
              });
            }
            let out = self.tmp();
            match left.ty {
              Ty::Number => {
                self.emit(format!(
                  "  {out} = fcmp oeq double {}, {}",
                  left.ir, right.ir
                ));
              }
              Ty::Bool => {
                self.emit(format!("  {out} = icmp eq i1 {}, {}", left.ir, right.ir));
              }
              Ty::String => {
                let eq = self.emit_strcmp_eq(&left.ir, &right.ir)?;
                return Ok(Value {
                  ty: Ty::Bool,
                  ir: eq,
                });
              }
              Ty::Null | Ty::Undefined => {
                // `null === null` and `undefined === undefined`.
                return Ok(Value {
                  ty: Ty::Bool,
                  ir: "1".to_string(),
                });
              }
              _ => {
                return Err(CodegenError::TypeError {
                  message:
                    "`===` currently only supports numbers, booleans, strings, null, and undefined"
                      .to_string(),
                  loc: bin.loc,
                });
              }
            }
            Ok(Value {
              ty: Ty::Bool,
              ir: out,
            })
          }
          OperatorName::StrictInequality => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty == Ty::Void || right.ty == Ty::Void {
              return Err(CodegenError::TypeError {
                message: "cannot compare a void expression".to_string(),
                loc: bin.loc,
              });
            }
            if left.ty != right.ty {
              // JS semantics: different types are always strictly not equal.
              return Ok(Value {
                ty: Ty::Bool,
                ir: "1".to_string(),
              });
            }

            match left.ty {
              Ty::Number => {
                let eq = self.tmp();
                self.emit(format!(
                  "  {eq} = fcmp oeq double {}, {}",
                  left.ir, right.ir
                ));
                let out = self.tmp();
                self.emit(format!("  {out} = xor i1 {eq}, true"));
                Ok(Value {
                  ty: Ty::Bool,
                  ir: out,
                })
              }
              Ty::Bool => {
                let eq = self.tmp();
                self.emit(format!("  {eq} = icmp eq i1 {}, {}", left.ir, right.ir));
                let out = self.tmp();
                self.emit(format!("  {out} = xor i1 {eq}, true"));
                Ok(Value {
                  ty: Ty::Bool,
                  ir: out,
                })
              }
              Ty::String => {
                let eq = self.emit_strcmp_eq(&left.ir, &right.ir)?;
                let out = self.tmp();
                self.emit(format!("  {out} = xor i1 {eq}, true"));
                Ok(Value {
                  ty: Ty::Bool,
                  ir: out,
                })
              }
              Ty::Null | Ty::Undefined => Ok(Value {
                ty: Ty::Bool,
                ir: "0".to_string(),
              }),
              _ => Err(CodegenError::TypeError {
                message:
                  "`!==` currently only supports numbers, booleans, strings, null, and undefined"
                    .to_string(),
                loc: bin.loc,
              }),
            }
          }
          OperatorName::LessThan
          | OperatorName::LessThanOrEqual
          | OperatorName::GreaterThan
          | OperatorName::GreaterThanOrEqual => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "numeric comparison currently only supports numbers".to_string(),
                loc: bin.loc,
              });
            }
            let out = self.tmp();
            let pred = match bin.stx.operator {
              OperatorName::LessThan => "olt",
              OperatorName::LessThanOrEqual => "ole",
              OperatorName::GreaterThan => "ogt",
              OperatorName::GreaterThanOrEqual => "oge",
              _ => unreachable!(),
            };
            self.emit(format!(
              "  {out} = fcmp {pred} double {}, {}",
              left.ir, right.ir
            ));
            Ok(Value {
              ty: Ty::Bool,
              ir: out,
            })
          }
          OperatorName::LogicalAnd | OperatorName::LogicalOr => {
            // Support short-circuit semantics for boolean-only `&&`/`||`.
            //
            // We implement this using a local alloca + stores instead of an SSA phi node so we
            // don't need to track the current basic block label name.
            let left = self.compile_expr(&bin.stx.left)?;
            if left.ty != Ty::Bool {
              return Err(CodegenError::TypeError {
                message: "logical operators currently only support booleans".to_string(),
                loc: bin.loc,
              });
            }

            let result_slot = self.emit_alloca(Ty::Bool, bin.loc)?;
            let rhs = self.fresh_block("logic.rhs");
            let short = self.fresh_block("logic.short");
            let cont = self.fresh_block("logic.cont");

            match bin.stx.operator {
              OperatorName::LogicalAnd => {
                // false && rhs  => false (skip rhs)
                self.emit(format!("  br i1 {}, label %{rhs}, label %{short}", left.ir));
                self.emit(format!("{short}:"));
                self.emit_store(Ty::Bool, "0", &result_slot)?;
                self.emit(format!("  br label %{cont}"));
              }
              OperatorName::LogicalOr => {
                // true || rhs => true (skip rhs)
                self.emit(format!("  br i1 {}, label %{short}, label %{rhs}", left.ir));
                self.emit(format!("{short}:"));
                self.emit_store(Ty::Bool, "1", &result_slot)?;
                self.emit(format!("  br label %{cont}"));
              }
              _ => unreachable!(),
            }

            self.emit(format!("{rhs}:"));
            let right = self.compile_expr(&bin.stx.right)?;
            if right.ty != Ty::Bool {
              return Err(CodegenError::TypeError {
                message: "logical operators currently only support booleans".to_string(),
                loc: bin.stx.right.loc,
              });
            }
            self.emit_store(Ty::Bool, right.ir.as_str(), &result_slot)?;
            self.emit(format!("  br label %{cont}"));

            self.emit(format!("{cont}:"));
            let loaded = self.emit_load(Ty::Bool, &result_slot)?;
            Ok(Value {
              ty: Ty::Bool,
              ir: loaded,
            })
          }
          other => Err(CodegenError::UnsupportedOperator { op: other, loc: bin.loc }),
        }
      }

      Expr::Unary(unary) => {
        let arg = self.compile_expr(&unary.stx.argument)?;
        match unary.stx.operator {
          OperatorName::UnaryNegation => {
            if arg.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "unary `-` currently only supports numbers".to_string(),
                loc: unary.loc,
              });
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fneg double {}", arg.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::UnaryPlus => {
            if arg.ty != Ty::Number {
              return Err(CodegenError::TypeError {
                message: "unary `+` currently only supports numbers".to_string(),
                loc: unary.loc,
              });
            }
            Ok(arg)
          }
          OperatorName::LogicalNot => {
            let arg_bool = self.emit_truthy_to_bool(arg, unary.stx.argument.loc)?;
            let out = self.tmp();
            self.emit(format!("  {out} = xor i1 {arg_bool}, true"));
            Ok(Value {
              ty: Ty::Bool,
              ir: out,
            })
          }
          other => Err(CodegenError::UnsupportedOperator { op: other, loc: unary.loc }),
        }
      }

      Expr::Call(call) => {
        let builtin = recognize_builtin(call);
        if let Some(builtin) = builtin {
          if !self.opts.builtins {
            return Err(CodegenError::BuiltinsDisabled { loc: call.loc });
          }

          match builtin {
            BuiltinCall::Print { args } => {
              self.emit_print_log_call(args)?;
              // Make stdout useful for debugging even when the program later traps (e.g. SIGSEGV).
              self.emit("  notail call i32 @fflush(ptr null)".to_string());
              Ok(Value::void())
            }
            BuiltinCall::Assert { cond, msg } => {
              let cond_v = self.compile_expr(cond)?;
              let cond_bool = self.emit_truthy_to_bool(cond_v, cond.loc)?;

              let ok = self.fresh_block("assert.ok");
              let fail = self.fresh_block("assert.fail");
              self.emit(format!("  br i1 {cond_bool}, label %{ok}, label %{fail}"));

              self.emit(format!("{fail}:"));
              if let Some(msg) = msg {
                let msg_v = self.compile_expr(msg)?;
                self.emit_print_value(msg_v)?;
              } else {
                let default_msg = self.emit_string_ptr(b"assertion failed");
                self.emit(format!("  notail call i32 @puts(ptr {default_msg})"));
              }
              self.emit("  notail call i32 @fflush(ptr null)".to_string());
              self.emit("  notail call void @abort()".to_string());
              self.emit("  unreachable".to_string());

              self.emit(format!("{ok}:"));
              Ok(Value::void())
            }
            BuiltinCall::Panic { msg } => {
              if let Some(msg) = msg {
                let msg_v = self.compile_expr(msg)?;
                self.emit_print_value(msg_v)?;
              }
              self.emit("  notail call i32 @fflush(ptr null)".to_string());
              self.emit("  notail call void @abort()".to_string());
              self.emit("  unreachable".to_string());

              // Keep the IR structurally valid by starting a fresh (unreachable) block for any
              // subsequent statements / the implicit final `ret`.
              let cont = self.fresh_block("panic.after");
              self.emit(format!("{cont}:"));
              Ok(Value::void())
            }
            BuiltinCall::Trap => {
              self.emit("  notail call i32 @fflush(ptr null)".to_string());
              self.emit("  notail call void @llvm.trap()".to_string());
              self.emit("  unreachable".to_string());

              let cont = self.fresh_block("trap.after");
              self.emit(format!("{cont}:"));
              Ok(Value::void())
            }
          }
        } else {
          // Minimal support for direct calls to user-defined functions.
          if call.stx.optional_chaining {
            return Err(CodegenError::UnsupportedExpr { loc: call.loc });
          }

          let callee = match call.stx.callee.stx.as_ref() {
            Expr::Id(id) => id.stx.name.as_str(),
            _ => {
              return Err(CodegenError::UnsupportedExpr {
                loc: call.stx.callee.loc,
              });
            }
          };

          let sig = self.function_sigs.get(callee).cloned().ok_or_else(|| {
            CodegenError::TypeError {
              message: format!("call to unknown function `{callee}`"),
              loc: call.loc,
            }
          })?;
          let llvm_name = self
            .function_llvm_names
            .get(callee)
            .cloned()
            .expect("collected function LLVM names earlier");

          if sig.params.len() != call.stx.arguments.len() {
            return Err(CodegenError::TypeError {
              message: format!(
                "function `{callee}` expects {} args, got {}",
                sig.params.len(),
                call.stx.arguments.len()
              ),
              loc: call.loc,
            });
          }

          let mut arg_irs = Vec::with_capacity(sig.params.len());
          for (idx, (param_ty, arg)) in sig.params.iter().zip(&call.stx.arguments).enumerate() {
            if arg.stx.spread {
              return Err(CodegenError::UnsupportedExpr { loc: arg.loc });
            }
            let v = self.compile_expr(&arg.stx.value)?;
            if v.ty != *param_ty {
              return Err(CodegenError::TypeError {
                message: format!(
                  "argument {idx} to `{callee}` has type {got:?}, expected {expected:?}",
                  got = v.ty,
                  expected = param_ty
                ),
                loc: arg.loc,
              });
            }
            let llvm_ty = Self::llvm_type_of(*param_ty);
            let value_ir = match v.ty {
              Ty::Null | Ty::Undefined => "0".to_string(),
              _ => v.ir,
            };
            arg_irs.push(format!("{llvm_ty} {value_ir}"));
          }

          let ret_ty = sig.ret;
          if ret_ty == Ty::Void {
            self.emit(format!(
              "  notail call void {llvm_name}({})",
              arg_irs.join(", ")
            ));
            Ok(Value::void())
          } else {
            let out = self.tmp();
            let llvm_ret = Self::llvm_type_of(ret_ty);
            self.emit(format!(
              "  {out} = notail call {llvm_ret} {llvm_name}({})",
              arg_irs.join(", ")
            ));
            Ok(Value {
              ty: ret_ty,
              ir: out,
            })
          }
        }
      }

      _ => Err(CodegenError::UnsupportedExpr { loc: expr.loc }),
    })();

    self.dbg_current_pos = saved_dbg_pos;
    result
  }
}

pub(super) fn emit_llvm_module(
  ast: &Node<TopLevel>,
  source: &str,
  opts: CompileOptions,
) -> Result<String, CodegenError> {
  // The minimal parse-js-driven emitter is intended for single-module programs. Module-level
  // `import`/`export` syntax requires project compilation so we can build a module graph, resolve
  // bindings, and order initializers deterministically.
  //
  // Scan upfront so we return `UnsupportedStmt` consistently (instead of e.g. failing with
  // "call to unknown function" while compiling a function body that references an imported
  // binding). `native-js-cli` relies on this error to decide when to fall back to the project
  // pipeline.
  for stmt in &ast.stx.body {
    match stmt.stx.as_ref() {
      Stmt::Import(_) | Stmt::ImportTypeDecl(_) | Stmt::ImportEqualsDecl(_) => {
        return Err(CodegenError::UnsupportedStmt { loc: stmt.loc });
      }
      // `export { ... }` (no `from`) is a runtime no-op. The minimal single-module emitter can
      // safely ignore it, which also lets callers add `export {};` as a deterministic module
      // marker for otherwise-script sources.
      Stmt::ExportList(export) => {
        if export.stx.from.is_some() {
          return Err(CodegenError::UnsupportedStmt { loc: stmt.loc });
        }
      }
      Stmt::ExportDefaultExpr(_)
      | Stmt::ExportAssignmentDecl(_)
      | Stmt::ExportAsNamespaceDecl(_)
      | Stmt::ExportTypeDecl(_) => return Err(CodegenError::UnsupportedStmt { loc: stmt.loc }),
      _ => {}
    }
  }

  let mut cg = Codegen::new(opts);
  if cg.dbg.is_some() {
    cg.set_debug_main_file(SYNTHETIC_INPUT_FILE, source);
  }

  cg.collect_function_signatures(ast)?;
  cg.compile_function_decls(ast)?;

  cg.reset_fn_ctx(None);

  if cg.dbg.is_some() {
    let file_id = cg.dbg_current_file_id();
    let main_pos = ast
      .stx
      .body
      .first()
      .and_then(|stmt| cg.dbg_pos_for_loc(stmt.loc))
      .unwrap_or(DebugPos { line: 1, col: 1 });
    cg.dbg_main_subprogram = match (cg.dbg.as_mut(), file_id) {
      (Some(dbg), Some(file_id)) => {
        Some(dbg.new_subprogram("main", "main", file_id, main_pos.line, 0))
      }
      _ => None,
    };
    cg.dbg_current_scope = cg.dbg_main_subprogram;
    cg.dbg_current_pos = Some(main_pos);
  }

  cg.emit("entry:");
  for stmt in &ast.stx.body {
    cg.compile_stmt(stmt)?;
  }
  cg.emit("  ret i32 0");

  let mut out = String::new();
  out.push_str("; ModuleID = 'native-js'\n");
  out.push_str(&format!(
    "source_filename = \"{}\"\n\n",
    llvm_escape_metadata_string(SYNTHETIC_INPUT_FILE)
  ));

  for def in &cg.strings.defs {
    out.push_str(def);
    out.push('\n');
  }
  if !cg.strings.defs.is_empty() {
    out.push('\n');
  }

  out.push_str("declare i32 @puts(ptr)\n");
  out.push_str("declare i32 @printf(ptr, ...)\n");
  out.push_str("declare i32 @fflush(ptr)\n");
  out.push_str("declare i32 @strcmp(ptr, ptr)\n");
  out.push_str("declare void @abort()\n");
  out.push_str("declare void @llvm.trap()\n\n");

  for func in &cg.function_defs {
    out.push_str(func);
    out.push('\n');
  }

  // Stack-walkability invariants for precise GC:
  // - Keep frame pointers so the runtime can walk the frame chain.
  // - Disable tail calls so frames are not elided.
  //
  // See `native-js/docs/gc_stack_walking.md`.
  let dbg = cg
    .dbg_main_subprogram
    .map(|id| format!(" !dbg !{id}"))
    .unwrap_or_default();
  out.push_str(&format!("define i32 @main() #0{dbg} {{\n"));
  for line in &cg.body {
    out.push_str(line);
    out.push('\n');
  }
  out.push_str("}\n");
  out.push_str("\nattributes #0 = { \"frame-pointer\"=\"all\" \"disable-tail-calls\"=\"true\" }\n");

  if let Some(dbg) = cg.dbg.as_ref() {
    out.push_str(&dbg.render());
  }

  Ok(out)
}

#[derive(Clone, Debug)]
struct FunctionSig {
  ret: Ty,
  params: Vec<Ty>,
}

#[derive(Clone, Debug)]
struct FnCtx {
  tmp_counter: usize,
  block_counter: usize,
  loop_stack: Vec<LoopContext>,
  vars: HashMap<String, (Ty, String)>,
  body: Vec<String>,
  current_return_ty: Option<Ty>,
  block_terminated: bool,
}

impl Codegen {
  fn reset_fn_ctx(&mut self, ret: Option<Ty>) {
    self.tmp_counter = 0;
    self.block_counter = 0;
    self.loop_stack.clear();
    self.vars.clear();
    self.body.clear();
    self.current_return_ty = ret;
    self.block_terminated = false;
  }

  fn take_fn_ctx(&mut self) -> FnCtx {
    FnCtx {
      tmp_counter: self.tmp_counter,
      block_counter: self.block_counter,
      loop_stack: std::mem::take(&mut self.loop_stack),
      vars: std::mem::take(&mut self.vars),
      body: std::mem::take(&mut self.body),
      current_return_ty: self.current_return_ty,
      block_terminated: self.block_terminated,
    }
  }

  fn restore_fn_ctx(&mut self, ctx: FnCtx) {
    self.tmp_counter = ctx.tmp_counter;
    self.block_counter = ctx.block_counter;
    self.loop_stack = ctx.loop_stack;
    self.vars = ctx.vars;
    self.body = ctx.body;
    self.current_return_ty = ctx.current_return_ty;
    self.block_terminated = ctx.block_terminated;
  }

  fn type_from_type_expr(ty: &Node<TypeExpr>) -> Result<Ty, CodegenError> {
    match ty.stx.as_ref() {
      TypeExpr::Number(_) => Ok(Ty::Number),
      TypeExpr::Boolean(_) => Ok(Ty::Bool),
      TypeExpr::String(_) => Ok(Ty::String),
      TypeExpr::Void(_) => Ok(Ty::Void),
      TypeExpr::Null(_) => Ok(Ty::Null),
      TypeExpr::Undefined(_) => Ok(Ty::Undefined),
      other => Err(CodegenError::TypeError {
        message: format!("unsupported type annotation: {other:?}"),
        loc: ty.loc,
      }),
    }
  }

  fn default_value_ir(ty: Ty) -> String {
    match ty {
      Ty::Number => f64_to_llvm_const(0.0),
      Ty::Bool => "0".to_string(),
      Ty::String => "null".to_string(),
      Ty::Null | Ty::Undefined => "0".to_string(),
      Ty::Void => String::new(),
    }
  }

  fn collect_function_signatures(&mut self, ast: &Node<TopLevel>) -> Result<(), CodegenError> {
    for stmt in &ast.stx.body {
      let Stmt::FunctionDecl(decl) = stmt.stx.as_ref() else {
        continue;
      };
      let Some(name) = decl.stx.name.as_ref().map(|n| n.stx.name.clone()) else {
        return Err(CodegenError::TypeError {
          message: "function declarations must have a name".to_string(),
          loc: decl.loc,
        });
      };
      if name == "main" {
        return Err(CodegenError::TypeError {
          message: "`main` is reserved for the native entrypoint; use a different function name"
            .to_string(),
          loc: decl.loc,
        });
      }
      if self.function_sigs.contains_key(&name) {
        return Err(CodegenError::TypeError {
          message: format!("duplicate function declaration `{name}`"),
          loc: decl.loc,
        });
      }

      let func = &decl.stx.function;
      if func.stx.async_ || func.stx.generator {
        return Err(CodegenError::TypeError {
          message: format!("function `{name}` must not be async or a generator"),
          loc: func.loc,
        });
      }

      let ret = match func.stx.return_type.as_ref() {
        Some(ret) => Self::type_from_type_expr(ret)?,
        None => Ty::Number,
      };

      let mut params = Vec::new();
      for param in &func.stx.parameters {
        if param.stx.rest || param.stx.optional {
          return Err(CodegenError::TypeError {
            message: format!("function `{name}` has unsupported parameter syntax"),
            loc: param.loc,
          });
        }
        let param_ty = match param.stx.type_annotation.as_ref() {
          Some(ann) => Self::type_from_type_expr(ann)?,
          None => Ty::Number,
        };
        params.push(param_ty);
      }

      self
        .function_llvm_names
        .insert(name.clone(), format!("@{name}"));
      self.function_sigs.insert(name, FunctionSig { ret, params });
    }
    Ok(())
  }

  fn compile_function_decls(&mut self, ast: &Node<TopLevel>) -> Result<(), CodegenError> {
    for stmt in &ast.stx.body {
      let Stmt::FunctionDecl(decl) = stmt.stx.as_ref() else {
        continue;
      };
      self.compile_function_decl(decl)?;
    }
    Ok(())
  }

  fn compile_function_decl(&mut self, decl: &Node<FuncDecl>) -> Result<(), CodegenError> {
    let Some(name) = decl.stx.name.as_ref().map(|n| n.stx.name.clone()) else {
      return Err(CodegenError::TypeError {
        message: "function declarations must have a name".to_string(),
        loc: decl.loc,
      });
    };
    let sig = self
      .function_sigs
      .get(&name)
      .expect("collected function signatures earlier")
      .clone();

    let saved_dbg_scope = self.dbg_current_scope;
    let saved_dbg_pos = self.dbg_current_pos;

    let saved = self.take_fn_ctx();
    self.reset_fn_ctx(Some(sig.ret));

    let file_id = self.dbg_current_file_id();
    let fn_pos = self.dbg_pos_for_loc(decl.loc);
    let fn_line = fn_pos.map(|p| p.line).unwrap_or(1);
    let subprogram = match (self.dbg.as_mut(), file_id) {
      (Some(dbg), Some(file_id)) => Some(
        dbg.new_subprogram(
          &name,
          self
            .function_llvm_names
            .get(&name)
            .expect("collected function LLVM names earlier")
            .trim_start_matches('@'),
          file_id,
          fn_line,
          sig.params.len(),
        ),
      ),
      _ => None,
    };
    self.dbg_current_scope = subprogram;
    self.dbg_current_pos = fn_pos;

    // Emit function prologue.
    self.emit("entry:");

    let mut param_decls = Vec::new();
    // Map parameters into local slots, so we can use the same variable lookup logic as locals.
    for (idx, param) in decl.stx.function.stx.parameters.iter().enumerate() {
      let param_name = match param.stx.pattern.stx.pat.stx.as_ref() {
        Pat::Id(id) => id.stx.name.clone(),
        _ => {
          return Err(CodegenError::TypeError {
            message: format!("function `{name}` parameter {idx} must be an identifier"),
            loc: param.stx.pattern.loc,
          });
        }
      };

      let expected_ty = sig
        .params
        .get(idx)
        .copied()
        .ok_or_else(|| CodegenError::TypeError {
          message: "parameter list mismatch".to_string(),
          loc: decl.loc,
        })?;
      let llvm_ty = Self::llvm_type_of(expected_ty);
      param_decls.push(format!("{llvm_ty} %{param_name}"));

      let slot = self.emit_alloca(expected_ty, param.loc)?;
      self.emit_store(expected_ty, &format!("%{param_name}"), &slot)?;
      self.vars.insert(param_name, (expected_ty, slot));
    }

    match decl.stx.function.stx.body.as_ref() {
      Some(FuncBody::Block(stmts)) => {
        for stmt in stmts {
          self.compile_stmt(stmt)?;
        }
      }
      Some(FuncBody::Expression(expr)) => {
        let value = self.compile_expr(expr)?;
        if value.ty != sig.ret {
          return Err(CodegenError::TypeError {
            message: format!(
              "function `{name}` returns {got:?}, expected {expected:?}",
              got = value.ty,
              expected = sig.ret
            ),
            loc: expr.loc,
          });
        }
        let llvm_ty = Self::llvm_type_of(sig.ret);
        let value_ir = match sig.ret {
          Ty::Null | Ty::Undefined => "0".to_string(),
          _ => value.ir,
        };
        self.emit(format!("  ret {llvm_ty} {value_ir}"));
      }
      None => {}
    }

    // Ensure the function is well-formed even if the source forgot a `return`.
    if !self.block_terminated {
      match sig.ret {
        Ty::Void => self.emit("  ret void".to_string()),
        other => {
          let llvm_ty = Self::llvm_type_of(other);
          let value_ir = Self::default_value_ir(other);
          self.emit(format!("  ret {llvm_ty} {value_ir}"));
        }
      }
    }

    let llvm_name = self
      .function_llvm_names
      .get(&name)
      .expect("collected function LLVM names earlier");
    let mut def = String::new();
    let dbg = subprogram
      .map(|id| format!(" !dbg !{id}"))
      .unwrap_or_default();
    def.push_str(&format!(
      "define {} {llvm_name}({}) #0{dbg} {{\n",
      Self::llvm_type_of(sig.ret),
      param_decls.join(", ")
    ));
    for line in &self.body {
      def.push_str(line);
      def.push('\n');
    }
    def.push_str("}\n");
    self.function_defs.push(def);

    self.restore_fn_ctx(saved);
    self.dbg_current_scope = saved_dbg_scope;
    self.dbg_current_pos = saved_dbg_pos;
    Ok(())
  }
}

pub(crate) struct LlvmModuleBuilder {
  cg: Codegen,
  source_filename: String,
}

impl LlvmModuleBuilder {
  pub(crate) fn new(opts: CompileOptions) -> Self {
    Self {
      cg: Codegen::new(opts),
      source_filename: "native-js".to_string(),
    }
  }

  pub(crate) fn set_entry_file(&mut self, filename: &str, source: &str) {
    self.source_filename = self.cg.debug_file_key_fields(filename).0;
    self.cg.set_debug_main_file(filename, source);
  }

  pub(crate) fn set_source_file(&mut self, filename: &str, source: &str) {
    self.cg.set_debug_current_file(filename, source);
  }

  fn with_call_targets<T>(
    &mut self,
    call_targets: &BTreeMap<String, UserFunctionSig>,
    f: impl FnOnce(&mut Codegen) -> Result<T, CodegenError>,
  ) -> Result<T, CodegenError> {
    let saved_sigs = std::mem::take(&mut self.cg.function_sigs);
    let saved_names = std::mem::take(&mut self.cg.function_llvm_names);

    for (local, sig) in call_targets {
      self.cg.function_sigs.insert(
        local.clone(),
        FunctionSig {
          ret: sig.ret,
          params: sig.params.clone(),
        },
      );
      self
        .cg
        .function_llvm_names
        .insert(local.clone(), sig.llvm_name.clone());
    }

    let out = f(&mut self.cg);

    self.cg.function_sigs = saved_sigs;
    self.cg.function_llvm_names = saved_names;

    out
  }

  pub(crate) fn add_init_function(
    &mut self,
    llvm_name: &str,
    stmts: &[&Node<Stmt>],
    call_targets: &BTreeMap<String, UserFunctionSig>,
  ) -> Result<(), CodegenError> {
    self.with_call_targets(call_targets, |cg| {
      let saved_dbg_scope = cg.dbg_current_scope;
      let saved_dbg_pos = cg.dbg_current_pos;

      let saved = cg.take_fn_ctx();
      cg.reset_fn_ctx(Some(Ty::Void));

      let file_id = cg.dbg_current_file_id();
      let fn_pos = stmts
        .first()
        .and_then(|stmt| cg.dbg_pos_for_loc(stmt.loc))
        .unwrap_or(DebugPos { line: 1, col: 1 });
      let subprogram = match (cg.dbg.as_mut(), file_id) {
        (Some(dbg), Some(file_id)) => {
          let name = llvm_name.trim_start_matches('@');
          Some(dbg.new_subprogram(name, name, file_id, fn_pos.line, 0))
        }
        _ => None,
      };
      cg.dbg_current_scope = subprogram;
      cg.dbg_current_pos = Some(fn_pos);

      cg.emit("entry:");
      for stmt in stmts {
        cg.compile_stmt(stmt)?;
      }
      if !cg.block_terminated {
        cg.emit("  ret void".to_string());
      }

      let mut def = String::new();
      let dbg = subprogram
        .map(|id| format!(" !dbg !{id}"))
        .unwrap_or_default();
      def.push_str(&format!("define void {llvm_name}() #0{dbg} {{\n"));
      for line in &cg.body {
        def.push_str(line);
        def.push('\n');
      }
      def.push_str("}\n");
      cg.function_defs.push(def);

      cg.restore_fn_ctx(saved);
      cg.dbg_current_scope = saved_dbg_scope;
      cg.dbg_current_pos = saved_dbg_pos;
      Ok(())
    })
  }

  pub(crate) fn add_ts_function(
    &mut self,
    _llvm_name: &str,
    decl: &Node<FuncDecl>,
    call_targets: &BTreeMap<String, UserFunctionSig>,
  ) -> Result<(), CodegenError> {
    self.with_call_targets(call_targets, |cg| cg.compile_function_decl(decl))
  }

  pub(crate) fn add_main(
    &mut self,
    init_symbols: &[String],
    entry_call: Option<&UserFunctionSig>,
  ) -> Result<(), CodegenError> {
    self.cg.reset_fn_ctx(None);

    if self.cg.dbg.is_some() {
      let file_id = self.cg.dbg_current_file_id();
      let main_pos = self
        .cg
        .dbg_pos_for_loc(Loc(0, 0))
        .unwrap_or(DebugPos { line: 1, col: 1 });
      self.cg.dbg_main_subprogram = match (self.cg.dbg.as_mut(), file_id) {
        (Some(dbg), Some(file_id)) => {
          Some(dbg.new_subprogram("main", "main", file_id, main_pos.line, 0))
        }
        _ => None,
      };
      self.cg.dbg_current_scope = self.cg.dbg_main_subprogram;
      self.cg.dbg_current_pos = Some(main_pos);
    }

    self.cg.emit("entry:");
    for init in init_symbols {
      self.cg.emit(format!("  call void {init}()"));
    }
    if let Some(entry) = entry_call {
      let ret = Codegen::llvm_type_of(entry.ret);
      self.cg.emit(format!("  call {ret} {}()", entry.llvm_name));
    }
    self.cg.emit("  ret i32 0");
    Ok(())
  }

  pub(crate) fn finish(self) -> String {
    let mut out = String::new();
    out.push_str("; ModuleID = 'native-js'\n");
    out.push_str(&format!(
      "source_filename = \"{}\"\n\n",
      llvm_escape_metadata_string(&self.source_filename)
    ));

    for def in &self.cg.strings.defs {
      out.push_str(def);
      out.push('\n');
    }
    if !self.cg.strings.defs.is_empty() {
      out.push('\n');
    }

    out.push_str("declare i32 @puts(ptr)\n");
    out.push_str("declare i32 @printf(ptr, ...)\n");
    out.push_str("declare i32 @fflush(ptr)\n");
    out.push_str("declare i32 @strcmp(ptr, ptr)\n");
    out.push_str("declare void @abort()\n");
    out.push_str("declare void @llvm.trap()\n\n");

    for func in &self.cg.function_defs {
      out.push_str(func);
      out.push('\n');
    }

    // Stack-walkability invariants for precise GC:
    // - Keep frame pointers so the runtime can walk the frame chain.
    // - Disable tail calls so frames are not elided.
    //
    // See `native-js/docs/gc_stack_walking.md`.
    let dbg = self
      .cg
      .dbg_main_subprogram
      .map(|id| format!(" !dbg !{id}"))
      .unwrap_or_default();
    out.push_str(&format!("define i32 @main() #0{dbg} {{\n"));
    for line in &self.cg.body {
      out.push_str(line);
      out.push('\n');
    }
    out.push_str("}\n");
    out.push_str(
      "\nattributes #0 = { \"frame-pointer\"=\"all\" \"disable-tail-calls\"=\"true\" }\n",
    );

    if let Some(dbg) = self.cg.dbg.as_ref() {
      out.push_str(&dbg.render());
    }

    out
  }
}
