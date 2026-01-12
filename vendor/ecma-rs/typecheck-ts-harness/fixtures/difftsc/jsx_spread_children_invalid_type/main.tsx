// @jsx: react
declare namespace JSX {
  interface Element {}
  interface IntrinsicElements {
    [s: string]: any;
  }
}

declare var React: any;

function Todo(props: { id: number }) {
  return <div>{props.id}</div>;
}

function TodoList() {
  return (
    <div>
      {...<Todo id={1} />}
      {...(<Todo id={1} /> as any)}
    </div>
  );
}
