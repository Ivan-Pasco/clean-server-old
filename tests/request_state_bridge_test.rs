//! Request-scoped state bridges: locale selection, request sessions, and the
//! MCP HTTP transport's header access and API-key enforcement.
//!
//! Each test drives the real server linker (`create_linker`) from a small WAT
//! module, so the bridges run exactly as they do for a compiled application.
//!
//! - Locale (frame-locale §1 `default`, §3 `locale.set`): `_i18n_set_locale`
//!   at startup sets the module default; during a request it changes only
//!   that request's locale. `_i18n_configure` (the `locale:` block) loads
//!   every translation file of the path and selects how requests choose
//!   their locale (`detection = header | cookie | query | manual`).
//! - Sessions (frame-auth AUTH-S001/S011): a session stored with
//!   `_session_store` during a request is that request's session, and a later
//!   request finds it through the application's configured cookie name.
//! - MCP HTTP (frame.mcp §8.2, §4.2): JSON-RPC is served on `POST /`, request
//!   headers are readable via `_mcp_http_header`, and `_mcp_http_require_key`
//!   makes the host answer 401 to requests without the API key.

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::session::SessionConfig;
use clean_server::wasm::{RequestContext, WasmState};
use std::collections::HashMap;
use std::sync::Arc;
use wasmtime::{Engine, Instance, Module, Store};

