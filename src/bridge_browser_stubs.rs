//! Browser-runtime bridge function stubs for the server.
//!
//! WASM modules compiled for browser-class targets import these bridge
//! functions (api.*, feed.*, live.*, ui.*, log.*, _assert, _panic, etc.).
//! When such a module is loaded on the server, the function registry still
//! requires every canonical name to be present in the linker — otherwise
//! `Linker::instantiate` fails with `unknown import`. These stubs satisfy
//! that contract with zero/empty returns so the module can link.
//!
//! Behaviour: each stub is a no-op that returns the registry-declared zero
//! value (0, 0.0, empty length-prefixed string, etc.). Callers on the
//! server are expected to receive sentinel values and degrade gracefully.
//!
//! AUTO-GENERATED via tools/gen_browser_stubs.py from
//! foundation/spec/platform/function-registry.toml. Re-run when
//! new browser-only Layer 2 functions are added to the registry.

use crate::error::{RuntimeError, RuntimeResult};
use crate::wasm::WasmState;
use host_bridge::write_string_to_caller;
use wasmtime::{Caller, Linker};

macro_rules! stub {
    ($linker:expr, $name:literal, $func:expr) => {{
        $linker
            .func_wrap("env", $name, $func)
            .map_err(|e| RuntimeError::wasm(format!("Failed to define {}: {}", $name, e)))?;
    }};
}

