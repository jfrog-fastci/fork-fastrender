#!/usr/bin/env node
// Minimal NDJSON TypeScript runner for the Rust harness.
const path = require("path");
const readline = require("readline");
const { loadTypeScript } = require("./typescript_loader");
let ts;
try {
  ts = loadTypeScript();
} catch (err) {
  process.stderr.write(`${err?.message ?? String(err)}\n`);
  process.exit(1);
}

const VIRTUAL_ROOT = "/";
const SCHEMA_VERSION = 2;

function normalizePath(fileName) {
  const normalized = path.posix.normalize(fileName.replace(/\\/g, "/"));
  // Canonicalize Windows drive letter casing so comparisons work on
  // case-insensitive file systems (and to keep paths stable across callers).
  const canonical = normalized.replace(
    /^(\/?)([A-Za-z]):/,
    (_match, leadingSlash, drive) => `${leadingSlash}${drive.toLowerCase()}:`,
  );

  // Preserve drive roots like `c:/` / `/c:/`.
  const isDriveRoot = /^[a-z]:\/$/i.test(canonical) || /^\/[a-z]:\/$/i.test(canonical);
  if (!isDriveRoot && canonical.length > 1 && canonical.endsWith("/")) {
    return canonical.slice(0, -1);
  }
  return canonical;
}

function isWithinRoot(candidate, root) {
  return candidate === root || candidate.startsWith(`${root}/`);
}

function computeAllowedDiskRoots() {
  const libPath = toAbsolute(ts.getDefaultLibFilePath({}));
  const libDir = normalizePath(path.posix.dirname(libPath));
  const packageRoot = normalizePath(path.posix.join(libDir, ".."));
  return new Set([libDir, packageRoot]);
}

const ALLOWED_DISK_ROOTS = computeAllowedDiskRoots();

function isAllowedDiskPath(absolutePath) {
  for (const root of ALLOWED_DISK_ROOTS) {
    if (isWithinRoot(absolutePath, root)) {
      return true;
    }
  }
  return false;
}

function toDiskPath(virtualAbsolutePath) {
  // `toAbsolute` uses POSIX rules so Windows rooted paths like `c:/x/y` become
  // `/c:/x/y` in the virtual FS. When delegating to the real disk-backed host,
  // strip the leading `/` so Node/TypeScript can open the file on Windows.
  if (virtualAbsolutePath.startsWith("/") && /^[a-z]:\//i.test(virtualAbsolutePath.slice(1))) {
    return virtualAbsolutePath.slice(1);
  }
  return virtualAbsolutePath;
}

function utf16ToUtf8ByteOffset(text, utf16Pos) {
  if (!text || utf16Pos <= 0) {
    return 0;
  }
  const target = Math.min(utf16Pos, text.length);
  let bytes = 0;
  let idx = 0;
  while (idx < target) {
    const code = text.charCodeAt(idx);
    if (code < 0x80) {
      bytes += 1;
      idx += 1;
      continue;
    }
    if (code < 0x800) {
      bytes += 2;
      idx += 1;
      continue;
    }
    // Surrogate pair (UTF-16 uses 2 code units, UTF-8 uses 4 bytes).
    if (code >= 0xd800 && code <= 0xdbff && idx + 1 < text.length) {
      const next = text.charCodeAt(idx + 1);
      if (next >= 0xdc00 && next <= 0xdfff && idx + 1 < target) {
        bytes += 4;
        idx += 2;
        continue;
      }
    }
    bytes += 3;
    idx += 1;
  }
  return bytes;
}

function utf8ByteOffsetToUtf16(text, bytePos) {
  if (!text || bytePos <= 0) {
    return 0;
  }

  let utf16Pos = 0;
  let bytes = 0;
  const target = Math.max(0, bytePos);
  while (utf16Pos < text.length && bytes < target) {
    const code = text.charCodeAt(utf16Pos);
    let charBytes = 0;
    let charLen = 1;
    if (code < 0x80) {
      charBytes = 1;
    } else if (code < 0x800) {
      charBytes = 2;
    } else if (code >= 0xd800 && code <= 0xdbff && utf16Pos + 1 < text.length) {
      const next = text.charCodeAt(utf16Pos + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        charBytes = 4;
        charLen = 2;
      } else {
        charBytes = 3;
      }
    } else {
      charBytes = 3;
    }

    if (bytes + charBytes > target) {
      return utf16Pos;
    }
    bytes += charBytes;
    utf16Pos += charLen;
  }
  return utf16Pos;
}