const DRIVER_WAT: &str = r#"
(module
  (import "env" "_i18n_set_locale" (func $set_locale (param i32 i32)))
  (import "env" "_i18n_locale" (func $locale (result i32)))
  (import "env" "_i18n_configure" (func $configure (param i32 i32 i32 i32 i32 i32 i32 i32)))
  (import "env" "_i18n_t" (func $t (param i32 i32 i32 i32) (result i32)))
  (import "env" "_session_store" (func $session_store (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "_session_get" (func $session_get (result i32)))
  (import "env" "_mcp_http_serve" (func $mcp_serve (param i32 i32 i32) (result i32)))
  (import "env" "_mcp_http_require_key" (func $mcp_key (param i32 i32) (result i32)))
  (import "env" "_mcp_http_accept" (func $mcp_accept (result i32)))
  (import "env" "_mcp_http_header" (func $mcp_header (param i32 i32) (result i32)))
  (import "env" "_mcp_http_respond" (func $mcp_respond (param i32 i32) (result i32)))

  (memory (export "memory") 4)
  (global $heap (export "__heap_ptr") (mut i32) (i32.const 8192))

  (func (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap
      (i32.and
        (i32.add (i32.add (global.get $heap) (local.get $size)) (i32.const 7))
        (i32.const -8)))
    (local.get $ptr))

  (func (export "set_locale") (param i32 i32) (call $set_locale (local.get 0) (local.get 1)))
  (func (export "locale") (result i32) (call $locale))
  (func (export "configure") (param i32 i32 i32 i32 i32 i32 i32 i32)
    (call $configure (local.get 0) (local.get 1) (local.get 2) (local.get 3)
                     (local.get 4) (local.get 5) (local.get 6) (local.get 7)))
  (func (export "t") (param i32 i32 i32 i32) (result i32)
    (call $t (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  ;; session_store(sid_lp, key_lp, value_lp) — Clean length-prefixed pointers
  (func (export "session_store") (param i32 i32 i32) (result i32)
    (call $session_store (local.get 0) (local.get 1) (local.get 2) (i32.const 0) (i32.const 0)))
  (func (export "session_get") (result i32) (call $session_get))
  (func (export "mcp_serve") (param i32 i32 i32) (result i32)
    (call $mcp_serve (local.get 0) (local.get 1) (local.get 2)))
  (func (export "mcp_key") (param i32 i32) (result i32) (call $mcp_key (local.get 0) (local.get 1)))
  (func (export "mcp_accept") (result i32) (call $mcp_accept))
  (func (export "mcp_header") (param i32 i32) (result i32) (call $mcp_header (local.get 0) (local.get 1)))
  (func (export "mcp_respond") (param i32 i32) (result i32) (call $mcp_respond (local.get 0) (local.get 1)))
)
"#;

struct Driver {
    instance: Instance,
    store: Store<WasmState>,
    /// Scratch cursor for argument strings, below the module's heap.
    scratch: usize,
}

impl Driver {
    fn new(engine: &Engine, module: &Module, state: WasmState) -> Self {
        let linker = create_linker(engine).expect("server linker");
        let mut store = Store::new(engine, state);
        let instance = linker.instantiate(&mut store, module).expect("instantiate");
        Self {
            instance,
            store,
            scratch: 64,
        }
    }

    /// Place `s` in memory as `[len: u32 LE][bytes]`; returns (lp_ptr, raw_ptr, len).
    fn put(&mut self, s: &str) -> (i32, i32, i32) {
        let memory = self.instance.get_memory(&mut self.store, "memory").unwrap();
        let ptr = self.scratch;
        let buf = memory.data_mut(&mut self.store);
        buf[ptr..ptr + 4].copy_from_slice(&(s.len() as u32).to_le_bytes());
        buf[ptr + 4..ptr + 4 + s.len()].copy_from_slice(s.as_bytes());
        self.scratch = (ptr + 4 + s.len() + 7) & !7;
        (ptr as i32, (ptr + 4) as i32, s.len() as i32)
    }

    fn read(&mut self, ptr: i32) -> String {
        let memory = self.instance.get_memory(&mut self.store, "memory").unwrap();
        let buf = memory.data(&self.store);
        let p = ptr as usize;
        let len = u32::from_le_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
        String::from_utf8(buf[p + 4..p + 4 + len].to_vec()).unwrap()
    }

    fn call0(&mut self, name: &str) -> i32 {
        let f = self
            .instance
            .get_typed_func::<(), i32>(&mut self.store, name)
            .unwrap();
        f.call(&mut self.store, ()).unwrap()
    }

    fn call_raw(&mut self, name: &str, s: &str) -> i32 {
        let (_, raw, len) = self.put(s);
        let f = self
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut self.store, name)
            .unwrap();
        f.call(&mut self.store, (raw, len)).unwrap()
    }

    fn set_locale(&mut self, tag: &str) {
        let (_, raw, len) = self.put(tag);
        let f = self
            .instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, "set_locale")
            .unwrap();
        f.call(&mut self.store, (raw, len)).unwrap();
    }

    fn locale(&mut self) -> String {
        let p = self.call0("locale");
        self.read(p)
    }

    /// `_i18n_configure(default, fallback, detection, path)`.
    fn configure(&mut self, default: &str, fallback: &str, detection: &str, path: &str) {
        let (_, d, dl) = self.put(default);
        let (_, f, fl) = self.put(fallback);
        let (_, m, ml) = self.put(detection);
        let (_, p, pl) = self.put(path);
        let func = self
            .instance
            .get_typed_func::<(i32, i32, i32, i32, i32, i32, i32, i32), ()>(
                &mut self.store,
                "configure",
            )
            .unwrap();
        func.call(&mut self.store, (d, dl, f, fl, m, ml, p, pl))
            .unwrap();
    }

    /// `t(key, "{}")`.
    fn t(&mut self, key: &str) -> String {
        let (_, k, kl) = self.put(key);
        let (_, p, pl) = self.put("{}");
        let func = self
            .instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut self.store, "t")
            .unwrap();
        let ptr = func.call(&mut self.store, (k, kl, p, pl)).unwrap();
        self.read(ptr)
    }
}

fn request(headers: &[(&str, &str)], query: &[(&str, &str)]) -> RequestContext {
    RequestContext {
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: String::new(),
        body_bytes: None,
        params: HashMap::new(),
        query: query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>(),
    }
}

fn setup() -> (Engine, Module) {
    let engine = Engine::default();
    let module = Module::new(&engine, DRIVER_WAT).expect("driver WAT");
    (engine, module)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locale_default_from_startup_and_per_request_set() {
    let (engine, module) = setup();
    let router = Arc::new(Router::new());

    // Startup: frame.locale emits `_i18n_set_locale("fr")` into the entry point.
    let mut init_state = WasmState::new(router.clone());
    init_state.initializing = true;
    let locale_state = init_state.locale_state.clone();
    let mut init = Driver::new(&engine, &module, init_state);
    init.set_locale("fr");

    let fresh_request = |headers: &[(&str, &str)], query: &[(&str, &str)]| {
        let mut state = WasmState::new(router.clone());
        state.locale_state = locale_state.clone();
        state.set_request(request(headers, query));
        Driver::new(&engine, &module, state)
    };

    // A request sees the module default, and locale.set changes it for that
    // request only.
    let mut req = fresh_request(&[], &[]);
    assert_eq!(req.locale(), "fr");
    req.set_locale("de");
    assert_eq!(req.locale(), "de");

    let mut next = fresh_request(&[], &[]);
    assert_eq!(
        next.locale(),
        "fr",
        "locale.set must not leak across requests"
    );

    // Request detection order: ?locale= query, then the cl_locale cookie.
    let mut by_query = fresh_request(&[("cookie", "cl_locale=it")], &[("locale", "es")]);
    assert_eq!(by_query.locale(), "es");
    let mut by_cookie = fresh_request(&[("Cookie", "cl_locale=it")], &[]);
    assert_eq!(by_cookie.locale(), "it");
}

/// A translations folder with en, fr, de and es files.
fn translations_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let files = [
        ("en", r#"{"hello": "Hello", "bye": "Goodbye"}"#),
        (
            "fr",
            r#"{"hello": "Bonjour", "bye": "Au revoir", "only_fr": "Seulement"}"#,
        ),
        ("de", r#"{"hello": "Hallo"}"#),
        ("es", r#"{"hello": "Hola"}"#),
    ];
    for (tag, json) in files {
        std::fs::write(dir.path().join(format!("{tag}.json")), json).unwrap();
    }
    // Not a translation file: ignored.
    std::fs::write(dir.path().join("README.txt"), "not json").unwrap();
    dir
}

/// Request headers / query parameters as `(name, value)` pairs.
type Pairs<'a> = &'a [(&'a str, &'a str)];

/// Configure the `locale:` block at startup and return a request factory
/// sharing that module state.
fn configured_module(
    engine: &Engine,
    module: &Module,
    detection: &str,
    dir: &std::path::Path,
) -> impl Fn(Pairs, Pairs) -> Driver {
    let router = Arc::new(Router::new());
    let mut init_state = WasmState::new(router.clone());
    init_state.initializing = true;
    let locale_state = init_state.locale_state.clone();
    let mut init = Driver::new(engine, module, init_state);
    init.configure("en", "fr", detection, dir.to_str().unwrap());
    let engine = engine.clone();
    let module = module.clone();
    move |headers: Pairs, query: Pairs| {
        let mut state = WasmState::new(router.clone());
        state.locale_state = locale_state.clone();
        state.set_request(request(headers, query));
        Driver::new(&engine, &module, state)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configure_loads_every_translation_file_and_detects_from_header() {
    let (engine, module) = setup();
    let dir = translations_dir();
    let fresh = configured_module(&engine, &module, "header", dir.path());

    // Without a preference the request gets the default locale.
    let mut plain = fresh(&[], &[]);
    assert_eq!(plain.locale(), "en");
    assert_eq!(plain.t("hello"), "Hello");

    // de and es are neither the default nor the fallback, yet their files
    // were loaded from the translations path.
    let mut german = fresh(&[("Accept-Language", "de-DE,de;q=0.9,en;q=0.5")], &[]);
    assert_eq!(german.locale(), "de");
    assert_eq!(german.t("hello"), "Hallo");
    // A key missing from de comes from the default, then the fallback.
    assert_eq!(german.t("bye"), "Goodbye");
    assert_eq!(german.t("only_fr"), "Seulement");
    let mut spanish = fresh(&[("accept-language", "es")], &[]);
    assert_eq!(spanish.t("hello"), "Hola");

    // detection = "header": the query parameter and the cookie are ignored.
    let mut other = fresh(&[("Cookie", "cl_locale=fr")], &[("locale", "es")]);
    assert_eq!(other.locale(), "en");
    // An Accept-Language with no loaded locale falls back to the default.
    let mut unknown = fresh(&[("Accept-Language", "ja")], &[]);
    assert_eq!(unknown.locale(), "en");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configure_detection_cookie_query_and_manual() {
    let (engine, module) = setup();
    let dir = translations_dir();
    let all = [
        ("Cookie", "theme=dark; cl_locale=fr"),
        ("Accept-Language", "de"),
    ];
    let query = [("locale", "es")];

    let by_cookie = configured_module(&engine, &module, "cookie", dir.path());
    let mut req = by_cookie(&all, &query);
    assert_eq!(
        req.locale(),
        "fr",
        "detection = cookie reads cl_locale only"
    );
    assert_eq!(req.t("hello"), "Bonjour");
    assert_eq!(
        by_cookie(&[("Accept-Language", "de")], &query).locale(),
        "en"
    );

    let by_query = configured_module(&engine, &module, "query", dir.path());
    let mut req = by_query(&all, &query);
    assert_eq!(req.locale(), "es", "detection = query reads ?locale= only");
    assert_eq!(req.t("hello"), "Hola");
    assert_eq!(by_query(&all, &[]).locale(), "en");

    let manual = configured_module(&engine, &module, "manual", dir.path());
    let mut req = manual(&all, &query);
    assert_eq!(req.locale(), "en", "detection = manual ignores the request");
    req.set_locale("de");
    assert_eq!(req.locale(), "de", "locale.set still selects the locale");
    assert_eq!(req.t("hello"), "Hallo");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_stored_in_request_is_current_and_found_by_configured_cookie() {
    let (engine, module) = setup();
    let router = Arc::new(Router::new());

    let mut first_state = WasmState::new(router.clone());
    let sessions = first_state.session_store.clone();
    sessions.write().unwrap().set_config(SessionConfig {
        cookie_name: "app.sid".to_string(),
        ..SessionConfig::default()
    });
    first_state.set_request(request(&[], &[]));
    let mut first = Driver::new(&engine, &module, first_state);

    let blob = r#"{"userId":7,"claims":{"role":"user"},"expiry":0}"#;
    let (sid, _, _) = first.put("sid-123");
    let (key, _, _) = first.put("");
    let (value, _, _) = first.put(blob);
    let f = first
        .instance
        .get_typed_func::<(i32, i32, i32), i32>(&mut first.store, "session_store")
        .unwrap();
    assert_eq!(f.call(&mut first.store, (sid, key, value)).unwrap(), 1);
    // Same request, no cookie yet: the stored session is the current one.
    let got = first.call0("session_get");
    assert_eq!(first.read(got), blob);

    // Next request: found through the configured cookie name ...
    let mut second_state = WasmState::new(router.clone());
    second_state.session_store = sessions.clone();
    second_state.set_request(request(&[("Cookie", "theme=dark; app.sid=sid-123")], &[]));
    let mut second = Driver::new(&engine, &module, second_state);
    let got = second.call0("session_get");
    assert_eq!(second.read(got), blob);

    // ... and not through an unrelated cookie name.
    let mut third_state = WasmState::new(router);
    third_state.session_store = sessions;
    third_state.set_request(request(&[("Cookie", "session=sid-123")], &[]));
    let mut third = Driver::new(&engine, &module, third_state);
    let got = third.call0("session_get");
    assert_eq!(third.read(got), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_http_serves_root_exposes_headers_and_enforces_api_key() {
    let (engine, module) = setup();
    let port = portpicker::pick_unused_port().expect("free port");
    let mut driver = Driver::new(&engine, &module, WasmState::new(Arc::new(Router::new())));

    let (_, host_raw, host_len) = driver.put("127.0.0.1");
    let serve = driver
        .instance
        .get_typed_func::<(i32, i32, i32), i32>(&mut driver.store, "mcp_serve")
        .unwrap();
    assert_eq!(
        serve
            .call(&mut driver.store, (port as i32, host_raw, host_len))
            .unwrap(),
        1
    );
    assert_eq!(
        driver.call_raw("mcp_key", ""),
        0,
        "empty key must be refused"
    );
    assert_eq!(driver.call_raw("mcp_key", "k1"), 1);

    let url = format!("http://127.0.0.1:{}/", port);
    let client = reqwest::Client::new();
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    // Wait for the listener.
    let mut unauthorized = None;
    for _ in 0..50 {
        match client.post(&url).body(ping).send().await {
            Ok(resp) => {
                unauthorized = Some(resp.status().as_u16());
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    assert_eq!(unauthorized, Some(401), "request without the API key");
    let wrong = client
        .post(&url)
        .bearer_auth("k2")
        .body(ping)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status().as_u16(), 401, "request with a wrong key");

    // Authorized request on POST /: the module accepts it, reads a header,
    // and answers with it.
    let pending = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        async move {
            client
                .post(&url)
                .bearer_auth("k1")
                .header("X-Trace", "abc")
                .body(ping)
                .send()
                .await
                .unwrap()
        }
    });
    let answered = tokio::task::spawn_blocking(move || {
        let body_ptr = driver.call0("mcp_accept");
        let body = driver.read(body_ptr);
        let header_ptr = driver.call_raw("mcp_header", "x-trace");
        let trace = driver.read(header_ptr);
        let auth_ptr = driver.call_raw("mcp_header", "Authorization");
        let auth = driver.read(auth_ptr);
        let reply = format!(r#"{{"trace":"{}","auth":"{}"}}"#, trace, auth);
        assert_eq!(driver.call_raw("mcp_respond", &reply), 1);
        body
    })
    .await
    .unwrap();
    assert_eq!(answered, ping);
    let resp = pending.await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.text().await.unwrap(),
        r#"{"trace":"abc","auth":"Bearer k1"}"#
    );
}
