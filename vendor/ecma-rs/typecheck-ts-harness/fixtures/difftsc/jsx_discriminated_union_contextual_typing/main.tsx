// @jsx: react
// @strict: true

declare var React: any;

type Props =
  | { kind: "a"; onClick: (ev: { x: number }) => void }
  | { kind: "b"; onClick: (ev: { y: string }) => void };

function Foo(props: Props): JSX.Element {
  return null as any;
}

<Foo kind="a" onClick={(ev) => { const n: number = ev.x; }} />;
<Foo kind="b" onClick={(ev) => { const s: string = ev.y; }} />;