function toAbsolute(fileName) {
  const normalized = normalizePath(fileName);
  return path.posix.isAbsolute(normalized) ? normalized : path.posix.join(VIRTUAL_ROOT, normalized);
}

function collectVirtualDirectories(fileNames) {
  const dirs = new Set([VIRTUAL_ROOT]);
  for (const fileName of fileNames) {
    let dir = path.posix.dirname(fileName);
    while (dir && !dirs.has(dir)) {
      dirs.add(dir);
      const parent = path.posix.dirname(dir);
      if (parent === dir) {
        break;
      }
      dir = parent;
    }
  }
  return dirs;
}

function listVirtualSubdirectories(dirName, virtualDirectories) {
  const dir = dirName.endsWith("/") ? dirName : `${dirName}/`;
  const children = new Set();
  for (const candidate of virtualDirectories) {
    if (!candidate.startsWith(dir) || candidate === dirName) {
      continue;
    }
    const remainder = candidate.slice(dir.length);
    if (!remainder) {
      continue;
    }
    const next = remainder.split("/")[0];
    if (next) {
      children.add(path.posix.join(dirName, next));
    }
  }
  return Array.from(children).sort();
}

function categoryToString(category) {
  switch (category) {
    case ts.DiagnosticCategory.Message:
      return "message";
    case ts.DiagnosticCategory.Warning:
      return "warning";
    case ts.DiagnosticCategory.Suggestion:
      return "suggestion";
    case ts.DiagnosticCategory.Error:
    default:
      return "error";
  }
}

function flattenMessage(messageText) {
  return ts.flattenDiagnosticMessageText(messageText, "\n");
}

function computeLineStarts(text) {
  const starts = [0];
  for (let idx = 0; idx < text.length; idx++) {
    const ch = text.charCodeAt(idx);
    if (ch === 13 /* \r */) {
      if (text.charCodeAt(idx + 1) === 10 /* \n */) {
        idx++;
      }
      starts.push(idx + 1);
    } else if (ch === 10 /* \n */) {
      starts.push(idx + 1);
    }
  }
  return starts;
}

function collectTypeQueries(files) {
  const queries = [];
  const entries = Object.entries(files || {});
  entries.sort(([a], [b]) => a.localeCompare(b));
  for (const [rawName, text] of entries) {
    const normalized = normalizePath(rawName);
    const lineStarts = computeLineStarts(text);
    const lines = text.split(/\r?\n/);
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      let search = line.indexOf("^?");
      while (search !== -1) {
        const before = line.slice(0, search);
        const hasCodeBefore = before.trim().length > 0 && !before.trim().startsWith("//");
        const targetLine = hasCodeBefore ? i : i - 1;
        if (targetLine >= 0) {
          const startUtf16 = lineStarts[targetLine] ?? 0;
          const endUtf16 = lineStarts[targetLine + 1] ?? text.length;
          const columnUtf16 = Math.min(search, endUtf16 - startUtf16);
          const offsetUtf16 = startUtf16 + columnUtf16;
          const offset = utf16ToUtf8ByteOffset(text, offsetUtf16);
          const startBytes = utf16ToUtf8ByteOffset(text, startUtf16);
          const column = offset - startBytes;
          queries.push({
            file: normalized,
            offset,
            line: targetLine,
            column,
          });
        }
        search = line.indexOf("^?", search + 2);
      }
    }
  }
  return queries;
}

const TYPE_FORMAT_FLAGS =
  ts.TypeFormatFlags.NoTruncation | ts.TypeFormatFlags.WriteArrowStyleSignature;

