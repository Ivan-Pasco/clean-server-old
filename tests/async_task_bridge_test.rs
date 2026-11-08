//! `_async_fire_slot` / `_async_await_slot` (HOST_BRIDGE.md §Async).
//!
//! The compiler turns the call of a `background` or `later` statement into a
//! task: a function `(env: i32) -> i32` in `__indirect_function_table`
//! (`slot` is its index) and a heap environment block `env`. The host queues
//! a fired task and runs it once the entry point that started it has
//! returned. Awaiting a task runs it now; the task function itself runs the
//! call only once and returns the address of its result cell.
//!
//! The module below plays the compiler's part with hand-written tasks.

use clean_server::router::Router;
use clean_server::wasm::{RequestContext, WasmInstance};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ASYNC_WAT: &str = r#"
(module
  (import "env" "_async_fire_slot" (func $fire (param i32 i32)))
  (import "env" "_async_await_slot" (func $await (param i32 i32) (result i32)))
  (import "env" "_roles_register" (func $roles_register (param i32 i32) (result i32)))
  (import "env" "_role_has_permission" (func $role_has_permission (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (global $heap (export "__heap_ptr") (mut i32) (i32.const 8192))
  (global $flag (mut i32) (i32.const 0))
  (global $order (mut i32) (i32.const 0))
  (global $runs (mut i32) (i32.const 0))

  (data (i32.const 16) "\03\00\00\00yes")
  (data (i32.const 32) "\02\00\00\00no")
  (data (i32.const 48) "\05\00\00\00fired")
  ;; raw role data for _roles_register / _role_has_permission
  (data (i32.const 256) "{\"editor\":[\"posts.edit\"]}")
  (data (i32.const 320) "editor")
  (data (i32.const 336) "posts.edit")

  (table (export "__indirect_function_table") 4 funcref)
  (elem (i32.const 0) $task_flag $task_trap $task_count $task_register)

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

  ;; slot 0: records that it ran, and after which task (order 2 = second).
  (func $task_flag (param $env i32) (result i32)
    global.get $order
    i32.const 1
    i32.add
    global.set $order
    global.get $order
    global.set $flag
    local.get $env
    i32.const 8
    i32.add)

  ;; slot 1: a task that traps.
  (func $task_trap (param $env i32) (result i32)
    global.get $order
    i32.const 1
    i32.add
    global.set $order
    unreachable)

  ;; slot 2: runs once (state word at env), stores 42 in the result cell.
  (func $task_count (param $env i32) (result i32)
    local.get $env
    i32.load
    if
      local.get $env
      i32.const 8
      i32.add
      return
    end
    local.get $env
    i32.const 1
    i32.store
    global.get $runs
    i32.const 1
    i32.add
    global.set $runs
    local.get $env
    i32.const 42
    i32.store offset=8
    local.get $env
    i32.const 8
    i32.add)

  ;; slot 3: a host-visible side effect (roles are shared by all instances).
  (func $task_register (param $env i32) (result i32)
    i32.const 256
    i32.const 25
    call $roles_register
    drop
    local.get $env
    i32.const 8
    i32.add)

  ;; start: fires a trapping task, then the flag task.
  (func (export "_start")
    i32.const 1
    i32.const 1024
    call $fire
    i32.const 0
    i32.const 1040
    call $fire)

  ;; "yes" when the flag task ran at startup, after the trapping one.
  (func (export "__route_handler_flag") (result i32)
    global.get $flag
    i32.const 2
    i32.eq
    if (result i32)
      i32.const 16
    else
      i32.const 32
    end)

  ;; Awaits the counting task twice: "yes" when both reads give 42 and the
  ;; task ran once.
  (func (export "__route_handler_await") (result i32)
    (local $a i32)
    (local $b i32)
    i32.const 2
    i32.const 2048
    call $await
    i32.load
    local.set $a
    i32.const 2
    i32.const 2048
    call $await
    i32.load
    local.set $b
    local.get $a
    i32.const 42
    i32.eq
    local.get $b
    i32.const 42
    i32.eq
    i32.and
    global.get $runs
    i32.const 1
    i32.eq
    i32.and
    if (result i32)
      i32.const 16
    else
      i32.const 32
    end)

  ;; Fires the role-registering task; the task must not run before the
  ;; handler returns.
  (func (export "__route_handler_fire") (result i32)
    i32.const 3
    i32.const 3072
    call $fire
    i32.const 320
    i32.const 6
    i32.const 336
    i32.const 10
    call $role_has_permission
    if (result i32)
      i32.const 32
    else
      i32.const 48
    end)

  (func (export "__route_handler_role") (result i32)
    i32.const 320
    i32.const 6
    i32.const 336
    i32.const 10
    call $role_has_permission
    if (result i32)
      i32.const 16
    else
      i32.const 32
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
    let wasm = wat::parse_str(ASYNC_WAT).expect("valid WAT");
    let instance =
        WasmInstance::from_bytes(&wasm, Arc::new(Router::new())).expect("module should load");
    instance.initialize().expect("start function should run");
    instance
}

#[test]
fn tasks_fired_at_startup_run_in_order_before_the_snapshot_and_survive_a_trap() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load_initialized();
    let body = instance
        .call_handler("__route_handler_flag", request("/flag"))
        .expect("handler should run");
    assert_eq!(
        body, "yes",
        "the flag task must run after start: returns, after the trapping task, and before the startup snapshot"
    );
}

#[test]
fn awaiting_a_task_runs_it_once_and_returns_its_result_cell() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load_initialized();
    let body = instance
        .call_handler("__route_handler_await", request("/await"))
        .expect("handler should run");
    assert_eq!(
        body, "yes",
        "both awaits must read 42 and the task must run once"
    );
}

#[test]
fn tasks_fired_by_a_request_handler_run_after_it_returns() {
    let rt = runtime();
    let _guard = rt.enter();
    let instance = load_initialized();
    let response = instance
        .call_handler_with_auth("__route_handler_fire", request("/fire"), None)
        .expect("handler should run");
    assert_eq!(
        response.body, "fired",
        "the task must not run while the handler is still running"
    );

    // The task runs off the request path; wait for its side effect.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let body = instance
            .call_handler("__route_handler_role", request("/role"))
            .expect("handler should run");
        if body == "yes" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the task fired by the handler never ran"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
