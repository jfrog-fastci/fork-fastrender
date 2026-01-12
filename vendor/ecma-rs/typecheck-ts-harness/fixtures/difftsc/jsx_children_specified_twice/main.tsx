// @jsx: react
declare const React: any;

declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute {
    children: {};
  }
  interface IntrinsicElements {
    div: { children?: string };
  }
}

const el = <div children="x">y</div>;