function moduleResolutionKindToString(kind) {
  if (kind === undefined || kind === null) {
    return "node10";
  }
  if (kind === ts.ModuleResolutionKind.Classic) {
    return "classic";
  }
  // `NodeJs` was renamed to `Node10` but is still present in older versions.
  if (
    kind === ts.ModuleResolutionKind.Node10 ||
    kind === ts.ModuleResolutionKind.NodeJs
  ) {
    return "node10";
  }
  if (kind === ts.ModuleResolutionKind.Node16) {
    return "node16";
  }
  if (kind === ts.ModuleResolutionKind.NodeNext) {
    return "nodenext";
  }
  if (kind === ts.ModuleResolutionKind.Bundler) {
    return "bundler";
  }
  return "node10";
}

function moduleResolutionModeString(options) {
  // TypeScript does not always materialize defaults into the plain `options`
  // object, so prefer querying the helper when available.
  let kind = options?.moduleResolution;
  if (
    (kind === undefined || kind === null) &&
    typeof ts.getEmitModuleResolutionKind === "function"
  ) {
    try {
      kind = ts.getEmitModuleResolutionKind(options);
    } catch {
      // ignore
    }
  }
  if (
    (kind === undefined || kind === null) &&
    options?.module === ts.ModuleKind.Node16
  ) {
    kind = ts.ModuleResolutionKind.Node16;
  }
  if (
    (kind === undefined || kind === null) &&
    options?.module === ts.ModuleKind.NodeNext
  ) {
    kind = ts.ModuleResolutionKind.NodeNext;
  }
  return moduleResolutionKindToString(kind);
}

function createResolutionTraceCollector(mode) {
  return { mode, byFrom: new Map() };
}

function recordResolutionTrace(collector, entry) {
  const from = entry.from;
  const specifier = entry.specifier;
  let bySpecifier = collector.byFrom.get(from);
  if (!bySpecifier) {
    bySpecifier = new Map();
    collector.byFrom.set(from, bySpecifier);
  }
  let list = bySpecifier.get(specifier);
  if (!list) {
    list = [];
    bySpecifier.set(specifier, list);
  }
  list.push(entry);
}

function finalizeResolutionTrace(collector) {
  const out = [];
  const fromKeys = Array.from(collector.byFrom.keys()).sort();
  for (const from of fromKeys) {
    const bySpecifier = collector.byFrom.get(from);
    const specKeys = Array.from(bySpecifier.keys()).sort();
    for (const specifier of specKeys) {
      const entries = bySpecifier.get(specifier) || [];
      for (const entry of entries) {
        out.push(entry);
      }
    }
  }
  return out;
}

