// @jsx: react

declare const React: any;

declare namespace JSX {
  interface Element {}
  interface IntrinsicElements {
    div: {};
  }
  interface ElementChildrenAttribute {
    children: {};
  }
}

const el = <div data-foo="x" />;
