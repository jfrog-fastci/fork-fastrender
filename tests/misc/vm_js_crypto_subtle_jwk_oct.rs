use fastrender::dom2::parse_html;
use fastrender::js::{EventLoop, RunLimits, RunUntilIdleOutcome, WindowHostState};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, Result};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use vm_js::Value;

struct NoFetchResourceFetcher;

impl ResourceFetcher for NoFetchResourceFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    Err(Error::Other(format!(
      "NoFetchResourceFetcher.fetch unexpectedly called for {url:?}"
    )))
  }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
  const BLOCK_SIZE: usize = 64;
  let mut key_block = [0u8; BLOCK_SIZE];
  if key.len() > BLOCK_SIZE {
    let digest = Sha256::digest(key);
    key_block[..digest.len()].copy_from_slice(&digest);
  } else {
    key_block[..key.len()].copy_from_slice(key);
  }

  let mut o_key_pad = [0u8; BLOCK_SIZE];
  let mut i_key_pad = [0u8; BLOCK_SIZE];
  for i in 0..BLOCK_SIZE {
    o_key_pad[i] = key_block[i] ^ 0x5c;
    i_key_pad[i] = key_block[i] ^ 0x36;
  }

  let mut inner = Sha256::new();
  inner.update(i_key_pad);
  inner.update(data);
  let inner_digest = inner.finalize();

  let mut outer = Sha256::new();
  outer.update(o_key_pad);
  outer.update(inner_digest);
  let out = outer.finalize();

  let mut sig = [0u8; 32];
  sig.copy_from_slice(&out);
  sig
}

fn to_hex(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for b in bytes {
    out.push_str(&format!("{b:02x}"));
  }
  out
}