function installResolutionTracing(host, options, collector) {
  const mode = collector.mode;

  const originalResolveModuleNameLiterals = host.resolveModuleNameLiterals
    ? host.resolveModuleNameLiterals.bind(host)
    : null;
  if (originalResolveModuleNameLiterals) {
    host.resolveModuleNameLiterals = (
      moduleLiterals,
      containingFile,
      redirectedReference,
      compilerOptions,
      containingSourceFile,
      reusedNames,
    ) => {
      const results = originalResolveModuleNameLiterals(
        moduleLiterals,
        containingFile,
        redirectedReference,
        compilerOptions,
        containingSourceFile,
        reusedNames,
      );
      for (let i = 0; i < moduleLiterals.length; i++) {
        const literal = moduleLiterals[i];
        const specifier =
          literal && typeof literal.text === "string" ? literal.text : String(literal);
        const resolved = results?.[i]?.resolvedModule?.resolvedFileName ?? null;
        let kind = null;
        if (
          containingSourceFile &&
          typeof ts.getModeForResolutionAtIndex === "function"
        ) {
          try {
            const resolutionMode = ts.getModeForResolutionAtIndex(
              containingSourceFile,
              i,
              compilerOptions ?? options,
            );
            if (resolutionMode === ts.ModuleKind.CommonJS) {
              kind = "require";
            } else if (resolutionMode != null) {
              kind = "import";
            }
          } catch {
            // ignore
          }
        }
        recordResolutionTrace(collector, {
          from: normalizePath(containingFile),
          specifier,
          resolved: resolved ? normalizePath(resolved) : null,
          kind,
          mode,
        });
      }
      return results;
    };
  }

  const originalResolveModuleNames = host.resolveModuleNames
    ? host.resolveModuleNames.bind(host)
    : null;
  if (originalResolveModuleNames) {
    host.resolveModuleNames = (
      moduleNames,
      containingFile,
      reusedNames,
      redirectedReference,
      compilerOptions,
      containingSourceFile,
    ) => {
      const results = originalResolveModuleNames(
        moduleNames,
        containingFile,
        reusedNames,
        redirectedReference,
        compilerOptions,
        containingSourceFile,
      );
      for (let i = 0; i < moduleNames.length; i++) {
        const specifier = moduleNames[i];
        const resolved = results?.[i]?.resolvedFileName ?? null;
        recordResolutionTrace(collector, {
          from: normalizePath(containingFile),
          specifier,
          resolved: resolved ? normalizePath(resolved) : null,
          kind: null,
          mode,
        });
      }
      return results;
    };
  }

  const originalResolveTypeReferenceDirectives = host.resolveTypeReferenceDirectives
    ? host.resolveTypeReferenceDirectives.bind(host)
    : null;
  if (originalResolveTypeReferenceDirectives) {
    host.resolveTypeReferenceDirectives = (
      typeDirectiveNames,
      containingFile,
      redirectedReference,
      compilerOptions,
    ) => {
      const results = originalResolveTypeReferenceDirectives(
        typeDirectiveNames,
        containingFile,
        redirectedReference,
        compilerOptions,
      );
      for (let i = 0; i < typeDirectiveNames.length; i++) {
        const specifier = typeDirectiveNames[i];
        const resolved = results?.[i]?.resolvedFileName ?? null;
        recordResolutionTrace(collector, {
          from: normalizePath(containingFile),
          specifier,
          resolved: resolved ? normalizePath(resolved) : null,
          kind: null,
          mode,
        });
      }
      return results;
    };
  }

  const originalResolveTypeReferenceDirectiveReferences =
    host.resolveTypeReferenceDirectiveReferences
      ? host.resolveTypeReferenceDirectiveReferences.bind(host)
      : null;
  if (originalResolveTypeReferenceDirectiveReferences) {
    host.resolveTypeReferenceDirectiveReferences = (
      typeDirectiveReferences,
      containingFile,
      redirectedReference,
      compilerOptions,
      containingSourceFile,
      reusedNames,
    ) => {
      const results = originalResolveTypeReferenceDirectiveReferences(
        typeDirectiveReferences,
        containingFile,
        redirectedReference,
        compilerOptions,
        containingSourceFile,
        reusedNames,
      );
      for (let i = 0; i < typeDirectiveReferences.length; i++) {
        const ref = typeDirectiveReferences[i];
        const specifier =
          ref && typeof ref.fileName === "string"
            ? ref.fileName
            : ref && typeof ref.text === "string"
              ? ref.text
              : String(ref);
        const resolved =
          results?.[i]?.resolvedTypeReferenceDirective?.resolvedFileName ??
          results?.[i]?.resolvedFileName ??
          null;
        recordResolutionTrace(collector, {
          from: normalizePath(containingFile),
          specifier,
          resolved: resolved ? normalizePath(resolved) : null,
          kind: null,
          mode,
        });
      }
      return results;
    };
  }
}

function renderType(checker, type, context) {
  return checker.typeToString(type, context, TYPE_FORMAT_FLAGS).trim();
}

function collectExportTypes(checker, sourceFile) {
  const moduleSymbol = checker.getSymbolAtLocation(sourceFile);
  if (!moduleSymbol) {
    return [];
  }
  const exports = [...(checker.getExportsOfModule(moduleSymbol) || [])];
  // `getExportsOfModule` omits the synthetic `export=` symbol created for
  // `export = <expr>` assignments. Surface it explicitly so difftsc can record
  // the exported value type for CommonJS-style modules.
  const exportEq =
    moduleSymbol.exports && typeof moduleSymbol.exports.get === "function"
      ? moduleSymbol.exports.get("export=")
      : null;
  if (exportEq && !exports.some((sym) => sym.getName() === "export=")) {
    exports.push(exportEq);
  }
  const facts = [];
  for (const sym of exports) {
    const target = sym.getFlags() & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(sym) : sym;
    const decl =
      target.valueDeclaration || (target.declarations && target.declarations[0]) || sourceFile;
    const type = checker.getTypeOfSymbolAtLocation(target, decl);
    const typeStr = renderType(checker, type, decl);
    facts.push({
      file: path.posix.relative(VIRTUAL_ROOT, normalizePath(sourceFile.fileName)),
      name: sym.getName(),
      type: typeStr,
    });
  }
  return facts;
}

