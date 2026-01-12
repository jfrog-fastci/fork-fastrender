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

type PropsWithChildren =
  | { kind: "a"; children: (ev: { x: number }) => void }
  | { kind: "b"; children: (ev: { y: string }) => void };

function FooWithChildren(props: PropsWithChildren): JSX.Element {
  return null as any;
}

<FooWithChildren kind="a">{(ev) => { const n: number = ev.x; }}</FooWithChildren>;
<FooWithChildren kind="b">{(ev) => { const s: string = ev.y; }}</FooWithChildren>;
