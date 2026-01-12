import {
  Validator,
  ValuePath,
  VArray,
  VBoolean,
  VFiniteNumber,
  VInteger,
  VOptional,
  VString,
  VStringEnum,
  VStruct,
  VTagged,
  VUnion,
  VUnknown,
} from "@wzlin/valid";

//
// Legacy ("/compile") schema
//

export type StableId =
  | {
      type: "number";
      value: string;
    }
  | { type: "text"; value: string };

export type StableConst =
  | { kind: "null" | "undefined" }
  | { kind: "big_int"; value: string }
  | { kind: "bool"; value: boolean }
  | { kind: "num"; value: number }
  | { kind: "str"; value: string };

export type StableArg =
  | { kind: "builtin"; value: string }
  | { kind: "const"; value: StableConst }
  | { kind: "fn"; value: number }
  | { kind: "var"; value: number };

export type StableEffectLocation =
  | { kind: "heap" }
  | { kind: "foreign"; id: StableId }
  | { kind: "unknown"; name: string };

export type EffectSummary = {
  flags: string;
  throws: "never" | "maybe" | "always";
};

export type StableEffects = {
  reads?: StableEffectLocation[];
  writes?: StableEffectLocation[];
  summary: EffectSummary;
  unknown?: boolean;
};

export type StablePurity = "pure" | "read_only" | "allocating" | "impure";

export type StableOwnershipState = "owned" | "borrowed" | "shared" | "unknown";

export type StableEscapeState =
  | { kind: "no_escape" }
  | { kind: "arg_escape"; value: number }
  | { kind: "return_escape" }
  | { kind: "global_escape" }
  | { kind: "unknown" };

export type StableInstMeta = {
  effects?: StableEffects;
  purity?: StablePurity;
  calleePurity?: StablePurity;
  resultEscape?: StableEscapeState;
  ownership?: StableOwnershipState;
  typeId?: StableId;
  nativeLayout?: StableId;
};

export type StableInst = {
  t: string;
  tgts: number[];
  args: StableArg[];
  spreads: number[];
  labels: number[];
  binOp?: string;
  unOp?: string;
  foreign?: StableId;
  unknown?: string;
  meta?: StableInstMeta;
};

export type StableDebugStep = {
  name: string;
  bblockOrder: number[];
  bblocks: Map<number, StableInst[]> | Record<string, StableInst[]>;
  cfgChildren: Map<number, number[]> | Record<string, number[]>;
};

export type StableDebug = {
  steps: StableDebugStep[];
};

export type StableCfg = {
  bblockOrder: number[];
  bblocks: Map<number, StableInst[]>;
  cfgChildren: Map<number, number[]>;
};

export type StableFunction = {
  debug: StableDebug;
  cfg: StableCfg;
};

export type StableProgramSymbol = {
  id: StableId;
  name: string;
  scope: StableId;
  captured: boolean;
};

export type StableFreeSymbols = {
  top_level: StableId[];
  functions: StableId[][];
};

export type StableScope = {
  id: StableId;
  parent?: StableId;
  kind: string;
  symbols?: StableId[];
  children?: StableId[];
  tdz_bindings?: StableId[];
  is_dynamic: boolean;
  has_direct_eval: boolean;
};

export type StableProgramSymbols = {
  symbols: StableProgramSymbol[];
  free_symbols?: StableFreeSymbols;
  names: string[];
  scopes: StableScope[];
};

export type CompileProgramDumpV1 = {
  functions: StableFunction[];
  top_level: StableFunction;
  symbols?: StableProgramSymbols;
};

export type CompileProgramDump = {
  version: "v1";
  program: CompileProgramDumpV1;
};

//
// Program dump ("/compile_dump") schema
//

export type SourceModeDump = "module" | "script" | "global";

export type NullabilityFactDump = {
  mayBeNull: boolean;
  mayBeUndefined: boolean;
  mayBeOther: boolean;
  isBottom: boolean;
};

