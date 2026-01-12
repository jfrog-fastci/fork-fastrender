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
  GraphInst,
  NormalizedBlock,
  NormalizedStep,
  StableArg,
  StableConst,
  argToLabel,
  blockMatchesQuery,
  formatForeign,
} from "./schema";

export type BBlockNode = Node<
  {
    label: number;
    insts: Array<GraphInst>;
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

const InstElement = ({
  inst,
  symbolNames,
}: {
  inst: GraphInst;
  symbolNames?: Map<string, string>;
}) => {
  const foreignLabel = () => {
    const foreign = (inst as any).foreign;
    const key = foreign != undefined ? formatForeign(foreign) : "foreign";
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
            <span> {(inst as any).binOp} </span>
            <ArgElement arg={inst.args[1]} />
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
            <span>{(inst as any).unOp} </span>
            <ArgElement arg={inst.args[0]} />
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
            <span className="unknown">unknown {(inst as any).unknown}</span>
          </div>
        </>
      );
    case "UnknownStore":
      return (
        <>
          <div>
            <span className="unknown">unknown {(inst as any).unknown}</span>
            <span className="eq"> =</span>
          </div>
          <div>
            <ArgElement arg={inst.args[0]} />
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
        </>
      );
    default:
      return (
        <>
          <div>
            <span className="unknown">{inst.t}</span>
          </div>
          <div>{inst.args.map(argToLabel).join(", ")}</div>
        </>
      );
  }
};

const escapeKind = (state: any): string | undefined => {
  if (state == undefined) {
    return undefined;
  }
  if (typeof state === "string") {
    return state;
  }
  if (typeof state === "object" && state) {
    if (typeof state.kind === "string") {
      return state.kind;
    }
    const entries = Object.entries(state);
    if (entries.length === 1 && typeof entries[0][0] === "string") {
      return entries[0][0];
    }
  }
  return undefined;
};