#[test]
fn vm_js_crypto_subtle_import_export_jwk_aes_gcm_roundtrip() -> Result<()> {
  // 32-byte key: 0x00..0x1f, base64url(no pad) = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
  const KEY_B64URL: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";

  let html = "<!doctype html><html><head></head><body></body></html>";
  let dom = parse_html(html)?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  let clock = event_loop.clock();
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(NoFetchResourceFetcher);
  let mut host = WindowHostState::new_with_fetcher_and_clock(
    dom,
    "https://example.com/index.html",
    fetcher,
    clock,
  )?;

  let source = format!(
    r#"
    globalThis.__done = false;
    globalThis.__ok = false;
    globalThis.__export_ok = false;
    globalThis.__err_name = null;
    (async () => {{
      try {{
        const jwk = {{
          kty: "oct",
          k: "{KEY_B64URL}",
          alg: "A256GCM",
          ext: true,
          key_ops: ["encrypt","decrypt"],
        }};
        const key = await crypto.subtle.importKey("jwk", jwk, {{ name: "AES-GCM" }}, true, ["encrypt","decrypt"]);

        const iv = new Uint8Array([0,1,2,3,4,5,6,7,8,9,10,11]);
        const pt = new Uint8Array([1,2,3,4,5,6,7,8]);
        const ct = await crypto.subtle.encrypt({{ name: "AES-GCM", iv }}, key, pt);
        const dec = await crypto.subtle.decrypt({{ name: "AES-GCM", iv }}, key, ct);
        const decBytes = new Uint8Array(dec);
        let ok = decBytes.length === pt.length;
        for (let i = 0; i < pt.length && ok; i++) {{
          if (decBytes[i] !== pt[i]) ok = false;
        }}
        globalThis.__ok = ok;

        const jwk2 = await crypto.subtle.exportKey("jwk", key);
        const ops = (jwk2 && jwk2.key_ops && jwk2.key_ops.join(",")) || "";
        globalThis.__export_ok =
          jwk2.kty === "oct" &&
          jwk2.k === "{KEY_B64URL}" &&
          jwk2.ext === true &&
          ops === "encrypt,decrypt";
      }} catch (e) {{
        globalThis.__err_name = e && e.name || String(e);
      }}
      globalThis.__done = true;
    }})();
  "#
  );

  host.exec_script_in_event_loop(&mut event_loop, &source)?;

  let mut errors: Vec<String> = Vec::new();
  assert_eq!(
    event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |err| {
      errors.push(err.to_string());
    })?,
    RunUntilIdleOutcome::Idle
  );
  assert!(errors.is_empty(), "expected no JS errors; got {errors:?}");

  assert_eq!(
    host.exec_script_in_event_loop(&mut event_loop, "globalThis.__done")?,
    Value::Bool(true)
  );
  assert_eq!(
    host.exec_script_in_event_loop(&mut event_loop, "globalThis.__err_name === null")?,
    Value::Bool(true)
  );
  assert_eq!(
    host.exec_script_in_event_loop(&mut event_loop, "globalThis.__ok")?,
    Value::Bool(true)
  );
  assert_eq!(
    host.exec_script_in_event_loop(&mut event_loop, "globalThis.__export_ok")?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn vm_js_crypto_subtle_import_jwk_ext_false_rejects_extractable_true() -> Result<()> {
  const KEY_B64URL: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";

  let html = "<!doctype html><html><head></head><body></body></html>";
  let dom = parse_html(html)?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  let clock = event_loop.clock();
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(NoFetchResourceFetcher);
  let mut host = WindowHostState::new_with_fetcher_and_clock(
    dom,
    "https://example.com/index.html",
    fetcher,
    clock,
  )?;

  let source = format!(
    r#"
    globalThis.__done = false;
    globalThis.__err_name = null;
    (async () => {{
      try {{
        const jwk = {{ kty: "oct", k: "{KEY_B64URL}", ext: false, key_ops: ["encrypt"] }};
        await crypto.subtle.importKey("jwk", jwk, {{ name: "AES-GCM" }}, true, ["encrypt"]);
        globalThis.__err_name = "resolved";
      }} catch (e) {{
        globalThis.__err_name = e && e.name || String(e);
      }}
      globalThis.__done = true;
    }})();
    "#
  );

  host.exec_script_in_event_loop(&mut event_loop, &source)?;

  let mut errors: Vec<String> = Vec::new();
  assert_eq!(
    event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |err| {
      errors.push(err.to_string());
    })?,
    RunUntilIdleOutcome::Idle
  );
  assert!(errors.is_empty(), "expected no JS errors; got {errors:?}");

  assert_eq!(
    host.exec_script_in_event_loop(&mut event_loop, "globalThis.__done")?,
    Value::Bool(true)
  );
  assert_eq!(
    host.exec_script_in_event_loop(&mut event_loop, "globalThis.__err_name === 'DataError'")?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn vm_js_crypto_subtle_import_jwk_hs256_sign_matches_rust() -> Result<()> {
  // 32-byte key: 0x00..0x1f
  const KEY_B64URL: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
  let key_bytes: Vec<u8> = (0u8..32).collect();
  let data = b"abc";
  let expected_hex = to_hex(&hmac_sha256(&key_bytes, data));

  let html = "<!doctype html><html><head></head><body></body></html>";
  let dom = parse_html(html)?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  let clock = event_loop.clock();
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(NoFetchResourceFetcher);
  let mut host = WindowHostState::new_with_fetcher_and_clock(
    dom,
    "https://example.com/index.html",
    fetcher,
    clock,
  )?;

  let source = format!(
    r#"
    globalThis.__done = false;
    globalThis.__sig_hex = null;
    globalThis.__err_name = null;
    (async () => {{
      try {{
        const jwk = {{
          kty: "oct",
          k: "{KEY_B64URL}",
          alg: "HS256",
          ext: true,
          key_ops: ["sign"],
        }};
        const key = await crypto.subtle.importKey(
          "jwk",
          jwk,
          {{ name: "HMAC", hash: {{ name: "SHA-256" }} }},
          true,
          ["sign"],
        );
        const data = new TextEncoder().encode("abc");
        const sig = await crypto.subtle.sign("HMAC", key, data);
        const bytes = new Uint8Array(sig);
        const HEX = ["0","1","2","3","4","5","6","7","8","9","a","b","c","d","e","f"];
        let s = "";
        for (let i = 0; i < bytes.length; i++) {{
          const b = bytes[i];
          s += HEX[b >> 4];
          s += HEX[b & 15];
        }}
        globalThis.__sig_hex = s;
      }} catch (e) {{
        globalThis.__err_name = e && e.name || String(e);
      }}
      globalThis.__done = true;
    }})();
    "#
  );

  host.exec_script_in_event_loop(&mut event_loop, &source)?;

  let mut errors: Vec<String> = Vec::new();
  assert_eq!(
    event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |err| {
      errors.push(err.to_string());
    })?,
    RunUntilIdleOutcome::Idle
  );
  assert!(errors.is_empty(), "expected no JS errors; got {errors:?}");

  assert_eq!(
    host.exec_script_in_event_loop(&mut event_loop, "globalThis.__done")?,
    Value::Bool(true)
  );
  assert_eq!(
    host.exec_script_in_event_loop(&mut event_loop, "globalThis.__err_name === null")?,
    Value::Bool(true)
  );
  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    &format!("globalThis.__sig_hex === \"{expected_hex}\""),
  )?;
  assert!(
    matches!(ok, Value::Bool(true)),
    "expected signature to match {expected_hex:?}; got {ok:?}"
  );
  Ok(())
}