export type DumpInstMeta = {
  effects: {
    reads: unknown[];
    writes: unknown[];
    summary: unknown;
    unknown: boolean;
  };
  purity: string;
  calleePurity: string;
  ownership: string;
  argUseModes: string[];
  inPlaceHint?: unknown;
  resultEscape?: unknown;
  range?: unknown;
  nullability?: NullabilityFactDump;
  encoding?: string;
  typeId?: string;
  typeSummary?: string;
  excludesNullish: boolean;
  nativeLayout?: string;
  span?: { start: number; end: number };
  preserveVarAssign?: boolean;
  stackAllocCandidate?: boolean;
  awaitKnownResolved?: boolean;
  awaitBehavior?: unknown;
  parallel?: unknown;
  nullabilityNarrowing?: unknown;
  value?: unknown;
  layoutId?: number;
  hirExpr?: number;
};

export type DumpInst = {
  t: string;
  tgts: number[];
  args: StableArg[];
  spreads: number[];
  labels: number[];
  binOp?: string;
  unOp?: string;
  foreign?: StableId | number;
  unknown?: string;
  meta: DumpInstMeta;
};

export type DumpCfg = {
  entry: number;
  bblockOrder: number[];
  bblocks: Map<number, DumpInst[]>;
  cfgEdges: Map<number, number[]>;
};

export type DumpFunction = {
  id?: number;
  params: number[];
  cfg: DumpCfg;
  cfgDeconstructed?: DumpCfg;
};

export type ProgramDump = {
  version: number;
  sourceMode: SourceModeDump;
  topLevel: DumpFunction;
  functions: DumpFunction[];
  symbols?: unknown;
  analyses?: unknown;
};

//
// Shared / normalization
//

export type GraphInst = StableInst | DumpInst;

export type NormalizedBlock = {
  label: number;
  insts: GraphInst[];
};

export type NormalizedStep = {
  name: string;
  blocks: NormalizedBlock[];
  children: Map<number, number[]>;
};

//
// Validators
//

class VObjectMapAsMap<K, V> extends Validator<Map<K, V>> {
  constructor(
    private readonly key: Validator<K>,
    private readonly value: Validator<V>,
  ) {
    super(new Map());
  }

  parse(theValue: ValuePath, raw: unknown): Map<K, V> {
    if (raw instanceof Map) {
      return new Map(
        [...raw.entries()].map(([k, v]) => [
          this.key.parse(theValue.andThen(String(k)), k),
          this.value.parse(theValue.andThen(String(k)), v),
        ]),
      );
    }
    if (typeof raw != "object" || !raw) {
      throw theValue.isBadAsIt("is not an object");
    }
    return new Map(
      Object.entries(raw).map(([k, v]) => [
        this.key.parse(theValue.andThen(k), k),
        this.value.parse(theValue.andThen(k), v),
      ]),
    );
  }
}

const vId = new VStruct({
  type: new VStringEnum({
    number: "number",
    text: "text",
  }),
  value: new VString(),
});

const vConst = new VTagged("kind", {
  big_int: new VStruct({ value: new VString() }),
  bool: new VStruct({ value: new VBoolean() }),
  num: new VStruct({ value: new VFiniteNumber() }),
  str: new VStruct({ value: new VString() }),
  null: new VStruct({}),
  undefined: new VStruct({}),
});

const vArg = new VTagged("kind", {
  builtin: new VStruct({ value: new VString() }),
  const: new VStruct({ value: vConst }),
  fn: new VStruct({ value: new VInteger() }),
  var: new VStruct({ value: new VInteger() }),
});

// /compile meta
const vEffectLocation = new VTagged("kind", {
  heap: new VStruct({}),
  foreign: new VStruct({ id: vId }),
  unknown: new VStruct({ name: new VString() }),
});

const vEffectSummary = new VStruct({
  flags: new VString(),
  throws: new VStringEnum({
    never: "never",
    maybe: "maybe",
    always: "always",
  }),
});

