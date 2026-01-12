// @jsx: react-jsx
// @moduleResolution: node

declare function Title(props: { children: string }): JSX.Element;
declare function Wrong(props: { offspring: string }): JSX.Element;

const ok = <Title>Hello</Title>;
const bad = <Wrong>Byebye</Wrong>;