pub fn register_browser_stubs(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // ===== category: api (15 fns) =====
    stub!(linker, "_api_auth", |_caller: Caller<'_, WasmState>,
                                _a0: i32,
                                _a1: i32,
                                _a2: i32,
                                _a3: i32|
     -> i32 { 0 });
    stub!(
        linker,
        "_api_body",
        |mut caller: Caller<'_, WasmState>| -> i32 { write_string_to_caller(&mut caller, "") }
    );
    stub!(linker, "_api_clearAuth", |_caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 { 0 });
    stub!(linker, "_api_delete", |_caller: Caller<'_, WasmState>,
                                  _a0: i32,
                                  _a1: i32,
                                  _a2: i32,
                                  _a3: i32|
     -> i32 { 0 });
    stub!(linker, "_api_get", |_caller: Caller<'_, WasmState>,
                               _a0: i32,
                               _a1: i32,
                               _a2: i32,
                               _a3: i32|
     -> i32 { 0 });
    stub!(linker, "_api_header", |_caller: Caller<'_, WasmState>,
                                  _a0: i32,
                                  _a1: i32,
                                  _a2: i32,
                                  _a3: i32|
     -> i32 { 0 });
    stub!(linker, "_api_json", |mut caller: Caller<'_, WasmState>,
                                _a0: i32,
                                _a1: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    stub!(linker, "_api_ok", |_caller: Caller<'_, WasmState>| -> i32 {
        0
    });
    stub!(linker, "_api_patch", |_caller: Caller<'_, WasmState>,
                                 _a0: i32,
                                 _a1: i32,
                                 _a2: i32,
                                 _a3: i32,
                                 _a4: i32,
                                 _a5: i32|
     -> i32 { 0 });
    stub!(linker, "_api_post", |_caller: Caller<'_, WasmState>,
                                _a0: i32,
                                _a1: i32,
                                _a2: i32,
                                _a3: i32,
                                _a4: i32,
                                _a5: i32|
     -> i32 { 0 });
    stub!(linker, "_api_put", |_caller: Caller<'_, WasmState>,
                               _a0: i32,
                               _a1: i32,
                               _a2: i32,
                               _a3: i32,
                               _a4: i32,
                               _a5: i32|
     -> i32 { 0 });
    stub!(linker, "_api_responseHeader", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                          _a0: i32,
                                          _a1: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    stub!(
        linker,
        "_api_status",
        |_caller: Caller<'_, WasmState>| -> i32 { 0 }
    );
    stub!(linker, "_api_submit", |_caller: Caller<'_, WasmState>,
                                  _a0: i32,
                                  _a1: i32,
                                  _a2: i32,
                                  _a3: i32,
                                  _a4: i32,
                                  _a5: i32,
                                  _a6: i32,
                                  _a7: i32|
     -> i32 { 0 });
    stub!(linker, "_api_timeout", |_caller: Caller<'_, WasmState>,
                                   _a0: i32|
     -> i32 { 0 });

    // ===== category: console (7 fns) =====
    stub!(linker, "_assert", |_caller: Caller<'_, WasmState>,
                              _a0: i64,
                              _a1: i32,
                              _a2: i32|
     -> i64 { 0 });
    stub!(linker, "_console_info", |_caller: Caller<'_, WasmState>,
                                    _a0: i32,
                                    _a1: i32|
     -> i64 { 0 });
    stub!(linker, "_log_debug", |_caller: Caller<'_, WasmState>,
                                 _a0: i32,
                                 _a1: i32|
     -> i64 { 0 });
    stub!(linker, "_log_error", |_caller: Caller<'_, WasmState>,
                                 _a0: i32,
                                 _a1: i32|
     -> i64 { 0 });
    stub!(linker, "_log_info", |_caller: Caller<'_, WasmState>,
                                _a0: i32,
                                _a1: i32|
     -> i64 { 0 });
    stub!(linker, "_log_warn", |_caller: Caller<'_, WasmState>,
                                _a0: i32,
                                _a1: i32|
     -> i64 { 0 });
    stub!(linker, "_panic", |_caller: Caller<'_, WasmState>,
                             _a0: i32,
                             _a1: i32|
     -> i64 { 0 });

    // ===== category: feed (7 fns) =====
    stub!(linker, "_feed_close", |_caller: Caller<'_, WasmState>,
                                  _a0: i32|
     -> i32 { 0 });
    stub!(
        linker,
        "_feed_connId",
        |_caller: Caller<'_, WasmState>| -> i32 { 0 }
    );
    stub!(
        linker,
        "_feed_data",
        |mut caller: Caller<'_, WasmState>| -> i32 { write_string_to_caller(&mut caller, "") }
    );
    stub!(linker, "_feed_eventType", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    stub!(linker, "_feed_lastId", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    stub!(linker, "_feed_on", |_caller: Caller<'_, WasmState>,
                               _a0: i32,
                               _a1: i32,
                               _a2: i32,
                               _a3: i32,
                               _a4: i32|
     -> i32 { 0 });
    stub!(linker, "_feed_open", |_caller: Caller<'_, WasmState>,
                                 _a0: i32,
                                 _a1: i32,
                                 _a2: i32,
                                 _a3: i32,
                                 _a4: i32,
                                 _a5: i32|
     -> i32 { 0 });

    // ===== category: live (9 fns) =====
    stub!(linker, "_live_close", |_caller: Caller<'_, WasmState>,
                                  _a0: i32|
     -> i32 { 0 });
    stub!(linker, "_live_closeCode", |_caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 { 0 });
    stub!(linker, "_live_closeReason", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    stub!(
        linker,
        "_live_connId",
        |_caller: Caller<'_, WasmState>| -> i32 { 0 }
    );
    stub!(linker, "_live_error", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    stub!(linker, "_live_message", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    stub!(linker, "_live_open", |_caller: Caller<'_, WasmState>,
                                 _a0: i32,
                                 _a1: i32,
                                 _a2: i32,
                                 _a3: i32,
                                 _a4: i32,
                                 _a5: i32,
                                 _a6: i32,
                                 _a7: i32|
     -> i32 { 0 });
    stub!(linker, "_live_send", |_caller: Caller<'_, WasmState>,
                                 _a0: i32,
                                 _a1: i32,
                                 _a2: i32|
     -> i32 { 0 });
    stub!(linker, "_live_state", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                  _a0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ===== category: memory (3 fns) =====
    stub!(linker, "_alloc_string", |_caller: Caller<'_, WasmState>,
                                    _a0: i64|
     -> i64 { 0 });
    stub!(linker, "_memory_copy", |_caller: Caller<'_, WasmState>,
                                   _a0: i64,
                                   _a1: i64,
                                   _a2: i64|
     -> i64 { 0 });
    stub!(linker, "_memory_fill", |_caller: Caller<'_, WasmState>,
                                   _a0: i64,
                                   _a1: i64,
                                   _a2: i64|
     -> i64 { 0 });

    // ===== category: string (2 fns) =====
    stub!(linker, "_parse_float", |_caller: Caller<'_, WasmState>,
                                   _a0: i32,
                                   _a1: i32|
     -> f64 { 0.0 });
    stub!(linker, "_parse_int", |_caller: Caller<'_, WasmState>,
                                 _a0: i32,
                                 _a1: i32|
     -> i64 { 0 });

    // ===== category: time (2 fns) =====
    stub!(linker, "_time_now_seconds", |_caller: Caller<
        '_,
        WasmState,
    >|
     -> f64 { 0.0 });
    stub!(linker, "_time_performance_now", |_caller: Caller<
        '_,
        WasmState,
    >|
     -> f64 { 0.0 });

    // ===== category: ui (20 fns) =====
    stub!(
        linker,
        "_ui_clipboard_read_cb",
        |_caller: Caller<'_, WasmState>, _a0: i32, _a1: i32, _a2: i32, _a3: i32| {}
    );
    stub!(
        linker,
        "_ui_clipboard_write_cb",
        |_caller: Caller<'_, WasmState>,
         _a0: i32,
         _a1: i32,
         _a2: i32,
         _a3: i32,
         _a4: i32,
         _a5: i32| {}
    );
    stub!(
        linker,
        "_ui_download_text",
        |_caller: Caller<'_, WasmState>,
         _a0: i32,
         _a1: i32,
         _a2: i32,
         _a3: i32,
         _a4: i32,
         _a5: i32| {}
    );
    stub!(
        linker,
        "_ui_download_url",
        |_caller: Caller<'_, WasmState>, _a0: i32, _a1: i32, _a2: i32, _a3: i32| {}
    );
    stub!(linker, "_ui_fetch_cb", |_caller: Caller<'_, WasmState>,
                                   _a0: i32,
                                   _a1: i32,
                                   _a2: i32,
                                   _a3: i32,
                                   _a4: i32,
                                   _a5: i32,
                                   _a6: i32,
                                   _a7: i32,
                                   _a8: i32,
                                   _a9: i32|
     -> i32 { 0 });
    stub!(linker, "_ui_focus_trap", |_caller: Caller<
        '_,
        WasmState,
    >,
                                     _a0: i32,
                                     _a1: i32|
     -> i32 { 0 });
    stub!(
        linker,
        "_ui_focus_trap_release",
        |_caller: Caller<'_, WasmState>, _a0: i32| {}
    );
    stub!(linker, "_ui_history_back", |_caller: Caller<
        '_,
        WasmState,
    >| {});
    stub!(linker, "_ui_history_forward", |_caller: Caller<
        '_,
        WasmState,
    >| {});
    stub!(
        linker,
        "_ui_history_push",
        |_caller: Caller<'_, WasmState>, _a0: i32, _a1: i32, _a2: i32, _a3: i32| {}
    );
    stub!(
        linker,
        "_ui_history_replace",
        |_caller: Caller<'_, WasmState>, _a0: i32, _a1: i32, _a2: i32, _a3: i32| {}
    );
    stub!(linker, "_ui_intersect_observe", |_caller: Caller<
        '_,
        WasmState,
    >,
                                            _a0: i32,
                                            _a1: i32,
                                            _a2: i32,
                                            _a3: i32,
                                            _a4: f64|
     -> i32 { 0 });
    stub!(
        linker,
        "_ui_intersect_unobserve",
        |_caller: Caller<'_, WasmState>, _a0: i32| {}
    );
    stub!(linker, "_ui_observe_visible", |_caller: Caller<
        '_,
        WasmState,
    >,
                                          _a0: i32,
                                          _a1: i32,
                                          _a2: i32,
                                          _a3: i32|
     -> i32 { 0 });
    stub!(linker, "_ui_resize_observe", |_caller: Caller<
        '_,
        WasmState,
    >,
                                         _a0: i32,
                                         _a1: i32,
                                         _a2: i32,
                                         _a3: i32|
     -> i32 { 0 });
    stub!(
        linker,
        "_ui_resize_unobserve",
        |_caller: Caller<'_, WasmState>, _a0: i32| {}
    );
    stub!(linker, "_ui_set_timeout", |_caller: Caller<
        '_,
        WasmState,
    >,
                                      _a0: i32,
                                      _a1: i32,
                                      _a2: i32|
     -> i32 { 0 });
    stub!(linker, "_ui_toast", |_caller: Caller<'_, WasmState>,
                                _a0: i32,
                                _a1: i32,
                                _a2: i32,
                                _a3: i32,
                                _a4: i32,
                                _a5: i32,
                                _a6: i32|
     -> i32 { 0 });
    stub!(
        linker,
        "_ui_toast_dismiss",
        |_caller: Caller<'_, WasmState>, _a0: i32| {}
    );
    stub!(linker, "_ui_toast_dismiss_all", |_caller: Caller<
        '_,
        WasmState,
    >| {});

    Ok(())
}