const vEffects = new VStruct({
  reads: new VOptional(new VArray(vEffectLocation)),
  writes: new VOptional(new VArray(vEffectLocation)),
  summary: vEffectSummary,
  unknown: new VOptional(new VBoolean()),
});

const vPurity = new VStringEnum({
  pure: "pure",
  read_only: "read_only",
  allocating: "allocating",
  impure: "impure",
});

const vOwnershipState = new VStringEnum({
  owned: "owned",
  borrowed: "borrowed",
  shared: "shared",
  unknown: "unknown",
});

const vEscapeState = new VTagged("kind", {
  no_escape: new VStruct({}),
  arg_escape: new VStruct({ value: new VInteger() }),
  return_escape: new VStruct({}),
  global_escape: new VStruct({}),
  unknown: new VStruct({}),
});

const vStableInstMeta = new VStruct({
  effects: new VOptional(vEffects),
  purity: new VOptional(vPurity),
  calleePurity: new VOptional(vPurity),
  resultEscape: new VOptional(vEscapeState),
  ownership: new VOptional(vOwnershipState),
  typeId: new VOptional(vId),
  nativeLayout: new VOptional(vId),
});

const vStableInst = new VStruct({
  t: new VString(),
  tgts: new VArray(new VInteger()),
  args: new VArray(vArg),
  spreads: new VArray(new VInteger()),
  labels: new VArray(new VInteger()),
  binOp: new VOptional(new VString()),
  unOp: new VOptional(new VString()),
  foreign: new VOptional(vId),
  unknown: new VOptional(new VString()),
  meta: new VOptional(vStableInstMeta),
});

const vDebugStep = new VStruct({
  name: new VString(),
  bblockOrder: new VArray(new VInteger()),
  bblocks: new VObjectMapAsMap(new VInteger(), new VArray(vStableInst)),
  cfgChildren: new VObjectMapAsMap(new VInteger(), new VArray(new VInteger())),
});

const vDebug = new VStruct({
  steps: new VArray(vDebugStep),
});

const vCfg = new VStruct({
  bblockOrder: new VArray(new VInteger()),
  bblocks: new VObjectMapAsMap(new VInteger(), new VArray(vStableInst)),
  cfgChildren: new VObjectMapAsMap(new VInteger(), new VArray(new VInteger())),
});

const vFunction = new VStruct({
  debug: vDebug,
  cfg: vCfg,
});

const vProgramSymbol = new VStruct({
  id: vId,
  name: new VString(),
  scope: vId,
  captured: new VBoolean(),
});

const vFreeSymbols = new VStruct({
  top_level: new VArray(vId),
  functions: new VArray(new VArray(vId)),
});

const vScope = new VStruct({
  id: vId,
  parent: new VOptional(vId),
  kind: new VString(),
  symbols: new VOptional(new VArray(vId)),
  children: new VOptional(new VArray(vId)),
  tdz_bindings: new VOptional(new VArray(vId)),
  is_dynamic: new VBoolean(),
  has_direct_eval: new VBoolean(),
});

const vProgramSymbols = new VStruct({
  symbols: new VArray(vProgramSymbol),
  free_symbols: new VOptional(vFreeSymbols),
  names: new VArray(new VString()),
  scopes: new VArray(vScope),
});

const vCompileProgram = new VStruct({
  functions: new VArray(vFunction),
  top_level: vFunction,
  symbols: new VOptional(vProgramSymbols),
});

const vCompileProgramDump = new VStruct({
  version: new VStringEnum({ v1: "v1" }),
  program: vCompileProgram,
});

export const parseCompileProgramDump = (raw: unknown): CompileProgramDump =>
  vCompileProgramDump.parseRoot(raw);

export const parseCompileProgram = (raw: unknown): CompileProgramDumpV1 =>
  parseCompileProgramDump(raw).program;

// /compile_dump
const vDumpEffectSet = new VStruct({
  reads: new VArray(new VUnknown()),
  writes: new VArray(new VUnknown()),
  summary: new VUnknown(),
  unknown: new VBoolean(),
});