const BBlockElement = ({
  data: { label, insts },
  symbolNames,
  changed,
  selectedInst,
  onHoverInst,
  onSelectInst,
}: {
  data: BBlockNode["data"];
  symbolNames?: Map<string, string>;
  changed?: boolean;
  selectedInst?: GraphInst;
  onHoverInst?: (inst: GraphInst | undefined) => void;
  onSelectInst?: (inst: GraphInst | undefined) => void;
}) => {
  return (
    <>
      <Handle type="target" position={Position.Left} />
      <div className={`bblock ${changed ? "changed" : ""}`}>
        <h1>:{label}</h1>
        <ol className="insts">
          {insts.map((s, i) => (
            <li
              key={i}
              className={`inst ${s === selectedInst ? "selected" : ""}`}
              title={(s as any).meta ? JSON.stringify((s as any).meta, null, 2) : undefined}
              onMouseEnter={() => onHoverInst?.(s)}
              onMouseLeave={() => onHoverInst?.(undefined)}
              onClick={() => onSelectInst?.(s)}
            >
              <InstElement inst={s} symbolNames={symbolNames} />
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

const GraphCanvas = ({
  stepNames,
  currentStepName,
  initNodes,
  initEdges,
  nodeTypes,
}: {
  stepNames: Array<string>;
  currentStepName: string;
  initNodes: Array<BBlockNode>;
  initEdges: Array<Edge>;
  nodeTypes: Record<string, any>;
}) => {
  const { fitView } = useReactFlow();
  const [nodes, setNodes, onNodesChange] = useNodesState(initNodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(initEdges);
  const nodesSized = useNodesInitialized();
  const [layoutCalculated, setLayoutCalculated] = useState(false);
  useEffect(() => {
    setNodes(initNodes);
    setEdges(initEdges);
    setLayoutCalculated(false);
  }, [initNodes, initEdges, setNodes, setEdges]);

  useEffect(() => {
    if (!nodesSized || layoutCalculated) {
      return;
    }
    const layouted = getLayoutedElements(nodes, edges, { direction: "TB" });
    setNodes(layouted.nodes);
    setEdges(layouted.edges);
    setLayoutCalculated(true);
  }, [nodesSized, layoutCalculated, nodes, edges, setNodes, setEdges]);

  useEffect(() => {
    if (nodesSized && layoutCalculated) {
      fitView();
    }
  }, [nodesSized, layoutCalculated, fitView]);

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
            <li key={i} className={name == currentStepName ? "current" : ""}>
              {name}
            </li>
          ))}
        </ul>
      </Panel>
    </ReactFlow>
  );
};

export const Graph = ({
  stepNames,
  step,
  symbolNames,
  changed,
  filter,
  onlyUnknownEffects,
  onlyEscapingAllocs,
  selectedInst,
  onHoverInst,
  onSelectInst,
}: {
  stepNames: Array<string>;
  step: NormalizedStep;
  symbolNames?: Map<string, string>;
  changed?: Set<number>;
  filter: string;
  onlyUnknownEffects?: boolean;
  onlyEscapingAllocs?: boolean;
  selectedInst?: GraphInst;
  onHoverInst?: (inst: GraphInst | undefined) => void;
  onSelectInst?: (inst: GraphInst | undefined) => void;
}) => {
  const query = filter.trim().toLowerCase();

  const metaFilteredBlocks: NormalizedBlock[] = useMemo(() => {
    const unknown = !!onlyUnknownEffects;
    const escaping = !!onlyEscapingAllocs;
    if (!unknown && !escaping) {
      return step.blocks;
    }

    const isEscaping = (inst: GraphInst): boolean => {
      const kind = escapeKind((inst as any).meta?.resultEscape);
      if (!kind) {
        return false;
      }
      return kind !== "no_escape" && kind !== "NoEscape";
    };

    const shouldKeep = (inst: GraphInst): boolean => {
      const effectsUnknown = (inst as any).meta?.effects?.unknown === true;
      const escapes = isEscaping(inst);
      if (unknown && escaping) {
        return effectsUnknown || escapes;
      }
      if (unknown) {
        return effectsUnknown;
      }
      return escapes;
    };

    return step.blocks
      .map((block) => ({ ...block, insts: block.insts.filter(shouldKeep) }))
      .filter((block) => block.insts.length > 0);
  }, [step, onlyUnknownEffects, onlyEscapingAllocs]);

  const filteredBlocks: NormalizedBlock[] = useMemo(() => {
    if (!query) {
      return metaFilteredBlocks;
    }
    return metaFilteredBlocks.filter((block) => blockMatchesQuery(block, query, symbolNames));
  }, [metaFilteredBlocks, query, symbolNames]);

  useEffect(() => {
    onHoverInst?.(undefined);
  }, [step, query, onlyUnknownEffects, onlyEscapingAllocs]);

  useEffect(() => {
    onSelectInst?.(undefined);
  }, [step]);

  const visible = useMemo(() => new Set(filteredBlocks.map((b) => b.label)), [filteredBlocks]);
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
    [filteredBlocks, changed],
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
    [step, visible],
  );

  const nodeTypes = useMemo(
    () => ({
      bblock: (props: any) => (
        <BBlockElement
          {...props}
          symbolNames={symbolNames}
          changed={changed?.has(props.data.label)}
          selectedInst={selectedInst}
          onHoverInst={onHoverInst}
          onSelectInst={onSelectInst}
        />
      ),
    }),
    [symbolNames, changed, selectedInst, onHoverInst, onSelectInst],
  );

  return (
    <div className="BBlocksExplorer">
      <ReactFlowProvider>
        <GraphCanvas
          stepNames={stepNames}
          currentStepName={step.name}
          initNodes={initNodes}
          initEdges={initEdges}
          nodeTypes={nodeTypes}
        />
      </ReactFlowProvider>
    </div>
  );
};
