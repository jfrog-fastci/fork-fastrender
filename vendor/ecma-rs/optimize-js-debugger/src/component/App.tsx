import { Editor } from "@monaco-editor/react";
import { decode, encode } from "@msgpack/msgpack";
import { useEffect, useMemo, useRef, useState } from "react";
import { Graph } from "./Graph";
import { InstMetaPanel } from "./InstMetaPanel";
import { SymbolsPanel } from "./SymbolsPanel";
import "./App.css";
import { utf8ByteOffsetToUtf16Offset } from "./textOffset";
import {
  CompileProgramDumpV1,
  GraphInst,
  NormalizedStep,
  ProgramDump,
  buildSymbolNames,
  computeChangedBlocks,
  formatId,
  normalizeCfg,
  normalizeStep,
  parseCompileProgram,
  parseProgramDump,
} from "./schema";

const INIT_SOURCE = `
(() => {
  a?.b?.c;
  let x = 1;
  if (x) {
    g();
    x += Math.round(1.1);
    for (;;) {
      x += 1;
      setTimeout(() => {
        h(x);
      }, 1000);
    }
  }
  f(x);
})();
`.trim();

const errorMessage = (raw: unknown, status: number): string => {
  if (Array.isArray((raw as any)?.diagnostics)) {
    return (raw as any).diagnostics.map((d: any) => `${d.code}: ${d.message}`).join("\n");
  }
  return `HTTP ${status}`;
};

