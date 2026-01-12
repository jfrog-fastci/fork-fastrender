// @jsx: react
// @lib: es5
function Foo(props: { x: number }): JSX.Element { return null as any; }
const ok = <Foo {...{ x: 1 }} />;
const bad = <Foo {...{ x: 1 }} y={1} />;
