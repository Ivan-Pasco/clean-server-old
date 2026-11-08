#!/usr/bin/env python3
"""Generate Rust no-op stub registrations for browser-only Layer 2
bridge functions, so that browser-compiled WASM modules can still link
against the server runtime.

Pulls signatures directly from function-registry.toml. Re-run when new
browser-only Layer 2 functions are added.

Usage:
    python3 tools/gen_browser_stubs.py > src/bridge_browser_stubs.rs
    cargo fmt

The list of names to stub is intentionally hard-coded here so that:
1. The script is deterministic (does not silently expand its scope).
2. New stubs are an explicit code change rather than the script picking
   them up automatically — the developer reviews each addition.

When the function-registry adds a new browser-only Layer 2 function whose
canonical name is not yet in the server linker, append it to MISSING and
re-run the script.
"""
import sys
import tomllib
from pathlib import Path

# <repo>/tools/gen_browser_stubs.py -> <repo>/../foundation/spec/platform
REGISTRY = (
    Path(__file__).resolve().parents[1].parent
    / "foundation/spec/platform/function-registry.toml"
)

# Categories whose functions are plugin bridges (declared in a plugin's
# [bridge] section: frame.client api/feed/live, frame.ui ui). The compiler
# emits plugin bridges with the PluginBridge convention, where "integer" is a
# WASM i32 — the server stubs must use the same width or the module fails to
# instantiate with "incompatible import type". Every other category is a
# HostBridge function, where "integer" is an i64.
PLUGIN_BRIDGE_CATEGORIES = {"api", "feed", "live", "ui"}

MISSING = sorted([
    "_alloc_string",
    "_api_auth","_api_body","_api_clearAuth","_api_delete","_api_get","_api_header","_api_json",
    "_api_ok","_api_patch","_api_post","_api_put","_api_responseHeader","_api_status","_api_submit","_api_timeout",
    "_assert","_console_info",
    "_feed_close","_feed_connId","_feed_data","_feed_eventType","_feed_lastId","_feed_on","_feed_open",
    "_live_close","_live_closeCode","_live_closeReason","_live_connId","_live_error","_live_message",
    "_live_open","_live_send","_live_state",
    "_log_debug","_log_error","_log_info","_log_warn",
    "_memory_copy","_memory_fill",
    "_panic",
    "_parse_float","_parse_int",
    "_time_now_seconds","_time_performance_now",
    "_ui_clipboard_read_cb","_ui_clipboard_write_cb","_ui_download_text","_ui_download_url",
    "_ui_fetch_cb","_ui_focus_trap","_ui_focus_trap_release",
    "_ui_history_back","_ui_history_forward","_ui_history_push","_ui_history_replace",
    "_ui_intersect_observe","_ui_intersect_unobserve","_ui_observe_visible",
    "_ui_resize_observe","_ui_resize_unobserve",
    "_ui_set_timeout","_ui_toast","_ui_toast_dismiss","_ui_toast_dismiss_all",
])

def integer_width(category):
    return "i32" if category in PLUGIN_BRIDGE_CATEGORIES else "i64"

def expand_param(t, category):
    return {"string": ["i32","i32"], "integer": [integer_width(category)], "number": ["f64"],
            "boolean": ["i32"], "i32": ["i32"], "i64": ["i64"]}[t]

def expand_return(t, category):
    return {"void": None, "ptr": "i32", "string": "i32", "i32": "i32",
            "i64": "i64", "boolean": "i32", "integer": integer_width(category),
            "number": "f64"}.get(t)

def main():
    reg = tomllib.loads(REGISTRY.read_text())
    by_name = {fn["name"]: fn for fn in reg["functions"]}
    missing_fns = [by_name[n] for n in MISSING]
    groups = {}
    for fn in missing_fns:
        groups.setdefault(fn["category"], []).append(fn)

    out = sys.stdout
    out.write("""//! Browser-runtime bridge function stubs for the server.
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

pub fn register_browser_stubs(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {""")
    for cat in sorted(groups):
        out.write(f"\n\n    // ===== category: {cat} ({len(groups[cat])} fns) =====\n")
        for fn in groups[cat]:
            name = fn["name"]
            wasm_params = [w for p in fn["params"] for w in expand_param(p, cat)]
            wasm_ret = expand_return(fn["returns"], cat)
            ret = fn["returns"]
            needs_mut = wasm_ret == "i32" and ret in ("string", "ptr")
            caller_arg = "mut caller: Caller<'_, WasmState>" if needs_mut else "_caller: Caller<'_, WasmState>"
            args = [caller_arg] + [f"_a{i}: {w}" for i, w in enumerate(wasm_params)]
            if wasm_ret is None:
                ret_str = ""
                body = "{}"
            elif wasm_ret == "i32" and ret in ("string", "ptr"):
                ret_str = " -> i32"
                body = '{ write_string_to_caller(&mut caller, "") }'
            elif wasm_ret in ("i32", "i64"):
                ret_str = f" -> {wasm_ret}"
                body = "{ 0 }"
            elif wasm_ret == "f64":
                ret_str = " -> f64"
                body = "{ 0.0 }"
            else:
                raise SystemExit(f"unhandled return: {wasm_ret} for {name}")
            args_str = ", ".join(args)
            out.write(f'    stub!(linker, "{name}", |{args_str}|{ret_str} {body});\n')

    out.write("\n    Ok(())\n}\n")

if __name__ == "__main__":
    main()
