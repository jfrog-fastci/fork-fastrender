// @lib: es2015, es2015.proxy
const target: object = {};
const handler: ProxyHandler<object> = {};
const p = new Proxy(target, handler);
void p;