const vNullabilityFact = new VStruct({
  mayBeNull: new VBoolean(),
  mayBeUndefined: new VBoolean(),
  mayBeOther: new VBoolean(),
  isBottom: new VBoolean(),
});

const vSpan = new VStruct({
  start: new VInteger(),
  end: new VInteger(),
});

const vDumpInstMeta = new VStruct({
  effects: vDumpEffectSet,
  purity: new VString(),
  calleePurity: new VString(),
  ownership: new VString(),
  argUseModes: new VArray(new VString()),
  inPlaceHint: new VOptional(new VUnknown()),
  resultEscape: new VOptional(new VUnknown()),
  range: new VOptional(new VUnknown()),
  nullability: new VOptional(vNullabilityFact),
  encoding: new VOptional(new VString()),
  typeId: new VOptional(new VString()),
  typeSummary: new VOptional(new VString()),
  excludesNullish: new VBoolean(),
  nativeLayout: new VOptional(new VString()),
  span: new VOptional(vSpan),
  preserveVarAssign: new VOptional(new VBoolean()),
  stackAllocCandidate: new VOptional(new VBoolean()),
  awaitKnownResolved: new VOptional(new VBoolean()),
  awaitBehavior: new VOptional(new VUnknown()),
  parallel: new VOptional(new VUnknown()),
  nullabilityNarrowing: new VOptional(new VUnknown()),
  value: new VOptional(new VUnknown()),
  layoutId: new VOptional(new VInteger()),
  hirExpr: new VOptional(new VInteger()),
});

const vForeign = new VUnion(vId, new VInteger());

const vDumpInst = new VStruct({
  t: new VString(),
  tgts: new VArray(new VInteger()),
  args: new VArray(vArg),
  spreads: new VArray(new VInteger()),
  labels: new VArray(new VInteger()),
  binOp: new VOptional(new VString()),
  unOp: new VOptional(new VString()),
  foreign: new VOptional(vForeign),
  unknown: new VOptional(new VString()),
  meta: vDumpInstMeta,
});

const vDumpCfg = new VStruct({
  entry: new VInteger(),
  bblockOrder: new VArray(new VInteger()),
  bblocks: new VObjectMapAsMap(new VInteger(), new VArray(vDumpInst)),
  cfgEdges: new VObjectMapAsMap(new VInteger(), new VArray(new VInteger())),
});

const vDumpFunction = new VStruct({
  id: new VOptional(new VInteger()),
  params: new VArray(new VInteger()),
  cfg: vDumpCfg,
  cfgDeconstructed: new VOptional(vDumpCfg),
});

const vProgramDump = new VStruct({
  version: new VInteger(),
  sourceMode: new VStringEnum({
    module: "module",
    script: "script",
    global: "global",
  }),
  topLevel: vDumpFunction,
  functions: new VArray(vDumpFunction),
  symbols: new VOptional(new VUnknown()),
  analyses: new VOptional(new VUnknown()),
});

export const parseProgramDump = (raw: unknown): ProgramDump => vProgramDump.parseRoot(raw);

export const formatId = (id: StableId): string => id.value;

export const formatForeign = (id: StableId | number): string =>
  typeof id === "number" ? `${id}` : formatId(id);

export const constToLabel = (value: StableConst): string => {
  switch (value.kind) {
    case "null":
      return "null";
    case "undefined":
      return "undefined";
    case "big_int":
      return `${BigInt(value.value).toString()}n`;
    case "bool":
      return value.value ? "true" : "false";
    case "num":
      return `${value.value}`;
    case "str":
      return JSON.stringify(value.value);
  }
};

export const argToLabel = (arg: StableArg): string => {
  switch (arg.kind) {
    case "builtin":
      return arg.value;
    case "const":
      return constToLabel(arg.value);
    case "fn":
      return `Fn${arg.value}`;
    case "var":
      return `%${arg.value}`;
  }
};

export const buildSymbolNames = (
  symbols?: StableProgramSymbols,
): Map<string, string> | undefined => {
  if (!symbols) {
    return undefined;
  }
  const map = new Map<string, string>();
  for (const symbol of symbols.symbols) {
    map.set(formatId(symbol.id), symbol.name);
  }
  return map;
};

