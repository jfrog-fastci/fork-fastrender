// @lib: es2015
const Proxy = {
  revocable: (_target: object, _handler: object) => ({ proxy: {} }),
};

Proxy["revocable"]({}, {});

