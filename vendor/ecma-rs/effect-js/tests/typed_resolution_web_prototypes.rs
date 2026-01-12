#![cfg(feature = "typed")]

use diagnostics::TextRange;
use effect_js::typed::TypedProgram;
use effect_js::{load_default_api_database, resolve_call, resolve_member, ApiId};
use hir_js::{BodyId, ExprId, ExprKind};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es2015_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  })
}

fn range_of(source: &str, needle: &str) -> TextRange {
  let start = source.find(needle).expect("needle not found") as u32;
  TextRange::new(start, start + needle.len() as u32)
}

fn find_call_expr(lowered: &hir_js::LowerResult, span: TextRange) -> (BodyId, ExprId) {
  for body_id in lowered.body_index.keys().copied() {
    let Some(body) = lowered.body(body_id) else {
      continue;
    };
    for (idx, expr) in body.exprs.iter().enumerate() {
      if expr.span != span {
        continue;
      }
      match &expr.kind {
        ExprKind::Call(_) => return (body_id, ExprId(idx as u32)),
        _ => continue,
      }
    }
  }
  panic!("call expression not found for span {span:?}")
}

fn find_member_expr(lowered: &hir_js::LowerResult, span: TextRange) -> (BodyId, ExprId) {
  for body_id in lowered.body_index.keys().copied() {
    let Some(body) = lowered.body(body_id) else {
      continue;
    };
    for (idx, expr) in body.exprs.iter().enumerate() {
      if expr.span != span {
        continue;
      }
      match &expr.kind {
        ExprKind::Member(_) => return (body_id, ExprId(idx as u32)),
        _ => continue,
      }
    }
  }
  panic!("member expression not found for span {span:?}")
}

