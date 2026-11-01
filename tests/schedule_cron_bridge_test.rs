//! `_schedule_cron` return-value contract.
//!
//! function-registry.toml declares `_schedule_cron` as
//! `params = ["string", "string", "string"]`, `returns = "ptr"`: it returns a
//! length-prefixed registration ID, or an empty string on error. The frame.jobs
//! plugin exposes it as `schedule.cron(...) -> string` and documents
//! `schedule.cancel(scheduleId)`, so the returned ID must be accepted by
//! `_schedule_cancel`.

use clean_server::router::Router;
use clean_server::wasm::{RequestContext, WasmInstance};
use std::collections::HashMap;
use std::sync::Arc;

const SCHEDULE_WAT: &str = r#"
(module
  (import "env" "_schedule_cron" (func $cron (param i32 i32 i32 i32 i32 i32) (result i32)))
  (import "env" "_schedule_cancel" (func $cancel (param i32 i32) (result i32)))
  (memory (export "memory") 2)
  (global $heap (export "__heap_ptr") (mut i32) (i32.const 4096))
  (global $last_id (mut i32) (i32.const 0))

  (data (i32.const 16) "nightly-report")
  (data (i32.const 48) "0 3 * * *")
  (data (i32.const 64) "nightlyReport")
  (data (i32.const 80) "not a cron")
  (data (i32.const 96)  "\03\00\00\00yes")
  (data (i32.const 112) "\02\00\00\00no")
  (data (i32.const 128) "nightly-other")

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

  (func (export "_start"))

  ;; Returns the string pointer produced by _schedule_cron for a valid schedule.
  (func (export "__route_handler_register") (result i32)
    i32.const 16 i32.const 14
    i32.const 48 i32.const 9
    i32.const 64 i32.const 13
    call $cron)

  ;; Returns the string pointer produced by _schedule_cron for an invalid expression.
  (func (export "__route_handler_invalid") (result i32)
    i32.const 16 i32.const 14
    i32.const 80 i32.const 10
    i32.const 64 i32.const 13
    call $cron)

  ;; Registers a schedule, then cancels it using the returned ID.
  (func (export "__route_handler_cancel_by_id") (result i32)
    (local $id i32)
    i32.const 128 i32.const 13
    i32.const 48 i32.const 9
    i32.const 64 i32.const 13
    call $cron
    local.tee $id
    i32.const 4
    i32.add
    local.get $id
    i32.load
    call $cancel
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

fn request() -> RequestContext {
    RequestContext {
        method: "GET".to_string(),
        path: "/test".to_string(),
        headers: Vec::new(),
        body: String::new(),
        body_bytes: None,
        params: HashMap::new(),
        query: HashMap::new(),
    }
}

fn load() -> WasmInstance {
    let wasm = wat::parse_str(SCHEDULE_WAT).expect("valid WAT");
    let instance =
        WasmInstance::from_bytes(&wasm, Arc::new(Router::new())).expect("module should load");
    instance.initialize().expect("start function should run");
    instance
}

#[test]
fn schedule_cron_returns_a_registration_id_string() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load();
    let id = instance
        .call_handler("__route_handler_register", request())
        .expect("_schedule_cron must return a readable length-prefixed string");
    assert!(
        !id.is_empty(),
        "a valid schedule must return a non-empty registration ID"
    );
    assert!(
        rt.block_on(instance.jobs_state.lock())
            .schedules
            .contains_key("nightly-report"),
        "the schedule must be registered under its name"
    );
}

#[test]
fn schedule_cron_returns_empty_string_for_invalid_expression() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load();
    let id = instance
        .call_handler("__route_handler_invalid", request())
        .expect("_schedule_cron must return a readable length-prefixed string");
    assert_eq!(
        id, "",
        "an invalid cron expression must return an empty string"
    );
}

#[test]
fn schedule_cancel_accepts_the_returned_registration_id() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load();
    let result = instance
        .call_handler("__route_handler_cancel_by_id", request())
        .expect("handler should run");
    assert_eq!(
        result, "yes",
        "_schedule_cancel must cancel the schedule identified by the ID _schedule_cron returned"
    );
}