const normalizeBBlockEntries = (blocks: any): Array<[number, GraphInst[]]> =>
  blocks instanceof Map
    ? [...blocks.entries()]
    : Object.entries(blocks).map(([k, v]) => [Number(k), v as GraphInst[]]);

const normalizeChildEntries = (children: any): Map<number, number[]> =>
  children instanceof Map
    ? children
    : new Map(
        Object.entries(children).map(([k, v]) => [
          Number(k),
          (v as any[]).map((n) => Number(n)),
        ]),
      );

export const normalizeStep = (step: StableDebugStep): NormalizedStep => {
  const blocks = normalizeBBlockEntries(step.bblocks)
    .map(([label, insts]) => ({ label, insts }))
    .sort((a, b) => a.label - b.label);
  return {
    name: step.name,
    blocks,
    children: normalizeChildEntries(step.cfgChildren),
  };
};

export const normalizeCfg = (name: string, cfg: DumpCfg): NormalizedStep => {
  const blocks = normalizeBBlockEntries(cfg.bblocks)
    .map(([label, insts]) => ({ label, insts }))
    .sort((a, b) => a.label - b.label);
  return {
    name,
    blocks,
    children: normalizeChildEntries(cfg.cfgEdges),
  };
};

const instMatchesQuery = (
  inst: GraphInst,
  query: string,
  symbolNames?: Map<string, string>,
): boolean => {
  if (inst.t.toLowerCase().includes(query)) {
    return true;
  }
  if ((inst as any).unknown?.toLowerCase().includes(query)) {
    return true;
  }
  const foreign = (inst as any).foreign;
  if (foreign != undefined) {
    const key = formatForeign(foreign);
    if (key.toLowerCase().includes(query)) {
      return true;
    }
    const name = symbolNames?.get(key);
    if (name && name.toLowerCase().includes(query)) {
      return true;
    }
  }
  const args = [
    ...inst.tgts.map((t) => `%${t}`),
    ...inst.labels.map((l) => `:${l}`),
    ...inst.args.map(argToLabel),
  ];
  return args.some((arg) => arg.toLowerCase().includes(query));
};

export const blockMatchesQuery = (
  block: NormalizedBlock,
  query: string,
  symbolNames?: Map<string, string>,
): boolean => {
  if (!query) {
    return true;
  }
  if (`${block.label}`.includes(query)) {
    return true;
  }
  return block.insts.some((inst) => instMatchesQuery(inst, query, symbolNames));
};

const instSignature = (inst: GraphInst): string =>
  JSON.stringify({
    t: inst.t,
    tgts: inst.tgts,
    args: inst.args.map(argToLabel),
    spreads: inst.spreads,
    labels: inst.labels,
    binOp: (inst as any).binOp,
    unOp: (inst as any).unOp,
    foreign: (inst as any).foreign == undefined ? undefined : formatForeign((inst as any).foreign),
    unknown: (inst as any).unknown,
    meta: (inst as any).meta,
  });

export const computeChangedBlocks = (steps: NormalizedStep[]): Array<Set<number>> => {
  const results: Array<Set<number>> = [];
  for (let i = 0; i < steps.length; i++) {
    const current = steps[i];
    const prev = steps[i - 1];
    const changed = new Set<number>();
    if (!prev) {
      current.blocks.forEach((b) => changed.add(b.label));
    } else {
      const prevMap = new Map(prev.blocks.map((b) => [b.label, b.insts]));
      for (const block of current.blocks) {
        const prevInsts = prevMap.get(block.label);
        if (!prevInsts) {
          changed.add(block.label);
          continue;
        }
        const sigA = block.insts.map(instSignature).join("|");
        const sigB = prevInsts.map(instSignature).join("|");
        if (sigA !== sigB) {
          changed.add(block.label);
        }
      }
    }
    results.push(changed);
  }
  return results;
};
