//! Frame.ui bridge function stubs for the server runtime.
//!
//! Frame.ui browser-side bridge functions are no-ops on the server. However,
//! WASM modules compiled with `component: client="on"` and an `events:` block
//! emit imports for every `_ui_on_event`, `_ui_toggle_class`, `_storage_*`, etc.
//! bridge function regardless of the execution target.
//!
//! Without these stubs, `Linker::instantiate` fails with
//! `unknown import: env::_ui_on_event` whenever the server loads a `.wasm` that
//! has any client-hydration component in it.
//!
//! This file registers a no-op for every frame.ui bridge entry that the
//! function registry (`foundation/spec/platform/function-registry.toml`)
//! scopes to `hosts = ["browser", "server"]`, plus browser-only functions
//! that the `events:` DSL may emit into server-mode WASM. None of these stubs
//! perform any work — browser interactions are not executed server-side.
//! `tests/ui_stubs_test.rs` checks the set against the registry.
//!
//! Server-side UI functions (`_ui_render_page`, `_ui_load_layout`, etc.) are
//! already registered with real implementations in `bridge.rs::register_ui_functions`.

use crate::error::{RuntimeError, RuntimeResult};
use crate::wasm::WasmState;
use host_bridge::write_string_to_caller;
use wasmtime::{Caller, Linker};

macro_rules! register_bridge_fn {
    ($linker:expr, $name:literal, $func:expr) => {{
        $linker
            .func_wrap("env", $name, $func)
            .map_err(|e| RuntimeError::wasm(format!("Failed to define {}: {}", $name, e)))?;
        let _stripped: &str = $name.trim_start_matches('_');
        if $name.starts_with('_') && !$name.starts_with("__") {
            if let Some(_dot_idx) = _stripped.find('_') {
                let _dot_name =
                    format!("{}.{}", &_stripped[.._dot_idx], &_stripped[_dot_idx + 1..]);
                $linker
                    .alias("env", $name, "env", &_dot_name)
                    .map_err(|e| {
                        RuntimeError::wasm(format!(
                            "Failed to alias {} -> {}: {}",
                            $name, _dot_name, e
                        ))
                    })?;
            }
        }
    }};
}

