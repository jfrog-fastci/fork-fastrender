#[path = "../../src/js/quickjs_url.rs"]
mod quickjs_url;

use rquickjs::{Context, Runtime};

#[test]
fn quickjs_url_and_urlsearchparams_bindings() {
  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();

  ctx
    .with(|ctx| {
      let globals = ctx.globals();
      quickjs_url::install_url_bindings(ctx.clone(), &globals).unwrap();

      let href: String = ctx
        .eval("new URL('foo', 'https://example.com/base').href")
        .unwrap();
      assert_eq!(href, "https://example.com/foo");

      let ctor_invalid_name: String = ctx
        .eval(
          r#"
          (() => {
            try {
              new URL("not a url");
              return "no-throw";
            } catch (e) {
              return e.name;
            }
          })()
        "#,
      )
      .unwrap();
      assert_eq!(ctor_invalid_name, "TypeError");

      let setter_invalid_name: String = ctx
        .eval(
          r#"
          (() => {
            const url = new URL("https://example.com/");
            try {
              url.href = "not a url";
              return "no-throw";
            } catch (e) {
              return e.name;
            }
          })()
        "#,
        )
        .unwrap();
      assert_eq!(setter_invalid_name, "TypeError");

      let stringifier: String = ctx
        .eval("`${new URL('https://example.com/a?b=c#d')}`")
        .unwrap();
      assert_eq!(stringifier, "https://example.com/a?b=c#d");

      let same_object: bool = ctx
        .eval(
          r#"
          (() => {
            const url = new URL("https://example.com/?a=1");
            return url.searchParams === url.searchParams;
          })()
        "#,
        )
        .unwrap();
      assert!(same_object);

      let mutated_href: String = ctx
        .eval(
          r#"
          (() => {
            const url = new URL("https://example.com/");
            url.searchParams.append("a", "b");
            return url.href;
          })()
        "#,
        )
        .unwrap();
      assert_eq!(mutated_href, "https://example.com/?a=b");

      let ctor_string: String = ctx
        .eval("new URLSearchParams('a=b&c=d').toString()")
        .unwrap();
      assert_eq!(ctor_string, "a=b&c=d");

      let ctor_sequence: String = ctx
        .eval("new URLSearchParams([['a','b'],['c','d']]).toString()")
        .unwrap();
      assert_eq!(ctor_sequence, "a=b&c=d");

      let ctor_record: String = ctx
        .eval("new URLSearchParams({a:'b',c:'d'}).toString()")
        .unwrap();
      assert_eq!(ctor_record, "a=b&c=d");

      let get_null: bool = ctx.eval("new URLSearchParams('').get('missing') === null").unwrap();
      assert!(get_null);

      let size: i32 = ctx.eval("new URLSearchParams('a=1&a=2&b=3').size").unwrap();
      assert_eq!(size, 3);

      let iter_joined: String = ctx
        .eval(
          r#"
          (() => {
            const params = new URLSearchParams("b=2&a=1&a=0");
            const out = [];
            for (const [k, v] of params) out.push(k + "=" + v);
            return out.join("&");
          })()
        "#,
        )
        .unwrap();
      assert_eq!(iter_joined, "b=2&a=1&a=0");

      let sorted: String = ctx
        .eval(
          r#"
          (() => {
            const params = new URLSearchParams("b=2&a=1&a=0");
            params.sort();
            return params.toString();
          })()
        "#,
        )
        .unwrap();
      assert_eq!(sorted, "a=1&a=0&b=2");

      let live_sync: String = ctx
        .eval(
          r#"
          (() => {
            const url = new URL("https://example.com/?a=1");
            const params = url.searchParams;
            url.search = "?b=2";
            return params.toString();
          })()
        "#,
        )
        .unwrap();
      assert_eq!(live_sync, "b=2");

      Ok::<(), rquickjs::Error>(())
    })
    .unwrap();
}