export const App = () => {
  const [source, setSource] = useState(INIT_SOURCE);
  const [dump, setDump] = useState<ProgramDump>();
  const [compileProgram, setCompileProgram] = useState<CompileProgramDumpV1>();
  const [curFnId, setCurFnId] = useState<number>();
  const [error, setError] = useState<string>();
  const [isGlobal, setIsGlobal] = useState(true);
  const [filter, setFilter] = useState("");
  const [stepIdx, setStepIdx] = useState(0);
  const [view, setView] = useState<"analysis" | "steps" | "symbols">("analysis");
  const [showDiff, setShowDiff] = useState(true);
  const [typed, setTyped] = useState(false);
  const [semanticOps, setSemanticOps] = useState(false);
  const [runAnalyses, setRunAnalyses] = useState(true);
  const [onlyUnknownEffects, setOnlyUnknownEffects] = useState(false);
  const [onlyEscapingAllocs, setOnlyEscapingAllocs] = useState(false);
  const [analysisCfg, setAnalysisCfg] = useState<"ssa" | "deconstructed">("ssa");
  const [hoveredInst, setHoveredInst] = useState<GraphInst>();
  const [selectedInst, setSelectedInst] = useState<GraphInst>();

  const editorRef = useRef<any>();
  const monacoRef = useRef<any>();
  const spanDecorationsRef = useRef<string[]>([]);

  const hoveredSpan = useMemo(() => {
    const span = (selectedInst as any)?.meta?.span ?? (hoveredInst as any)?.meta?.span;
    if (
      span &&
      typeof span === "object" &&
      typeof (span as any).start === "number" &&
      typeof (span as any).end === "number"
    ) {
      return { start: (span as any).start as number, end: (span as any).end as number };
    }
    return undefined;
  }, [hoveredInst]);

  useEffect(() => {
    const editor = editorRef.current;
    const monaco = monacoRef.current;
    if (!editor || !monaco) {
      return;
    }
    const model = editor.getModel?.();
    if (!model) {
      return;
    }

    const nextDecorations =
      hoveredSpan && hoveredSpan.end > hoveredSpan.start
        ? (() => {
            const start = utf8ByteOffsetToUtf16Offset(source, hoveredSpan.start);
            const end = utf8ByteOffsetToUtf16Offset(source, hoveredSpan.end);
            const startPos = model.getPositionAt(start);
            const endPos = model.getPositionAt(end);
            const range = new monaco.Range(
              startPos.lineNumber,
              startPos.column,
              endPos.lineNumber,
              endPos.column,
            );
            return [
              {
                range,
                options: {
                  inlineClassName: "inst-span-decoration",
                },
              },
            ];
          })()
        : [];

    spanDecorationsRef.current = editor.deltaDecorations(
      spanDecorationsRef.current,
      nextDecorations,
    );
  }, [hoveredSpan, source]);

  useEffect(() => {
    const src = source;
    if (!src.trim()) {
      return;
    }
    const ac = new AbortController();

    const fetchDump = async (): Promise<ProgramDump> => {
      const res = await fetch("//localhost:3001/compile_dump", {
        signal: ac.signal,
        method: "POST",
        headers: {
          "Content-Type": "application/msgpack",
        },
        body: encode({
          source: src,
          is_global: isGlobal,
          typed,
          semantic_ops: semanticOps,
          run_analyses: runAnalyses,
        }),
      });
      const raw = decode(await res.arrayBuffer());
      if (!res.ok) {
        throw new Error(`compile_dump: ${errorMessage(raw, res.status)}`);
      }
      return parseProgramDump(raw);
    };

    const fetchCompile = async (): Promise<CompileProgramDumpV1> => {
      const res = await fetch("//localhost:3001/compile", {
        signal: ac.signal,
        method: "POST",
        headers: {
          "Content-Type": "application/msgpack",
        },
        body: encode({
          source: src,
          is_global: isGlobal,
        }),
      });
      const raw = decode(await res.arrayBuffer());
      if (!res.ok) {
        throw new Error(`compile: ${errorMessage(raw, res.status)}`);
      }
      return parseCompileProgram(raw);
    };

    (async () => {
      try {
        const [dumpResult, compileResult] = await Promise.allSettled([fetchDump(), fetchCompile()]);

        const errors: string[] = [];
        if (dumpResult.status === "fulfilled") {
          setDump(dumpResult.value);
        } else {
          setDump(undefined);
          errors.push(String(dumpResult.reason));
        }

        if (compileResult.status === "fulfilled") {
          setCompileProgram(compileResult.value);
        } else {
          setCompileProgram(undefined);
          errors.push(String(compileResult.reason));
        }

        setHoveredInst(undefined);
        setSelectedInst(undefined);
        setError(errors.length > 0 ? errors.join("\n") : undefined);
        setStepIdx(0);
      } catch (err) {
        if (err instanceof DOMException && err.name === "AbortError") {
          return;
        }
        console.error(err);
        setError(String(err));
        setDump(undefined);
        setCompileProgram(undefined);
        setCurFnId(undefined);
        setHoveredInst(undefined);
        setSelectedInst(undefined);
      }
    })();

    return () => ac.abort();
  }, [source, isGlobal, typed, semanticOps, runAnalyses]);

  const symbolNames = useMemo(() => buildSymbolNames(compileProgram?.symbols), [compileProgram]);

  const currentCompileFunction =
    curFnId == undefined ? compileProgram?.top_level : compileProgram?.functions[curFnId];

  const debugSteps: NormalizedStep[] = useMemo(
    () => currentCompileFunction?.debug.steps.map(normalizeStep) ?? [],
    [currentCompileFunction],
  );

  const diffs = useMemo(() => computeChangedBlocks(debugSteps), [debugSteps]);
  const safeStepIdx = debugSteps.length === 0 ? 0 : Math.min(stepIdx, debugSteps.length - 1);
  const currentDebugStep = debugSteps[safeStepIdx];

  const currentDumpFunction = curFnId == undefined ? dump?.topLevel : dump?.functions[curFnId];

  const analyzedStep: NormalizedStep | undefined = useMemo(() => {
    if (!currentDumpFunction) {
      return undefined;
    }
    const cfg =
      analysisCfg === "ssa"
        ? currentDumpFunction.cfg
        : currentDumpFunction.cfgDeconstructed ?? currentDumpFunction.cfg;
    return normalizeCfg(analysisCfg, cfg);
  }, [currentDumpFunction, analysisCfg]);

  useEffect(() => {
    const listener = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setSelectedInst(undefined);
        return;
      }
      if (view !== "steps" || debugSteps.length === 0) {
        return;
      }
      if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        setStepIdx((idx) => Math.max(0, idx - 1));
      } else if (e.key === "ArrowRight" || e.key === "ArrowDown") {
        setStepIdx((idx) => Math.min((debugSteps.length ?? 1) - 1, idx + 1));
      }
    };
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, [debugSteps.length, view]);

  const fnCount = Math.max(dump?.functions.length ?? 0, compileProgram?.functions.length ?? 0);
  const fnIds = [undefined, ...Array.from({ length: fnCount }, (_, i) => i)];

  const activeInst = selectedInst ?? hoveredInst;

  return (
    <div className="App">
      <main>
        <div className="canvas">
          <div className="toolbar">
            <div className="function-tabs">
              {fnIds.map((fnId) => (
                <button
                  key={fnId ?? -1}
                  className={fnId === curFnId ? "active" : ""}
                  onClick={() => {
                    setCurFnId(fnId);
                    setStepIdx(0);
                    setHoveredInst(undefined);
                    setSelectedInst(undefined);
                  }}
                >
                  {fnId == undefined ? "Top level" : `Fn${fnId}`}
                </button>
              ))}
            </div>
            <div className="step-controls">
              <label className="toggle">
                <input
                  type="radio"
                  name="view"
                  checked={view === "analysis"}
                  onChange={() => {
                    setView("analysis");
                    setHoveredInst(undefined);
                    setSelectedInst(undefined);
                  }}
                />
                Analyzed CFG
              </label>
              <label className="toggle">
                <input
                  type="radio"
                  name="view"
                  checked={view === "steps"}
                  onChange={() => {
                    setView("steps");
                    setHoveredInst(undefined);
                    setSelectedInst(undefined);
                  }}
                />
                Optimizer steps
              </label>
              <label className="toggle">
                <input
                  type="radio"
                  name="view"
                  checked={view === "symbols"}
                  onChange={() => {
                    setView("symbols");
                    setHoveredInst(undefined);
                    setSelectedInst(undefined);
                  }}
                />
                Symbols
              </label>
              {view === "analysis" && (
                <label>
                  CFG:
                  <select
                    value={analysisCfg}
                    onChange={(e) => {
                      setAnalysisCfg(e.target.value as any);
                      setHoveredInst(undefined);
                      setSelectedInst(undefined);
                    }}
                  >
                    <option value="ssa">ssa (analyzed)</option>
                    <option value="deconstructed">deconstructed</option>
                  </select>
                </label>
              )}
              {view === "steps" && (
                <>
                  <label>
                    Step:
                    <select value={safeStepIdx} onChange={(e) => setStepIdx(Number(e.target.value))}>
                      {debugSteps.map((step, i) => (
                        <option key={i} value={i}>
                          {i}. {step.name}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="toggle">
                    <input
                      type="checkbox"
                      checked={showDiff}
                      onChange={(e) => setShowDiff(e.target.checked)}
                    />
                    Highlight changed blocks
                  </label>
                </>
              )}
              <input
                type="search"
                placeholder="Filter symbol/temp/label"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
              />
              {view === "analysis" && (
                <>
                  <label className="toggle">
                    <input
                      type="checkbox"
                      checked={onlyUnknownEffects}
                      onChange={(e) => setOnlyUnknownEffects(e.target.checked)}
                    />
                    Unknown effects
                  </label>
                  <label className="toggle">
                    <input
                      type="checkbox"
                      checked={onlyEscapingAllocs}
                      onChange={(e) => setOnlyEscapingAllocs(e.target.checked)}
                    />
                    Escaping allocs
                  </label>
                </>
              )}
              {view === "steps" && currentDebugStep && (
                <span className="step-summary">
                  {currentDebugStep.blocks.length} blocks •{" "}
                  {showDiff && diffs[safeStepIdx] ? `${diffs[safeStepIdx].size} changed` : "diffs off"}
                </span>
              )}
            </div>
          </div>

          {view === "analysis" && analyzedStep && (
            <Graph
              step={analyzedStep}
              stepNames={[analyzedStep.name]}
              symbolNames={symbolNames}
              changed={undefined}
              filter={filter}
              onlyUnknownEffects={onlyUnknownEffects}
              onlyEscapingAllocs={onlyEscapingAllocs}
              onHoverInst={setHoveredInst}
              selectedInst={selectedInst}
              onSelectInst={(inst) =>
                setSelectedInst((cur) => (cur === inst ? undefined : inst))
              }
            />
          )}
          {view === "steps" && currentDebugStep && (
            <Graph
              step={currentDebugStep}
              stepNames={debugSteps.map((s) => s.name)}
              symbolNames={symbolNames}
              changed={showDiff ? diffs[safeStepIdx] : undefined}
              filter={filter}
              onHoverInst={setHoveredInst}
              selectedInst={selectedInst}
              onSelectInst={(inst) =>
                setSelectedInst((cur) => (cur === inst ? undefined : inst))
              }
            />
          )}
          {view === "symbols" && <SymbolsPanel symbols={compileProgram?.symbols} filter={filter} />}
        </div>
        <div className="pane">
          <div className="info">
            {error && <p className="error">{error}</p>}
            {compileProgram?.symbols && (
              <p className="symbol-summary">
                {compileProgram.symbols.symbols.length} symbols across {compileProgram.symbols.scopes.length} scopes
              </p>
            )}
          </div>
          <InstMetaPanel
            inst={activeInst}
            source={source}
            pinned={selectedInst != undefined}
          />
          <div className="editor">
            <div className="controls">
              <label>
                Top-level mode:
                <select
                  value={isGlobal ? "global" : "module"}
                  onChange={(e) => setIsGlobal(e.target.value === "global")}
                >
                  <option value="global">global</option>
                  <option value="module">module</option>
                </select>
              </label>
              <label className="toggle">
                <input type="checkbox" checked={runAnalyses} onChange={(e) => setRunAnalyses(e.target.checked)} />
                Run analyses
              </label>
              <label className="toggle">
                <input type="checkbox" checked={typed} onChange={(e) => setTyped(e.target.checked)} />
                Typed
              </label>
              <label className="toggle">
                <input
                  type="checkbox"
                  checked={semanticOps}
                  onChange={(e) => setSemanticOps(e.target.checked)}
                />
                Semantic ops
              </label>
            </div>
            <Editor
              height="50vh"
              width="40vw"
              defaultLanguage="javascript"
              defaultValue={INIT_SOURCE}
              onMount={(editor, monaco) => {
                editorRef.current = editor;
                monacoRef.current = monaco;
              }}
              onChange={(e) => setSource(e ?? "")}
            />
            {compileProgram?.symbols && (
              <div className="legend">
                <span>Foreign vars: </span>
                <span>
                  {[...compileProgram.symbols.symbols]
                    .filter((s) => s.captured)
                    .map((s) => formatId(s.id))
                    .join(", ") || "none"}
                </span>
              </div>
            )}
          </div>
        </div>
      </main>
    </div>
  );
};
