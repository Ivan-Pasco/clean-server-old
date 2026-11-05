//! Bridge registration tests for the 14 frame.jobs bridge functions.
//!
//! Unlike `bridge_contract_test.rs` (which validates entries in function-registry.toml),
//! this test directly probes each job function and its spec-defined aliases against
//! the server linker.  The job functions are not yet in function-registry.toml — they
//! will be added in a follow-up foundation commit after this server release ships.
//! See the note at the end of `job-queue-server-runtime-implementation.md`.

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::WasmState;
use std::sync::Arc;
use wasmtime::{Engine, Module, Store};

fn make_store(engine: &Engine) -> Store<WasmState> {
    let router = Arc::new(Router::new());
    Store::new(engine, WasmState::new(router))
}

/// Build a WAT module that imports a single `env` function with the given
/// signature and verify that the server linker can resolve it.
#[test]
fn job_bridge_functions_are_all_registered() {
    let engine = Engine::default();
    let linker = create_linker(&engine).expect("Failed to create server linker");

    let mut missing: Vec<String> = Vec::new();

    /// Probe a single (name, params, result) triple and append to `missing` on failure.
    macro_rules! probe {
        ($name:expr, $params:expr, $result:expr) => {{
            let wat = format!(
                r#"(module (import "env" "{name}" (func {params} {result})))"#,
                name = $name,
                params = $params,
                result = $result,
            );
            if let Ok(module) = Module::new(&engine, &wat) {
                let mut store = make_store(&engine);
                if linker.instantiate(&mut store, &module).is_err() {
                    missing.push(format!("MISSING: {}", $name));
                }
            }
        }};
    }

    // -----------------------------------------------------------------------
    // Canonical underscore names
    // -----------------------------------------------------------------------

    // _job_register(name_ptr, name_len, handler_ptr, handler_len, maxAttempts,
    //               backoff_ptr, backoff_len, delay, timeout, queue_ptr, queue_len) -> void
    probe!(
        "_job_register",
        "(param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)",
        ""
    );

    // _job_enqueue(name_ptr, name_len, args_ptr, args_len) -> i32
    probe!("_job_enqueue", "(param i32 i32 i32 i32)", "(result i32)");

    // _job_enqueue_at(name_ptr, name_len, args_ptr, args_len, run_at_seconds: f64) -> i32
    // plugin.toml declares the timestamp param as "number" (= WASM f64).
    probe!(
        "_job_enqueue_at",
        "(param i32 i32 i32 i32 f64)",
        "(result i32)"
    );

    // _job_cancel(id_ptr, id_len) -> i32
    probe!("_job_cancel", "(param i32 i32)", "(result i32)");

    // _job_status(id_ptr, id_len) -> i32
    probe!("_job_status", "(param i32 i32)", "(result i32)");

    // _job_result(id_ptr, id_len) -> i32
    probe!("_job_result", "(param i32 i32)", "(result i32)");

    // _job_current_id() -> i32
    probe!("_job_current_id", "", "(result i32)");

    // _job_current_args() -> i32
    probe!("_job_current_args", "", "(result i32)");

    // _job_current_attempt() -> i32
    probe!("_job_current_attempt", "", "(result i32)");

    // _job_retry_after(delay_seconds) -> void
    probe!("_job_retry_after", "(param i32)", "");

    // _job_fail(reason_ptr, reason_len) -> void
    probe!("_job_fail", "(param i32 i32)", "");

    // _job_succeed(result_ptr, result_len) -> void
    probe!("_job_succeed", "(param i32 i32)", "");

    // _schedule_cron(name_ptr, name_len, cron_ptr, cron_len, handler_ptr, handler_len) -> i32
    probe!(
        "_schedule_cron",
        "(param i32 i32 i32 i32 i32 i32)",
        "(result i32)"
    );

    // _schedule_cancel(name_ptr, name_len) -> i32
    probe!("_schedule_cancel", "(param i32 i32)", "(result i32)");

    // -----------------------------------------------------------------------
    // Spec-defined dot-notation aliases
    // -----------------------------------------------------------------------

    probe!("queue.enqueue", "(param i32 i32 i32 i32)", "(result i32)");
    probe!(
        "queue.enqueue_at",
        "(param i32 i32 i32 i32 f64)",
        "(result i32)"
    );
    probe!("queue.cancel", "(param i32 i32)", "(result i32)");
    probe!("queue.status", "(param i32 i32)", "(result i32)");
    probe!("queue.result", "(param i32 i32)", "(result i32)");
    probe!("job.id", "", "(result i32)");
    probe!("job.args", "", "(result i32)");
    probe!("job.attempt", "", "(result i32)");
    probe!("job.retry_after", "(param i32)", "");
    probe!("job.fail", "(param i32 i32)", "");
    probe!("job.succeed", "(param i32 i32)", "");
    probe!("schedule.cancel", "(param i32 i32)", "(result i32)");

    // -----------------------------------------------------------------------
    // Auto-derived aliases (register_bridge_fn! macro produces these)
    // -----------------------------------------------------------------------

    probe!("job.enqueue", "(param i32 i32 i32 i32)", "(result i32)");
    probe!(
        "job.enqueue_at",
        "(param i32 i32 i32 i32 f64)",
        "(result i32)"
    );
    probe!("job.cancel", "(param i32 i32)", "(result i32)");
    probe!("job.status", "(param i32 i32)", "(result i32)");
    probe!("job.result", "(param i32 i32)", "(result i32)");
    probe!("job.current_id", "", "(result i32)");
    probe!("job.current_args", "", "(result i32)");
    probe!("job.current_attempt", "", "(result i32)");

    assert!(
        missing.is_empty(),
        "job_bridge_functions_are_all_registered FAILED — {} function(s) missing:\n\n{}\n",
        missing.len(),
        missing.join("\n")
    );

    eprintln!(
        "job_bridge_functions_are_all_registered PASSED — all 14 functions and their aliases resolved."
    );
}

/// Spot-check: verify the worker-loop state management functions compile cleanly.
#[test]
fn jobs_state_is_accessible_in_wasm_state() {
    use clean_server::jobs::create_shared_jobs_state;

    // WasmState must carry a jobs_state field (compile-time check).
    let router = Arc::new(Router::new());
    let state = WasmState::new(router);
    // If jobs_state is missing, the next line fails to compile.
    let _jobs = state.jobs_state.clone();
    let _jobs2 = create_shared_jobs_state();
}
