import { useMemo, useState } from "react";
import type { GraphInst } from "./schema";
import { utf8ByteOffsetToUtf16Offset } from "./textOffset";

const formatExternallyTagged = (value: unknown): string => {
  if (value == undefined) {
    return "n/a";
  }
  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (typeof value === "object" && value) {
    const entries = Object.entries(value as any);
    if (entries.length === 1) {
      const [k, v] = entries[0];
      if (v == undefined) {
        return k;
      }
      if (typeof v === "string" || typeof v === "number" || typeof v === "boolean") {
        return `${k}(${String(v)})`;
      }
      return `${k}(${JSON.stringify(v)})`;
    }
  }
  return JSON.stringify(value);
};

const formatEffectLocation = (loc: unknown): string => formatExternallyTagged(loc);

const formatRange = (range: unknown): string => {
  if (range == undefined) {
    return "n/a";
  }
  if (typeof range === "string") {
    switch (range) {
      case "Bottom":
        return "⊥";
      case "Unknown":
        return "⊤";
      default:
        return range;
    }
  }
  if (typeof range === "object" && range) {
    const entries = Object.entries(range as any);
    if (entries.length === 1 && entries[0][0] === "Interval") {
      const value = entries[0][1] as any;
      const bound = (b: any): string => {
        if (typeof b === "string") {
          if (b === "NegInf") return "-inf";
          if (b === "PosInf") return "+inf";
          return b;
        }
        if (typeof b === "object" && b) {
          const e = Object.entries(b);
          if (e.length === 1 && e[0][0] === "I64") {
            return String(e[0][1]);
          }
        }
        return formatExternallyTagged(b);
      };
      return `[${bound(value.lo)}, ${bound(value.hi)}]`;
    }
  }
  return JSON.stringify(range);
};

const formatArgUseModes = (modes: string[] | undefined): string => {
  if (!modes || modes.length === 0) {
    return "all borrow (default)";
  }
  return modes.map((mode, i) => `${i}:${mode}`).join(", ");
};

const formatMaybeStableId = (id: unknown): string => {
  if (id == undefined) {
    return "n/a";
  }
  if (typeof id === "string" || typeof id === "number" || typeof id === "boolean") {
    return String(id);
  }
  if (typeof id === "object" && id && typeof (id as any).value === "string") {
    return (id as any).value;
  }
  return JSON.stringify(id);
};