function collectMarkerTypes(checker, markers, sourceFiles) {
  const facts = [];
  for (const marker of markers) {
    const absName = toAbsolute(marker.file);
    const sf = sourceFiles.get(absName);
    if (!sf) continue;
    const offsetUtf16 = utf8ByteOffsetToUtf16(sf.text, marker.offset);
    // Prefer the token at the marker position so `^?` aligned to the start of
    // an identifier resolves that identifier (as `findPrecedingToken` treats
    // the position as an exclusive bound). However, `getTokenAtPosition`
    // returns the next token when the offset lands inside trivia/whitespace, so
    // fall back to the preceding token in that case.
    let node = ts.getTokenAtPosition(sf, offsetUtf16);
    if (!node) {
      node = ts.findPrecedingToken(offsetUtf16, sf);
    } else if (node.getStart(sf) > offsetUtf16) {
      node = ts.findPrecedingToken(offsetUtf16, sf) ?? node;
    }
    if (!node) continue;
    const type = checker.getTypeAtLocation(node);
    const typeStr = renderType(checker, type, node);
    facts.push({
      file: path.posix.relative(VIRTUAL_ROOT, normalizePath(sf.fileName)),
      offset: marker.offset,
      line: marker.line,
      column: marker.column,
      type: typeStr,
    });
  }
  return facts;
}

function collectTypeFacts(program, checker, markers, requestFiles) {
  const sourceFiles = new Map();
  for (const sf of program.getSourceFiles()) {
    sourceFiles.set(normalizePath(sf.fileName), sf);
  }
  const exports = [];
  for (const rawName of Object.keys(requestFiles || {})) {
    const absName = toAbsolute(rawName);
    const sf = sourceFiles.get(absName);
    if (!sf) continue;
    exports.push(...collectExportTypes(checker, sf));
  }
  const markerFacts = collectMarkerTypes(checker, markers, sourceFiles);
  if (exports.length === 0 && markerFacts.length === 0) {
    return null;
  }
  return { exports, markers: markerFacts };
}

function serializeDiagnostic(diagnostic) {
  const startUtf16 = diagnostic.start ?? 0;
  const endUtf16 = (diagnostic.start ?? 0) + (diagnostic.length ?? 0);
  const fileName = diagnostic.file ? toAbsolute(diagnostic.file.fileName) : null;
  const relative = fileName ? path.posix.relative(VIRTUAL_ROOT, fileName) : null;
  const text = diagnostic.file?.text;
  const start = text ? utf16ToUtf8ByteOffset(text, startUtf16) : startUtf16;
  const end = text ? utf16ToUtf8ByteOffset(text, endUtf16) : endUtf16;

  return {
    code: diagnostic.code,
    category: categoryToString(diagnostic.category),
    file: relative,
    start,
    end,
    message: flattenMessage(diagnostic.messageText),
  };
}

function serializeDiagnostics(diagnostics) {
  return diagnostics.map(serializeDiagnostic);
}

function parseOptions(rawOptions) {
  const defaults = { noEmit: true, skipLibCheck: true, pretty: false };
  const merged = { ...defaults, ...(rawOptions ?? {}) };
  const converted = ts.convertCompilerOptionsFromJson(merged, VIRTUAL_ROOT);
  return {
    options: converted.options ?? {},
    errors: converted.errors ?? [],
  };
}

