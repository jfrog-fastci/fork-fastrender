import Dagre from "@dagrejs/dagre";
import {
  Edge,
  Handle,
  Node,
  Panel,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useEdgesState,
  useNodesInitialized,
  useNodesState,
  useReactFlow,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { Fragment, useEffect, useMemo, useState } from "react";
import {
  NormalizedBlock,
  NormalizedStep,
  StableArg,
  StableConst,
  StableEffectLocation,
  StableEscapeState,
  StableInst,
  StablePurity,
  argToLabel,
  blockMatchesQuery,
  formatId,
} from "./schema";

export type InstMetaOverlays = {
  effects: boolean;
  purity: boolean;
  escape: boolean;
  ownership: boolean;
  typeId: boolean;
  nativeLayout: boolean;
  parallelizable: boolean;
};

export type BBlockNode = Node<
  {
    label: number;
    insts: Array<StableInst>;
  },
  "bblock"
>;

const ConstElement = ({ value }: { value: StableConst }) => {
  switch (value.kind) {
    case "null":
      return <span className="const null">null</span>;
    case "undefined":
      return <span className="const undefined">undefined</span>;
    case "big_int":
      return <span className="const bigint">{BigInt(value.value).toString()}n</span>;
    case "bool":
      return <span className="const bool">{value.value.toString()}</span>;
    case "num":
      return <span className="const num">{value.value}</span>;
    case "str":
      return <span className="const str">{JSON.stringify(value.value)}</span>;
  }
};

const VarElement = ({ id }: { id: number }) => <span className="var">%{id}</span>;

const ArgElement = ({ arg }: { arg: StableArg }) => {
  switch (arg.kind) {
    case "builtin":
      return <span className="builtin">{arg.value}</span>;
    case "const":
      return <ConstElement value={arg.value} />;
    case "fn":
      return <span className="fn">Fn{arg.value}</span>;
    case "var":
      return <VarElement id={arg.value} />;
  }
};

const effectLocationLabel = (
  loc: StableEffectLocation,
  symbolNames?: Map<string, string>,
): string => {
  switch (loc.kind) {
    case "heap":
      return "heap";
    case "foreign": {
      const key = formatId(loc.id);
      const name = symbolNames?.get(key);
      return name ? `foreign ${key} (${name})` : `foreign ${key}`;
    }
    case "unknown":
      return `unknown ${loc.name}`;
  }
};

const escapeLabel = (state: StableEscapeState): string => {
  switch (state.kind) {
    case "no_escape":
      return "no_escape";
    case "arg_escape":
      return `arg_escape(${state.value})`;
    case "return_escape":
      return "return_escape";
    case "global_escape":
      return "global_escape";
    case "unknown":
      return "unknown";
  }
};

const purityLabel = (purity: StablePurity): string => {
  switch (purity) {
    case "pure":
      return "pure";
    case "read_only":
      return "read_only";
    case "allocating":
      return "allocating";
    case "impure":
      return "impure";
  }
};

const purityClass = (purity: StablePurity): string => {
  switch (purity) {
    case "pure":
      return "pure";
    case "read_only":
      return "readonly";
    case "allocating":
      return "allocating";
    case "impure":
      return "impure";
  }
};

const InstMetaBadges = ({
  inst,
  overlays,
  symbolNames,
}: {
  inst: StableInst;
  overlays: InstMetaOverlays;
  symbolNames?: Map<string, string>;
}) => {
  const meta = inst.meta;

  const badges: Array<JSX.Element> = [];

  if (overlays.purity) {
    const purity: StablePurity = meta?.purity ?? "pure";
    badges.push(
      <span key="purity" className={`inst-meta-badge purity ${purityClass(purity)}`}>
        {purityLabel(purity)}
      </span>,
    );

    if (inst.t === "Call") {
      const calleePurity: StablePurity = meta?.calleePurity ?? "impure";
      badges.push(
        <span
          key="callee-purity"
          className={`inst-meta-badge callee-purity ${purityClass(calleePurity)}`}
        >
          callee:{purityLabel(calleePurity)}
        </span>,
      );
    }
  }

  if (overlays.effects && meta?.effects) {
    const reads = meta.effects.reads ?? [];
    const writes = meta.effects.writes ?? [];
    const summary = meta.effects.summary;
    const details = [
      `flags: ${summary.flags}`,
      `throws: ${summary.throws}`,
      reads.length ? `reads: ${reads.map((r) => effectLocationLabel(r, symbolNames)).join(", ")}` : "",
      writes.length
        ? `writes: ${writes.map((w) => effectLocationLabel(w, symbolNames)).join(", ")}`
        : "",
      meta.effects.unknown ? "unknown: true" : "",
    ]
      .filter(Boolean)
      .join("\n");
    badges.push(
      <span key="effects" className="inst-meta-badge effects" title={details}>
        fx
      </span>,
    );
  }

  if (overlays.escape && meta?.resultEscape) {
    badges.push(
      <span key="escape" className="inst-meta-badge escape">
        {escapeLabel(meta.resultEscape)}
      </span>,
    );
  }

  if (overlays.ownership && meta?.ownership) {
    badges.push(
      <span key="ownership" className="inst-meta-badge ownership">
        {meta.ownership}
      </span>,
    );
  }

  if (overlays.typeId && meta?.typeId) {
    badges.push(
      <span key="typeId" className="inst-meta-badge type-id">
        type:{formatId(meta.typeId)}
      </span>,
    );
  }

  if (overlays.nativeLayout && meta?.nativeLayout) {
    badges.push(
      <span key="nativeLayout" className="inst-meta-badge native-layout">
        layout:{formatId(meta.nativeLayout)}
      </span>,
    );
  }

  if (overlays.parallelizable) {
    const purity: StablePurity = meta?.purity ?? "pure";
    const throws = meta?.effects?.summary.throws ?? "never";
    const unknown = meta?.effects?.unknown ?? false;
    const parallelizable = purity === "pure" && throws === "never" && !unknown;
    if (parallelizable) {
      badges.push(
        <span key="parallel" className="inst-meta-badge parallelizable">
          parallel
        </span>,
      );
    }
  }

  if (badges.length === 0) {
    return null;
  }

  return <div className="inst-meta-badges">{badges}</div>;
};

const InstElement = ({
  inst,
  symbolNames,
  overlays,
}: {
  inst: StableInst;
  symbolNames?: Map<string, string>;
  overlays: InstMetaOverlays;
}) => {
  const foreignLabel = () => {
    const key = inst.foreign ? formatId(inst.foreign) : "foreign";
    const name = key && symbolNames?.get(key);
    return name ? `foreign ${key} (${name})` : `foreign ${key}`;
  };

  switch (inst.t) {
    case "Bin":
      return (
        <>
          <div>
            <VarElement id={inst.tgts[0]} />
            <span className="eq"> =</span>
          </div>
          <div>
            <ArgElement arg={inst.args[0]} />
            <span> {inst.binOp} </span>
            <ArgElement arg={inst.args[1]} />
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "Call":
      return (
        <>
          <div>
            {inst.tgts[0] == undefined ? <span /> : <VarElement id={inst.tgts[0]} />}
            <span className="eq"> =</span>
          </div>
          <div>
            <ArgElement arg={inst.args[0]} />
            <span>(</span>
            <span>this=</span>
            <ArgElement arg={inst.args[1]} />
            {inst.args.slice(2).map((arg, i) => (
              <Fragment key={i}>
                <span>, </span>
                {inst.spreads.includes(i + 2) && <span>&hellip;</span>}
                {arg && <ArgElement arg={arg} />}
              </Fragment>
            ))}
            <span>)</span>
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "CondGoto":
      return (
        <>
          <div>
            <span>condgoto</span>
          </div>
          <div>
            <span className="label">:{inst.labels[0]}</span>
            <span> if </span>
            <ArgElement arg={inst.args[0]} />
            <span> else </span>
            <span className="label">:{inst.labels[1]}</span>
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "ForeignLoad":
      return (
        <>
          <div>
            <VarElement id={inst.tgts[0]} />
            <span className="eq"> =</span>
          </div>
          <div>
            <span className="foreign">{foreignLabel()}</span>
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "ForeignStore":
      return (
        <>
          <div>
            <span className="foreign">{foreignLabel()}</span>
            <span className="eq"> =</span>
          </div>
          <div>
            <ArgElement arg={inst.args[0]} />
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "Phi":
      return (
        <>
          <div>
            <VarElement id={inst.tgts[0]} />
            <span className="eq"> =</span>
          </div>
          <div>
            <span>ϕ(</span>
            {inst.labels.map((label, i) => (
              <Fragment key={i}>
                <span>{i === 0 ? "" : ", "}</span>
                <span className="label">:{label}</span>
                <span> ⇒ </span>
                <ArgElement arg={inst.args[i]} />
              </Fragment>
              ))}
            <span>)</span>
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "PropAssign":
      return (
        <>
          <div>
            <ArgElement arg={inst.args[0]} />
            <span>[</span>
            <ArgElement arg={inst.args[1]} />
            <span>]</span>
            <span className="eq"> =</span>
          </div>
          <div>
            <ArgElement arg={inst.args[2]} />
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "Un":
      return (
        <>
          <div>
            <VarElement id={inst.tgts[0]} />
            <span className="eq"> =</span>
          </div>
          <div>
            <span>{inst.unOp} </span>
            <ArgElement arg={inst.args[0]} />
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "UnknownLoad":
      return (
        <>
          <div>
            <VarElement id={inst.tgts[0]} />
            <span className="eq"> =</span>
          </div>
          <div>
            <span className="unknown">unknown {inst.unknown}</span>
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "UnknownStore":
      return (
        <>
          <div>
            <span className="unknown">unknown {inst.unknown}</span>
            <span className="eq"> =</span>
          </div>
          <div>
            <ArgElement arg={inst.args[0]} />
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    case "VarAssign":
      return (
        <>
          <div>
            <VarElement id={inst.tgts[0]} />
            <span className="eq"> =</span>
          </div>
          <div>
            <ArgElement arg={inst.args[0]} />
          </div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
    default:
      return (
        <>
          <div>
            <span className="unknown">{inst.t}</span>
          </div>
          <div>{inst.args.map(argToLabel).join(", ")}</div>
          <div className="inst-meta">
            <InstMetaBadges inst={inst} overlays={overlays} symbolNames={symbolNames} />
          </div>
        </>
      );
  }
};

const BBlockElement = ({
  data: { label, insts },
  symbolNames,
  changed,
  overlays,
}: {
  data: BBlockNode["data"];
  symbolNames?: Map<string, string>;
  changed?: boolean;
  overlays: InstMetaOverlays;
}) => {
  return (
    <>
      <Handle type="target" position={Position.Left} />
      <div className={`bblock ${changed ? "changed" : ""}`}>
        <h1>:{label}</h1>
        <ol className="insts">
          {insts.map((s, i) => (
            <li key={i} className="inst">
              <InstElement inst={s} symbolNames={symbolNames} overlays={overlays} />
            </li>
          ))}
        </ol>
      </div>
      <Handle type="source" position={Position.Bottom} />
    </>
  );
};

export const getLayoutedElements = (
  nodes: Array<BBlockNode>,
  edges: Array<Edge>,
  options: { direction: string },
) => {
  const g = new Dagre.graphlib.Graph().setDefaultEdgeLabel(() => ({}));
  g.setGraph({ rankdir: options.direction });

  for (const edge of edges) {
    g.setEdge(edge.source, edge.target);
  }
  for (const node of nodes) {
    g.setNode(node.id, {
      ...node,
      width: node.measured?.width ?? 0,
      height: node.measured?.height ?? 0,
    });
  }

  Dagre.layout(g);

  return {
    nodes: nodes.map((node) => {
      const position = g.node(node.id);
      const x = position.x - (node.measured?.width ?? 0) / 2;
      const y = position.y - (node.measured?.height ?? 0) / 2;
      return { ...node, position: { x, y } };
    }),
    edges,
  };
};

export const Graph = ({
  stepNames,
  step,
  symbolNames,
  changed,
  filter,
  overlays,
}: {
  stepNames: Array<string>;
  step: NormalizedStep;
  symbolNames?: Map<string, string>;
  changed?: Set<number>;
  filter: string;
  overlays: InstMetaOverlays;
}) => {
  const query = filter.trim().toLowerCase();
  const filteredBlocks: NormalizedBlock[] = useMemo(
    () =>
      query
        ? step.blocks.filter((block) => blockMatchesQuery(block, query, symbolNames))
        : step.blocks,
    [step, query, symbolNames],
  );
  const visible = new Set(filteredBlocks.map((b) => b.label));
  const initNodes = useMemo(
    () =>
      filteredBlocks.map<BBlockNode>((block) => ({
        id: `${block.label}`,
        type: "bblock",
        data: {
          label: block.label,
          insts: block.insts,
        },
        position: { x: 0, y: 0 },
        className: changed?.has(block.label) ? "changed" : undefined,
      })),
    [step, changed, query],
  );
  const initEdges = useMemo(
    () =>
      [...step.children.entries()]
        .filter(([parent]) => visible.has(parent))
        .flatMap(([src, dests]) =>
          dests
            .filter((child) => visible.has(child))
            .map<Edge>((dest) => ({
              id: `${src}-${dest}`,
              source: `${src}`,
              target: `${dest}`,
              animated: true,
            })),
        ),
    [step, query],
  );

  const nodeTypes = useMemo(
    () => ({
      bblock: (props: any) => (
        <BBlockElement
          {...props}
          symbolNames={symbolNames}
          changed={changed?.has(props.data.label)}
          overlays={overlays}
        />
      ),
    }),
    [symbolNames, changed, overlays],
  );

  const GraphCanvas = () => {
    const { fitView } = useReactFlow();
    const [nodes, setNodes, onNodesChange] = useNodesState(initNodes);
    const [edges, setEdges, onEdgesChange] = useEdgesState(initEdges);
    const nodesSized = useNodesInitialized();
    const [layoutCalculated, setLayoutCalculated] = useState(false);
    useEffect(() => {
      setNodes(initNodes);
      setEdges(initEdges);
      setLayoutCalculated(false);
    }, [step, query]);

    useEffect(() => {
      if (!nodesSized || layoutCalculated) {
        return;
      }
      const layouted = getLayoutedElements(nodes, edges, { direction: "TB" });
      setNodes(layouted.nodes);
      setEdges(layouted.edges);
      setLayoutCalculated(true);
    }, [nodesSized]);

    useEffect(() => {
      if (nodesSized && layoutCalculated) {
        fitView();
      }
    }, [layoutCalculated]);

    return (
      <ReactFlow
        edges={edges}
        fitView
        nodes={nodes}
        nodesDraggable={false}
        nodeTypes={nodeTypes}
        onEdgesChange={onEdgesChange}
        onNodesChange={onNodesChange}
      >
        <Panel position="top-left">
          <ul className="step-names">
            {stepNames.map((name, i) => (
              <li key={i} className={name == step.name ? "current" : ""}>
                {name}
              </li>
            ))}
          </ul>
        </Panel>
      </ReactFlow>
    );
  };

  return (
    <div className="BBlocksExplorer">
      <ReactFlowProvider>
        <GraphCanvas />
      </ReactFlowProvider>
    </div>
  );
};