#[test]
fn typed_resolves_common_web_prototype_calls_and_getters() {
  let source = r#"
export {};

interface URL {
  readonly pathname: string;
  readonly searchParams: URLSearchParams;
  toString(): string;
}

declare const URL: {
  prototype: URL;
  new (url: string): URL;
};

interface URLSearchParams {
  readonly size: number;
  get(name: string): string | null;
}

declare const URLSearchParams: {
  prototype: URLSearchParams;
  new (init: string): URLSearchParams;
};

declare class Response {
  readonly ok: boolean;
  readonly status: number;
  json(): Promise<any>;
}

declare const resp: Response;

declare class EventTarget {
  addEventListener(type: string, listener: () => void): void;
}

declare class AbortSignal extends EventTarget {}

declare class AbortController {
  readonly signal: AbortSignal;
}

declare const controller: AbortController;

declare class Blob {
  readonly size: number;
  text(): Promise<string>;
}

declare class File extends Blob {
  readonly name: string;
}

declare const file: File;

declare function fetch(url: string): Promise<Response>;

interface Promise<T> {
  then<U>(onfulfilled: (value: T) => U): Promise<U>;
}

declare class Storage {
  readonly length: number;
  getItem(key: string): string | null;
}

declare const localStorage: Storage;

declare class MessagePort extends EventTarget {
  postMessage(message: string): void;
}

declare class MessageChannel {
  readonly port1: MessagePort;
  readonly port2: MessagePort;
}

declare class BroadcastChannel extends EventTarget {
  postMessage(message: string): void;
  close(): void;
}

declare const bc: BroadcastChannel;

declare class WebSocket extends EventTarget {
  send(data: string): void;
  close(): void;
}

declare const ws: WebSocket;

new URL("https://x").pathname;
new URL("https://x").searchParams;
new URL("https://x").toString();
new URLSearchParams("a=1").get("a");
new URLSearchParams("a=1").size;
fetch("x").then(r => r.status);
fetch("x").then(r => r.ok);
resp.json();
controller.signal.addEventListener("abort", () => {});
file.text();
file.size;
file.name;
const date = new Date();
date.toISOString();
const re = new RegExp("a");
re.test("a");
localStorage.getItem("k");
localStorage.length;
const channel = new MessageChannel();
channel.port1;
channel.port2;
channel.port1.postMessage("hi");
channel.port1.addEventListener("message", () => {});
bc.postMessage("hi");
bc.addEventListener("message", () => {});
bc.close();
ws.send("hi");
ws.addEventListener("open", () => {});
ws.close();
"#;

  let file = FileKey::new("index.ts");
  let mut host = es2015_host();
  host.insert(file.clone(), source);

  let program = Arc::new(Program::new(host, vec![file.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:#?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let lowered = program.hir_lowered(file_id).expect("HIR lowered");
  let lower = lowered.as_ref();
  let types = TypedProgram::from_program(Arc::clone(&program), file_id);
  let kb = load_default_api_database();

  let pathname_span = range_of(source, r#"new URL("https://x").pathname"#);
  let (pathname_body, pathname_expr) = find_member_expr(lower, pathname_span);
  let resolved_pathname =
    resolve_member(&kb, lower, pathname_body, pathname_expr, &types).expect("resolve URL.pathname");
  assert_eq!(resolved_pathname.api, "URL.prototype.pathname");
  assert_eq!(
    resolved_pathname.api_id,
    ApiId::from_name("URL.prototype.pathname")
  );

  let search_params_span = range_of(source, r#"new URL("https://x").searchParams"#);
  let (search_params_body, search_params_expr) = find_member_expr(lower, search_params_span);
  let resolved_search_params =
    resolve_member(&kb, lower, search_params_body, search_params_expr, &types)
      .expect("resolve URL.searchParams");
  assert_eq!(resolved_search_params.api, "URL.prototype.searchParams");
  assert_eq!(
    resolved_search_params.api_id,
    ApiId::from_name("URL.prototype.searchParams")
  );

  let url_to_string_span = range_of(source, r#"new URL("https://x").toString()"#);
  let (to_string_body, to_string_expr) = find_call_expr(lower, url_to_string_span);
  let to_string_body_ref = lower.body(to_string_body).expect("body");
  let resolved_to_string = resolve_call(
    lower,
    to_string_body,
    to_string_body_ref,
    to_string_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve URL.toString()");
  assert_eq!(resolved_to_string.api, "URL.prototype.toString");
  assert_eq!(
    resolved_to_string.api_id,
    ApiId::from_name("URL.prototype.toString")
  );

  let params_get_span = range_of(source, r#"new URLSearchParams("a=1").get("a")"#);
  let (params_body, params_expr) = find_call_expr(lower, params_get_span);
  let params_body_ref = lower.body(params_body).expect("body");
  let resolved_params_get = resolve_call(
    lower,
    params_body,
    params_body_ref,
    params_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve URLSearchParams.get()");
  assert_eq!(resolved_params_get.api, "URLSearchParams.prototype.get");
  assert_eq!(
    resolved_params_get.api_id,
    ApiId::from_name("URLSearchParams.prototype.get")
  );

  let params_size_span = range_of(source, r#"new URLSearchParams("a=1").size"#);
  let (params_size_body, params_size_expr) = find_member_expr(lower, params_size_span);
  let resolved_params_size = resolve_member(&kb, lower, params_size_body, params_size_expr, &types)
    .expect("resolve URLSearchParams.size");
  assert_eq!(resolved_params_size.api, "URLSearchParams.prototype.size");
  assert_eq!(
    resolved_params_size.api_id,
    ApiId::from_name("URLSearchParams.prototype.size")
  );

  let status_span = range_of(source, "r.status");
  let (status_body, status_expr) = find_member_expr(lower, status_span);
  let resolved_status =
    resolve_member(&kb, lower, status_body, status_expr, &types).expect("resolve Response.status");
  assert_eq!(resolved_status.api, "Response.prototype.status");
  assert_eq!(
    resolved_status.api_id,
    ApiId::from_name("Response.prototype.status")
  );

  let ok_span = range_of(source, "r.ok");
  let (ok_body, ok_expr) = find_member_expr(lower, ok_span);
  let resolved_ok =
    resolve_member(&kb, lower, ok_body, ok_expr, &types).expect("resolve Response.ok");
  assert_eq!(resolved_ok.api, "Response.prototype.ok");
  assert_eq!(
    resolved_ok.api_id,
    ApiId::from_name("Response.prototype.ok")
  );

  let json_span = range_of(source, "resp.json()");
  let (json_body, json_expr) = find_call_expr(lower, json_span);
  let json_body_ref = lower.body(json_body).expect("body");
  let resolved_json = resolve_call(
    lower,
    json_body,
    json_body_ref,
    json_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve Response.json()");
  assert_eq!(resolved_json.api, "Response.prototype.json");
  assert_eq!(
    resolved_json.api_id,
    ApiId::from_name("Response.prototype.json")
  );

  let add_listener_span = range_of(
    source,
    r#"controller.signal.addEventListener("abort", () => {})"#,
  );
  let (add_listener_body, add_listener_expr) = find_call_expr(lower, add_listener_span);
  let add_listener_body_ref = lower.body(add_listener_body).expect("body");
  let resolved_add_listener = resolve_call(
    lower,
    add_listener_body,
    add_listener_body_ref,
    add_listener_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve AbortSignal.addEventListener()");
  assert_eq!(
    resolved_add_listener.api,
    "EventTarget.prototype.addEventListener"
  );
  assert_eq!(
    resolved_add_listener.api_id,
    ApiId::from_name("EventTarget.prototype.addEventListener")
  );

  let file_text_span = range_of(source, "file.text()");
  let (file_text_body, file_text_expr) = find_call_expr(lower, file_text_span);
  let file_text_body_ref = lower.body(file_text_body).expect("body");
  let resolved_file_text = resolve_call(
    lower,
    file_text_body,
    file_text_body_ref,
    file_text_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve File.text()");
  assert_eq!(resolved_file_text.api, "Blob.prototype.text");
  assert_eq!(
    resolved_file_text.api_id,
    ApiId::from_name("Blob.prototype.text")
  );

  let file_size_span = range_of(source, "file.size");
  let (file_size_body, file_size_expr) = find_member_expr(lower, file_size_span);
  let resolved_file_size =
    resolve_member(&kb, lower, file_size_body, file_size_expr, &types).expect("resolve File.size");
  assert_eq!(resolved_file_size.api, "Blob.prototype.size");
  assert_eq!(
    resolved_file_size.api_id,
    ApiId::from_name("Blob.prototype.size")
  );

  let file_name_span = range_of(source, "file.name");
  let (file_name_body, file_name_expr) = find_member_expr(lower, file_name_span);
  let resolved_file_name =
    resolve_member(&kb, lower, file_name_body, file_name_expr, &types).expect("resolve File.name");
  assert_eq!(resolved_file_name.api, "File.prototype.name");
  assert_eq!(
    resolved_file_name.api_id,
    ApiId::from_name("File.prototype.name")
  );

  let date_to_iso_span = range_of(source, "date.toISOString()");
  let (date_to_iso_body, date_to_iso_expr) = find_call_expr(lower, date_to_iso_span);
  let date_to_iso_body_ref = lower.body(date_to_iso_body).expect("body");
  let resolved_date_to_iso = resolve_call(
    lower,
    date_to_iso_body,
    date_to_iso_body_ref,
    date_to_iso_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve Date.toISOString()");
  assert_eq!(resolved_date_to_iso.api, "Date.prototype.toISOString");
  assert_eq!(
    resolved_date_to_iso.api_id,
    ApiId::from_name("Date.prototype.toISOString")
  );

  let regexp_test_span = range_of(source, r#"re.test("a")"#);
  let (regexp_test_body, regexp_test_expr) = find_call_expr(lower, regexp_test_span);
  let regexp_test_body_ref = lower.body(regexp_test_body).expect("body");
  let resolved_regexp_test = resolve_call(
    lower,
    regexp_test_body,
    regexp_test_body_ref,
    regexp_test_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve RegExp.test()");
  assert_eq!(resolved_regexp_test.api, "RegExp.prototype.test");
  assert_eq!(
    resolved_regexp_test.api_id,
    ApiId::from_name("RegExp.prototype.test")
  );

  let storage_get_span = range_of(source, r#"localStorage.getItem("k")"#);
  let (storage_get_body, storage_get_expr) = find_call_expr(lower, storage_get_span);
  let storage_get_body_ref = lower.body(storage_get_body).expect("body");
  let resolved_storage_get_item = resolve_call(
    lower,
    storage_get_body,
    storage_get_body_ref,
    storage_get_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve Storage.getItem()");
  assert_eq!(resolved_storage_get_item.api, "Storage.prototype.getItem");
  assert_eq!(
    resolved_storage_get_item.api_id,
    ApiId::from_name("Storage.prototype.getItem")
  );

  let storage_length_span = range_of(source, "localStorage.length");
  let (storage_length_body, storage_length_expr) = find_member_expr(lower, storage_length_span);
  let resolved_storage_length = resolve_member(
    &kb,
    lower,
    storage_length_body,
    storage_length_expr,
    &types,
  )
  .expect("resolve Storage.length");
  assert_eq!(resolved_storage_length.api, "Storage.prototype.length");
  assert_eq!(
    resolved_storage_length.api_id,
    ApiId::from_name("Storage.prototype.length")
  );

  let port1_span = range_of(source, "channel.port1");
  let (port1_body, port1_expr) = find_member_expr(lower, port1_span);
  let resolved_port1 = resolve_member(&kb, lower, port1_body, port1_expr, &types)
    .expect("resolve MessageChannel.port1");
  assert_eq!(resolved_port1.api, "MessageChannel.prototype.port1");
  assert_eq!(
    resolved_port1.api_id,
    ApiId::from_name("MessageChannel.prototype.port1")
  );

  let port2_span = range_of(source, "channel.port2");
  let (port2_body, port2_expr) = find_member_expr(lower, port2_span);
  let resolved_port2 = resolve_member(&kb, lower, port2_body, port2_expr, &types)
    .expect("resolve MessageChannel.port2");
  assert_eq!(resolved_port2.api, "MessageChannel.prototype.port2");
  assert_eq!(
    resolved_port2.api_id,
    ApiId::from_name("MessageChannel.prototype.port2")
  );

  let post_message_span = range_of(source, r#"channel.port1.postMessage("hi")"#);
  let (post_message_body, post_message_expr) = find_call_expr(lower, post_message_span);
  let post_message_body_ref = lower.body(post_message_body).expect("body");
  let resolved_post_message = resolve_call(
    lower,
    post_message_body,
    post_message_body_ref,
    post_message_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve MessagePort.postMessage()");
  assert_eq!(resolved_post_message.api, "MessagePort.prototype.postMessage");
  assert_eq!(
    resolved_post_message.api_id,
    ApiId::from_name("MessagePort.prototype.postMessage")
  );

  let port_add_listener_span = range_of(
    source,
    r#"channel.port1.addEventListener("message", () => {})"#,
  );
  let (port_add_listener_body, port_add_listener_expr) =
    find_call_expr(lower, port_add_listener_span);
  let port_add_listener_body_ref = lower.body(port_add_listener_body).expect("body");
  let resolved_port_add_listener = resolve_call(
    lower,
    port_add_listener_body,
    port_add_listener_body_ref,
    port_add_listener_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve MessagePort.addEventListener()");
  assert_eq!(
    resolved_port_add_listener.api,
    "EventTarget.prototype.addEventListener"
  );
  assert_eq!(
    resolved_port_add_listener.api_id,
    ApiId::from_name("EventTarget.prototype.addEventListener")
  );

  let broadcast_post_message_span = range_of(source, r#"bc.postMessage("hi")"#);
  let (broadcast_post_message_body, broadcast_post_message_expr) =
    find_call_expr(lower, broadcast_post_message_span);
  let broadcast_post_message_body_ref = lower.body(broadcast_post_message_body).expect("body");
  let resolved_broadcast_post_message = resolve_call(
    lower,
    broadcast_post_message_body,
    broadcast_post_message_body_ref,
    broadcast_post_message_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve BroadcastChannel.postMessage()");
  assert_eq!(
    resolved_broadcast_post_message.api,
    "BroadcastChannel.prototype.postMessage"
  );
  assert_eq!(
    resolved_broadcast_post_message.api_id,
    ApiId::from_name("BroadcastChannel.prototype.postMessage")
  );

  let broadcast_add_listener_span = range_of(
    source,
    r#"bc.addEventListener("message", () => {})"#,
  );
  let (broadcast_add_listener_body, broadcast_add_listener_expr) =
    find_call_expr(lower, broadcast_add_listener_span);
  let broadcast_add_listener_body_ref =
    lower.body(broadcast_add_listener_body).expect("body");
  let resolved_broadcast_add_listener = resolve_call(
    lower,
    broadcast_add_listener_body,
    broadcast_add_listener_body_ref,
    broadcast_add_listener_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve BroadcastChannel.addEventListener()");
  assert_eq!(
    resolved_broadcast_add_listener.api,
    "EventTarget.prototype.addEventListener"
  );
  assert_eq!(
    resolved_broadcast_add_listener.api_id,
    ApiId::from_name("EventTarget.prototype.addEventListener")
  );

  let ws_send_span = range_of(source, r#"ws.send("hi")"#);
  let (ws_send_body, ws_send_expr) = find_call_expr(lower, ws_send_span);
  let ws_send_body_ref = lower.body(ws_send_body).expect("body");
  let resolved_ws_send = resolve_call(
    lower,
    ws_send_body,
    ws_send_body_ref,
    ws_send_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve WebSocket.send()");
  assert_eq!(resolved_ws_send.api, "WebSocket.prototype.send");
  assert_eq!(
    resolved_ws_send.api_id,
    ApiId::from_name("WebSocket.prototype.send")
  );

  let ws_add_listener_span = range_of(source, r#"ws.addEventListener("open", () => {})"#);
  let (ws_add_listener_body, ws_add_listener_expr) = find_call_expr(lower, ws_add_listener_span);
  let ws_add_listener_body_ref = lower.body(ws_add_listener_body).expect("body");
  let resolved_ws_add_listener = resolve_call(
    lower,
    ws_add_listener_body,
    ws_add_listener_body_ref,
    ws_add_listener_expr,
    &kb,
    Some(&types),
  )
  .expect("resolve WebSocket.addEventListener()");
  assert_eq!(
    resolved_ws_add_listener.api,
    "EventTarget.prototype.addEventListener"
  );
  assert_eq!(
    resolved_ws_add_listener.api_id,
    ApiId::from_name("EventTarget.prototype.addEventListener")
  );
}