function createInMemoryHost(files, options) {
  const defaultHost = ts.createCompilerHost(options, true);
  const normalizedFiles = new Map();
  const entries = Object.entries(files || {});
  entries.sort(([a], [b]) => a.localeCompare(b));
  for (const [rawName, text] of entries) {
    normalizedFiles.set(toAbsolute(rawName), text);
  }
  const virtualDirectories = collectVirtualDirectories(normalizedFiles.keys());

  const getSourceFile = (fileName, languageVersion, onError) => {
    const normalized = toAbsolute(fileName);
    const text = normalizedFiles.get(normalized);
    if (text !== undefined) {
      const scriptKind = ts.getScriptKindFromFileName(normalized);
      const target = languageVersion ?? options.target ?? ts.ScriptTarget.Latest;
      return ts.createSourceFile(normalized, text, target, true, scriptKind);
    }
    if (!isAllowedDiskPath(normalized)) {
      return undefined;
    }

    const diskText = defaultHost.readFile(toDiskPath(normalized));
    if (diskText === undefined) {
      if (onError) {
        onError(`File not found: ${normalized}`);
      }
      return undefined;
    }
    const scriptKind = ts.getScriptKindFromFileName(normalized);
    const target = languageVersion ?? options.target ?? ts.ScriptTarget.Latest;
    return ts.createSourceFile(normalized, diskText, target, true, scriptKind);
  };

  const host = {
    ...defaultHost,
    getCurrentDirectory: () => VIRTUAL_ROOT,
    getCanonicalFileName: (fileName) => normalizePath(fileName),
    fileExists: (fileName) => {
      const absolute = toAbsolute(fileName);
      return (
        normalizedFiles.has(absolute) ||
        (isAllowedDiskPath(absolute) && defaultHost.fileExists(toDiskPath(absolute)))
      );
    },
    readFile: (fileName) => {
      const absolute = toAbsolute(fileName);
      if (normalizedFiles.has(absolute)) {
        return normalizedFiles.get(absolute);
      }
      if (!isAllowedDiskPath(absolute)) {
        return undefined;
      }
      return defaultHost.readFile(toDiskPath(absolute));
    },
    directoryExists: (dirName) => {
      const absolute = toAbsolute(dirName);
      return (
        virtualDirectories.has(absolute) ||
        (isAllowedDiskPath(absolute) &&
          (defaultHost.directoryExists?.(toDiskPath(absolute)) ?? false))
      );
    },
    getDirectories: (dirName) => {
      const absolute = toAbsolute(dirName);
      const fromDefault = isAllowedDiskPath(absolute)
        ? defaultHost.getDirectories?.(toDiskPath(absolute)) ?? []
        : [];
      const virtual = listVirtualSubdirectories(absolute, virtualDirectories);
      return Array.from(new Set([...fromDefault, ...virtual]));
    },
    readDirectory: (rootDir, extensions, excludes, includes, depth) => {
      const absolute = toAbsolute(rootDir);
      if (!isAllowedDiskPath(absolute)) {
        return [];
      }
      const results =
        defaultHost.readDirectory?.(
          toDiskPath(absolute),
          extensions,
          excludes,
          includes,
          depth,
        ) ?? [];
      return results.map(toAbsolute);
    },
    realpath: (p) => {
      const absolute = toAbsolute(p);
      if (!isAllowedDiskPath(absolute)) {
        return absolute;
      }
      const diskPath = toDiskPath(absolute);
      const real = defaultHost.realpath ? defaultHost.realpath(diskPath) : diskPath;
      return toAbsolute(real);
    },
    getSourceFileByPath: (fileName, filePath, languageVersion, onError) =>
      // Ignore the cache key and treat it as the resolved file name.
      getSourceFile(fileName, languageVersion, onError),
    getSourceFile,
    writeFile: () => {},
  };
  return host;
}

function createTraceCollector() {
  const lines = [];
  let buffer = "";

  function pushLine(line) {
    const normalized = line.replace(/\r$/, "");
    if (normalized.trim().length === 0) {
      return;
    }
    lines.push(normalized);
  }

  function write(chunk) {
    buffer += String(chunk ?? "");
    // Split on `\n` but keep any remainder buffered so we correctly handle
    // partial writes (ts.sys.write can write without trailing newlines).
    while (true) {
      const index = buffer.indexOf("\n");
      if (index === -1) {
        break;
      }
      const line = buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
      pushLine(line);
    }
  }

  function finalize() {
    if (buffer.length > 0) {
      pushLine(buffer);
      buffer = "";
    }
  }

  return { write, finalize, lines };
}

