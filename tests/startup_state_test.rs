//! Startup state must be visible to every request handler.
//!
//! The server runs each request on a fresh WASM instance (SERVER_EXTENSIONS.md
//! "Request/Response Flow" step 4, MEMORY_POLICY.md "Fresh instance per
//! request"). State produced by the module's start function — module-level
//! globals such as the frame.auth JWT secret, heap data those globals point
//! to, and host-side registrations such as `_roles_register` — must still be
//! visible inside every handler, while anything a handler mutates must not
//! leak into the next request.

use clean_server::router::Router;
use clean_server::wasm::{RequestContext, WasmInstance};
use std::collections::HashMap;
use std::sync::Arc;

/// Module layout:
/// - `$heap` (exported `__heap_ptr`) and a bump `malloc` matching the
///   compiler's allocator contract.
/// - `$secret` and `$counter` are mutable globals that are NOT exported, just
///   like compiler-generated module-level state (`__jwt_secret`, etc.).
/// - `_start` copies the "startup-secret" literal into a heap allocation and
///   stores the pointer in `$secret`, then registers roles with the host.
const STARTUP_STATE_WAT: &str = r#"
(module
  (import "env" "_roles_register" (func $roles_register (param i32 i32) (result i32)))
  (import "env" "_role_has_permission" (func $role_has_permission (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (global $heap (export "__heap_ptr") (mut i32) (i32.const 4096))
  (global $secret (mut i32) (i32.const 0))
  (global $counter (mut i32) (i32.const 0))

  ;; length-prefixed string literals
  (data (i32.const 16)  "\0e\00\00\00startup-secret")
  (data (i32.const 64)  "\05\00\00\00first")
  (data (i32.const 80)  "\06\00\00\00leaked")
  (data (i32.const 96)  "\03\00\00\00yes")
  (data (i32.const 112) "\02\00\00\00no")
  (data (i32.const 128) "\05\00\00\00empty")
  ;; raw (not length-prefixed) role data
  (data (i32.const 256) "{\"admin\":[\"users.create\"]}")
  (data (i32.const 320) "admin")
  (data (i32.const 336) "users.create")

  (func $malloc (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    global.get $heap
    local.set $ptr
    global.get $heap
    local.get $size
    i32.add
    i32.const 7
    i32.add
    i32.const -8
    i32.and
    global.set $heap
    local.get $ptr)

  (func (export "_start")
    (local $p i32)
    ;; copy the 18-byte "startup-secret" string onto the heap
    i32.const 18
    call $malloc
    local.set $p
    local.get $p
    i32.const 16
    i32.const 18
    memory.copy
    local.get $p
    global.set $secret
    ;; host-side registration performed at startup
    i32.const 256
    i32.const 26
    call $roles_register
    drop)

  ;; Returns the string stored in the startup-initialised global.
  (func (export "__route_handler_secret") (result i32)
    global.get $secret
    if (result i32)
      global.get $secret
    else
      i32.const 128
    end)

  ;; Mutates module state; must observe a pristine copy on every request.
  (func (export "__route_handler_counter") (result i32)
    global.get $counter
    i32.const 1
    i32.add
    global.set $counter
    global.get $counter
    i32.const 1
    i32.eq
    if (result i32)
      i32.const 64
    else
      i32.const 80
    end)

  ;; Checks a role registered during startup.
  (func (export "__route_handler_role") (result i32)
    i32.const 320
    i32.const 5
    i32.const 336
    i32.const 12
    call $role_has_permission
    if (result i32)
      i32.const 96
    else
      i32.const 112
    end)
)
"#;

/// Bridge functions call `tokio::task::block_in_place`, which needs a
/// multi-threaded runtime context; tests enter one explicitly.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn request(path: &str) -> RequestContext {
    RequestContext {
        method: "GET".to_string(),
        path: path.to_string(),
        headers: Vec::new(),
        body: String::new(),
        body_bytes: None,
        params: HashMap::new(),
        query: HashMap::new(),
    }
}

fn load_initialized() -> WasmInstance {
    let wasm = wat::parse_str(STARTUP_STATE_WAT).expect("valid WAT");
    let instance =
        WasmInstance::from_bytes(&wasm, Arc::new(Router::new())).expect("module should load");
    instance.initialize().expect("start function should run");
    instance
}

#[test]
fn start_function_globals_are_visible_to_request_handlers() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load_initialized();
    let body = instance
        .call_handler("__route_handler_secret", request("/secret"))
        .expect("handler should run");
    assert_eq!(
        body, "startup-secret",
        "a global assigned in the start function must keep its value inside handlers"
    );

    // Every request sees the same startup state, not only the first one.
    let body = instance
        .call_handler("__route_handler_secret", request("/secret"))
        .expect("handler should run");
    assert_eq!(body, "startup-secret");
}

#[test]
fn roles_registered_at_startup_are_visible_to_request_handlers() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load_initialized();
    let body = instance
        .call_handler("__route_handler_role", request("/role"))
        .expect("handler should run");
    assert_eq!(
        body, "yes",
        "roles registered by _roles_register in the start function must be visible per request"
    );
}

#[test]
fn request_mutations_do_not_leak_into_later_requests() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load_initialized();
    for _ in 0..3 {
        let body = instance
            .call_handler("__route_handler_counter", request("/counter"))
            .expect("handler should run");
        assert_eq!(
            body, "first",
            "each request must start from the post-startup state, not the previous request's state"
        );
    }
}

#[test]
fn startup_state_is_visible_through_the_auth_aware_entry_point() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load_initialized();
    let response = instance
        .call_handler_with_auth("__route_handler_secret", request("/secret"), None)
        .expect("handler should run");
    assert_eq!(response.body, "startup-secret");
}