export const InstMetaPanel = ({
  inst,
  source,
}: {
  inst?: GraphInst;
  source?: string;
}) => {
  const [showEffects, setShowEffects] = useState(true);
  const [showOwnership, setShowOwnership] = useState(true);
  const [showFacts, setShowFacts] = useState(true);
  const [showTyped, setShowTyped] = useState(true);
  const [showRaw, setShowRaw] = useState(false);

  const meta: any = (inst as any)?.meta;

  const effectsSummary = useMemo(() => {
    const summary = meta?.effects?.summary;
    if (!summary || typeof summary !== "object") {
      return undefined;
    }
    const flags = (summary as any).flags;
    const throws = (summary as any).throws;
    if (typeof flags !== "string" || typeof throws !== "string") {
      return undefined;
    }
    return { flags, throws };
  }, [meta]);

  const span: { start: number; end: number } | undefined = useMemo(() => {
    const span = meta?.span;
    if (
      span &&
      typeof span === "object" &&
      typeof span.start === "number" &&
      typeof span.end === "number"
    ) {
      return { start: span.start, end: span.end };
    }
    return undefined;
  }, [meta]);

  const sourceSlice = useMemo(() => {
    if (!source || !span) {
      return undefined;
    }
    if (span.end <= span.start) {
      return "";
    }
    const start = utf8ByteOffsetToUtf16Offset(source, span.start);
    const end = utf8ByteOffsetToUtf16Offset(source, span.end);
    return source.slice(start, end);
  }, [source, span]);

  const rawJson = useMemo(
    () => (meta ? JSON.stringify(meta, null, 2) : ""),
    [meta],
  );

  return (
    <div className="InstMetaPanel">
      <header>
        <strong>Instruction metadata</strong>
        {inst ? (
          <span className="inst-summary">
            {" "}
            • {inst.t}
            {inst.tgts.length > 0 && ` → %${inst.tgts.join(", %")}`}
          </span>
        ) : (
          <span className="inst-summary"> • Hover an instruction</span>
        )}
      </header>

      <div className="toggles">
        <label className="toggle">
          <input
            type="checkbox"
            checked={showEffects}
            onChange={(e) => setShowEffects(e.target.checked)}
          />
          Effects
        </label>
        <label className="toggle">
          <input
            type="checkbox"
            checked={showOwnership}
            onChange={(e) => setShowOwnership(e.target.checked)}
          />
          Ownership/escape
        </label>
        <label className="toggle">
          <input
            type="checkbox"
            checked={showFacts}
            onChange={(e) => setShowFacts(e.target.checked)}
          />
          Range/nullability/encoding
        </label>
        <label className="toggle">
          <input
            type="checkbox"
            checked={showTyped}
            onChange={(e) => setShowTyped(e.target.checked)}
          />
          Typed IDs
        </label>
        <label className="toggle">
          <input
            type="checkbox"
            checked={showRaw}
            onChange={(e) => setShowRaw(e.target.checked)}
          />
          Raw
        </label>
      </div>

      {!inst ? (
        <p className="empty">Hover an instruction in the graph to view analysis metadata.</p>
      ) : !meta ? (
        <p className="empty">No analysis metadata available for this instruction.</p>
      ) : (
        <div className="sections">
          {showEffects && (
            <section>
              <h2>Effects</h2>
              <ul>
                {effectsSummary && (
                  <li>
                    summary:{" "}
                    <code>
                      flags={effectsSummary.flags}, throws={effectsSummary.throws}
                    </code>
                  </li>
                )}
                <li>
                  unknown:{" "}
                  <code>
                    {meta.effects ? (meta.effects.unknown ? "true" : "false") : "n/a"}
                  </code>
                </li>
                <li>
                  reads:{" "}
                  <code>
                    {!meta.effects || !Array.isArray(meta.effects.reads) || meta.effects.reads.length === 0
                      ? "[]"
                      : meta.effects.reads.map(formatEffectLocation).join(", ")}
                  </code>
                </li>
                <li>
                  writes:{" "}
                  <code>
                    {!meta.effects || !Array.isArray(meta.effects.writes) || meta.effects.writes.length === 0
                      ? "[]"
                      : meta.effects.writes.map(formatEffectLocation).join(", ")}
                  </code>
                </li>
              </ul>
            </section>
          )}

          {showOwnership && (
            <section>
              <h2>Ownership / escape</h2>
              <ul>
                <li>
                  ownership: <code>{meta.ownership ?? "n/a"}</code>
                </li>
                <li>
                  arg use modes: <code>{formatArgUseModes(meta.argUseModes)}</code>
                </li>
                {meta.preserveVarAssign && (
                  <li>
                    preserveVarAssign: <code>true</code>
                  </li>
                )}
                {meta.stackAllocCandidate && (
                  <li>
                    stackAllocCandidate: <code>true</code>
                  </li>
                )}
                <li>
                  in-place hint:{" "}
                  <code>
                    {meta.inPlaceHint ? formatExternallyTagged(meta.inPlaceHint) : "n/a"}
                  </code>
                </li>
                <li>
                  result escape:{" "}
                  <code>{formatExternallyTagged(meta.resultEscape)}</code>
                </li>
                <li>
                  purity: <code>{meta.purity ?? "n/a"}</code>
                </li>
                <li>
                  callee purity: <code>{meta.calleePurity ?? "n/a"}</code>
                </li>
              </ul>
            </section>
          )}

          {showFacts && (
            <section>
              <h2>Value facts</h2>
              <ul>
                {span && (
                  <li>
                    span:{" "}
                    <code>
                      {span.start}..{span.end}
                    </code>
                  </li>
                )}
                <li>
                  range: <code>{formatRange(meta.range)}</code>
                </li>
                <li>
                  nullability:{" "}
                  <code>
                    {meta.nullability
                      ? `null=${meta.nullability.mayBeNull}, undef=${meta.nullability.mayBeUndefined}, other=${meta.nullability.mayBeOther}, bottom=${meta.nullability.isBottom}`
                      : "n/a"}
                  </code>
                </li>
                <li>
                  encoding: <code>{meta.encoding ?? "n/a"}</code>
                </li>
                {meta.awaitKnownResolved && (
                  <li>
                    awaitKnownResolved: <code>true</code>
                  </li>
                )}
                {meta.awaitBehavior != undefined && (
                  <li>
                    awaitBehavior: <code>{formatExternallyTagged(meta.awaitBehavior)}</code>
                  </li>
                )}
                {meta.parallel != undefined && (
                  <li>
                    parallel: <code>{formatExternallyTagged(meta.parallel)}</code>
                  </li>
                )}
                {meta.nullabilityNarrowing != undefined && (
                  <li>
                    nullabilityNarrowing:{" "}
                    <code>{formatExternallyTagged(meta.nullabilityNarrowing)}</code>
                  </li>
                )}
                {meta.value != undefined && (
                  <li>
                    value: <code>{formatExternallyTagged(meta.value)}</code>
                  </li>
                )}
                <li>
                  excludesNullish:{" "}
                  <code>
                    {typeof meta.excludesNullish === "boolean"
                      ? meta.excludesNullish
                        ? "true"
                        : "false"
                      : "n/a"}
                  </code>
                </li>
              </ul>
              {sourceSlice != undefined && (
                <>
                  <h3>Source</h3>
                  <pre className="source">{sourceSlice}</pre>
                </>
              )}
            </section>
          )}

          {showTyped && (
            <section>
              <h2>Typed layouts</h2>
              <ul>
                <li>
                  typeId: <code>{formatMaybeStableId(meta.typeId)}</code>
                </li>
                <li>
                  nativeLayout: <code>{formatMaybeStableId(meta.nativeLayout)}</code>
                </li>
                <li>
                  typeSummary: <code>{meta.typeSummary ?? "n/a"}</code>
                </li>
                <li>
                  hirExpr: <code>{meta.hirExpr ?? "n/a"}</code>
                </li>
              </ul>
            </section>
          )}

          {showRaw && (
            <section>
              <h2>Raw</h2>
              <pre className="raw">{rawJson}</pre>
            </section>
          )}
        </div>
      )}
    </div>
  );
};