function runRequest(request) {
  const { options, errors: optionErrors } = parseOptions(request.options);
  const rootNames = (request.rootNames ?? []).map(toAbsolute);
  const host = createInMemoryHost(request.files ?? {}, options);

  // Harness-level structured resolution tracing. This is opt-in to avoid slowing
  // down normal runs.
  const traceResolutionRequest =
    request.traceResolution === true || request.trace_resolution === true;
  const resolutionTraceCollector = traceResolutionRequest
    ? createResolutionTraceCollector(moduleResolutionModeString(options))
    : null;
  if (resolutionTraceCollector) {
    installResolutionTracing(host, options, resolutionTraceCollector);
  }

  const diagnosticsOnly = request.diagnosticsOnly === true || request.diagnostics_only === true;
  // TypeScript also supports a `traceResolution` compiler option which emits a
  // verbose text trace. When enabled via compiler options, capture that output
  // (primarily useful for debugging).
  const traceResolutionOption = options.traceResolution === true;

  let program;
  let diagnostics;
  let typeFacts = null;
  let traceResolutionLog = null;

  if (traceResolutionOption) {
    const trace = createTraceCollector();
    const originalSysWrite = ts.sys.write;
    const originalHostTrace = host.trace;
    const originalConsoleLog = console.log;
    const originalConsoleError = console.error;
    const originalConsoleInfo = console.info;
    const originalConsoleWarn = console.warn;

    const writeLine = (...args) => trace.write(`${args.map(String).join(" ")}\n`);

    ts.sys.write = trace.write;
    host.trace = trace.write;
    console.log = writeLine;
    console.error = writeLine;
    console.info = writeLine;
    console.warn = writeLine;

    try {
      program = ts.createProgram({ rootNames, options, host });
      diagnostics = [...optionErrors, ...ts.getPreEmitDiagnostics(program)];

      if (!diagnosticsOnly) {
        const providedMarkers =
          (request.type_queries && request.type_queries.length
            ? request.type_queries
            : request.typeQueries && request.typeQueries.length
              ? request.typeQueries
              : null) ?? null;
        const markers =
          providedMarkers && providedMarkers.length
            ? providedMarkers
            : collectTypeQueries(request.files);
        const checker = program.getTypeChecker();
        typeFacts = collectTypeFacts(program, checker, markers, request.files ?? {});
      }
    } finally {
      trace.finalize();
      traceResolutionLog = trace.lines;
      ts.sys.write = originalSysWrite;
      host.trace = originalHostTrace;
      console.log = originalConsoleLog;
      console.error = originalConsoleError;
      console.info = originalConsoleInfo;
      console.warn = originalConsoleWarn;
    }
  } else {
    program = ts.createProgram({ rootNames, options, host });
    diagnostics = [...optionErrors, ...ts.getPreEmitDiagnostics(program)];

    if (!diagnosticsOnly) {
      const providedMarkers =
        (request.type_queries && request.type_queries.length
          ? request.type_queries
          : request.typeQueries && request.typeQueries.length
            ? request.typeQueries
            : null) ?? null;
      const markers =
        providedMarkers && providedMarkers.length
          ? providedMarkers
          : collectTypeQueries(request.files);
      const checker = program.getTypeChecker();
      typeFacts = collectTypeFacts(program, checker, markers, request.files ?? {});
    }
  }

  const response = {
    schemaVersion: SCHEMA_VERSION,
    metadata: {
      typescriptVersion: ts.version,
      options,
    },
    diagnostics: serializeDiagnostics(diagnostics),
  };
  if (traceResolutionOption) {
    response.traceResolutionLog = traceResolutionLog ?? [];
  }
  if (typeFacts) {
    response.type_facts = typeFacts;
  }
  if (resolutionTraceCollector) {
    response.resolutionTrace = finalizeResolutionTrace(resolutionTraceCollector);
  }
  return response;
}

function respond(payload) {
  process.stdout.write(`${JSON.stringify(payload)}\n`);
}

function main() {
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });

  rl.on("line", (line) => {
    if (!line.trim()) {
      return;
    }

    let request;
    try {
      request = JSON.parse(line);
    } catch (err) {
      respond({
        diagnostics: [],
        crash: { message: `invalid JSON input: ${err?.message ?? String(err)}` },
      });
      return;
    }

    try {
      const result = runRequest(request);
      respond(result);
    } catch (err) {
      respond({
        diagnostics: [],
        crash: {
          message: err?.message ?? String(err),
          stack: err?.stack,
        },
      });
    }
  });

  rl.on("close", () => process.exit(0));
}

main();
