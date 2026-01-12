declare const React: any;

declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute {
    children: {};
  }
  interface IntrinsicElements {
    div: { children?: [number, string] };
    p: { children?: [number, ...string[]] };
    span: {
      children?: [(ev: { x: number }) => void, (ev: { y: string }) => void];
    };
  }
}