pub fn register_ui_stubs(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // ── Event Handler Registration ────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_on_event", |_caller: Caller<'_, WasmState>,
                                                 _p0: i32,
                                                 _l0: i32,
                                                 _p1: i32,
                                                 _l1: i32,
                                                 _p2: i32,
                                                 _l2: i32|
     -> i32 { 0 });

    // ── State Management ──────────────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_set_state", |_caller: Caller<'_, WasmState>,
                                                  _p0: i32,
                                                  _l0: i32,
                                                  _p1: i32,
                                                  _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_get_state", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                  _p0: i32,
                                                  _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ── DOM Manipulation ──────────────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_update_element", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                       _p0: i32,
                                                       _l0: i32,
                                                       _p1: i32,
                                                       _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_update_attr", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                    _p0: i32,
                                                    _l0: i32,
                                                    _p1: i32,
                                                    _l1: i32,
                                                    _p2: i32,
                                                    _l2: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_update_element_self", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                            _p0: i32,
                                                            _l0: i32|
     -> i32 { 0 });

    // ── Form Binding & Validation ─────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_bind_input", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                   _p0: i32,
                                                   _l0: i32,
                                                   _p1: i32,
                                                   _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_validate", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                 _p0: i32,
                                                 _l0: i32,
                                                 _p1: i32,
                                                 _l1: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_input_value", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    _p0: i32,
                                                    _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_form_json", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                  _p0: i32,
                                                  _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "{}")
    });
    register_bridge_fn!(linker, "_ui_form_data", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                  _p0: i32,
                                                  _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_checked", |_caller: Caller<'_, WasmState>,
                                                _p0: i32,
                                                _l0: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_set_input", |_caller: Caller<'_, WasmState>,
                                                  _p0: i32,
                                                  _l0: i32,
                                                  _p1: i32,
                                                  _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_form_submit", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                    _p0: i32,
                                                    _l0: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_insert_at_cursor", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                         _p0: i32,
                                                         _l0: i32,
                                                         _p1: i32,
                                                         _l1: i32,
                                                         _p2: i32,
                                                         _l2: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_text_diff", |_caller: Caller<'_, WasmState>,
                                                  _p0: i32,
                                                  _l0: i32,
                                                  _p1: i32,
                                                  _l1: i32,
                                                  _p2: i32,
                                                  _l2: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_get_selection", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                      _p0: i32,
                                                      _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ── Event Handler Context ─────────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_event_attr", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                   _p0: i32,
                                                   _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_event_value", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_event_closest_attr", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                           _p0: i32,
                                                           _l0: i32,
                                                           _p1: i32,
                                                           _l1: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_event_type", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ── Class & Style Manipulation ────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_toggle_class", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                     _p0: i32,
                                                     _l0: i32,
                                                     _p1: i32,
                                                     _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_add_class", |_caller: Caller<'_, WasmState>,
                                                  _p0: i32,
                                                  _l0: i32,
                                                  _p1: i32,
                                                  _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_remove_class", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                     _p0: i32,
                                                     _l0: i32,
                                                     _p1: i32,
                                                     _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_set_style", |_caller: Caller<'_, WasmState>,
                                                  _p0: i32,
                                                  _l0: i32,
                                                  _p1: i32,
                                                  _l1: i32,
                                                  _p2: i32,
                                                  _l2: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_query_set_style", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                        _p0: i32,
                                                        _l0: i32,
                                                        _p1: i32,
                                                        _l1: i32,
                                                        _p2: i32,
                                                        _l2: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_query_set_attr", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                       _p0: i32,
                                                       _l0: i32,
                                                       _p1: i32,
                                                       _l1: i32,
                                                       _p2: i32,
                                                       _l2: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_query_add_class", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                        _p0: i32,
                                                        _l0: i32,
                                                        _p1: i32,
                                                        _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_query_remove_class", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                           _p0: i32,
                                                           _l0: i32,
                                                           _p1: i32,
                                                           _l1: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_filter_by_attr", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                       _p0: i32,
                                                       _l0: i32,
                                                       _p1: i32,
                                                       _l1: i32,
                                                       _p2: i32,
                                                       _l2: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_filter_by_text", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                       _p0: i32,
                                                       _l0: i32,
                                                       _p1: i32,
                                                       _l1: i32,
                                                       _p2: i32,
                                                       _l2: i32,
                                                       _p3: i32,
                                                       _l3: i32|
     -> i32 { 0 });

    // ── CSS Variables ─────────────────────────────────────────────────────────
    register_bridge_fn!(
        linker,
        "_ui_set_css_var",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32, _p1: i32, _l1: i32| {}
    );
    register_bridge_fn!(
        linker,
        "_ui_set_css_var_on",
        |_caller: Caller<'_, WasmState>,
         _p0: i32,
         _l0: i32,
         _p1: i32,
         _l1: i32,
         _p2: i32,
         _l2: i32| {}
    );
    register_bridge_fn!(linker, "_ui_get_css_var", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    _p0: i32,
                                                    _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(
        linker,
        "_ui_apply_css_vars",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32| {}
    );

    // ── Focus Management ──────────────────────────────────────────────────────
    register_bridge_fn!(
        linker,
        "_ui_focus",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32| {}
    );
    register_bridge_fn!(
        linker,
        "_ui_blur",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32| {}
    );
    register_bridge_fn!(linker, "_ui_get_focus", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ── DOM Queries ───────────────────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_get_text", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                 _p0: i32,
                                                 _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_get_attr", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                 _p0: i32,
                                                 _l0: i32,
                                                 _p1: i32,
                                                 _l1: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ── Clipboard ─────────────────────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_clipboard_write", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                        _p0: i32,
                                                        _l0: i32|
     -> i32 { 0 });

    // ── URL / Location ────────────────────────────────────────────────────────
    register_bridge_fn!(linker, "_ui_location_href", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                      _p0: i32,
                                                      _l0: i32|
     -> i32 { 0 });
    register_bridge_fn!(linker, "_ui_location_query", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                       _p0: i32,
                                                       _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_location_path", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_current_path", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ── Compiler-owned build_state keystore (no-op at runtime) ──
    // Registered at compile time by the WasmPluginAdapter; runtime stub exists
    // only so WASM modules referencing them can instantiate.
    register_bridge_fn!(linker, "_build_state_get", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                     _p0: i32,
                                                     _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(
        linker,
        "_build_state_set",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32, _p1: i32, _l1: i32| {}
    );

    // ── Component / slot stubs (browser-only; SSR cannot dispatch component events) ──
    register_bridge_fn!(linker, "_ui_register_component", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                           _p0: i32,
                                                           _l0: i32,
                                                           _p1: i32,
                                                           _l1: i32|
     -> i64 { 0 });
    register_bridge_fn!(linker, "_ui_get_component", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                      _p0: i32,
                                                      _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(linker, "_ui_set_slot", |_caller: Caller<'_, WasmState>,
                                                 _p0: i32,
                                                 _l0: i32,
                                                 _p1: i32,
                                                 _l1: i32|
     -> i64 { 0 });
    register_bridge_fn!(linker, "_ui_get_slot", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                 _p0: i32,
                                                 _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ── Storage (localStorage / sessionStorage) ───────────────────────────────
    register_bridge_fn!(linker, "_storage_local_get", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                       _p0: i32,
                                                       _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(
        linker,
        "_storage_local_set",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32, _p1: i32, _l1: i32| {}
    );
    register_bridge_fn!(
        linker,
        "_storage_local_remove",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32| {}
    );
    register_bridge_fn!(linker, "_storage_local_clear", |_caller: Caller<
        '_,
        WasmState,
    >| {});
    register_bridge_fn!(linker, "_storage_session_get", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                         _p0: i32,
                                                         _l0: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });
    register_bridge_fn!(
        linker,
        "_storage_session_set",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32, _p1: i32, _l1: i32| {}
    );
    register_bridge_fn!(
        linker,
        "_storage_session_remove",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32| {}
    );
    register_bridge_fn!(linker, "_storage_session_clear", |_caller: Caller<
        '_,
        WasmState,
    >| {});

    // ── Keyboard Shortcuts (browser-only, may be emitted by events: DSL) ──────
    register_bridge_fn!(linker, "_ui_shortcut_register", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                          _p0: i32,
                                                          _l0: i32,
                                                          _p1: i32,
                                                          _l1: i32,
                                                          _p2: i32,
                                                          _l2: i32|
     -> i32 { 0 });
    register_bridge_fn!(
        linker,
        "_ui_shortcut_remove",
        |_caller: Caller<'_, WasmState>, _a0: i32| {}
    );
    register_bridge_fn!(linker, "_ui_shortcut_clear", |_caller: Caller<
        '_,
        WasmState,
    >| {});

    // ── Navigation (browser-only, may be emitted by events: DSL) ─────────────
    register_bridge_fn!(
        linker,
        "_ui_navigate",
        |_caller: Caller<'_, WasmState>, _p0: i32, _l0: i32| {}
    );

    Ok(())
}
