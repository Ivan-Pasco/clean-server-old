//! WASM Alignment Tests
//!
//! Validates that the server linker can instantiate actual WASM binaries and
//! that the host function signatures provided by the linker match what compiled
//! WASM modules import.
//!
//! This closes the "three-way disagreement" gap: while the spec compliance test
//! validates registry <-> implementation, these tests validate
//! implementation <-> actual compiled binaries.
//!
//! # Tests
//!
//! 1. `test_examples_compile_and_instantiate_with_cln` — compiles every
//!    examples/**/*.cln and instantiates it against the full server linker.
//! 2. `test_compile_and_instantiate_with_cln` — compiles a minimal Clean
//!    Language source and instantiates the result.
//!
//! Tests 1 and 2 need the `cln` compiler (`~/.cleen/bin/cln`) and fail
//! without it. CI runs them in the e2e job, which installs the compiler, and
//! skips them (`--skip _with_cln`) in the integration job, which does not.
//! 3. `test_linker_provides_all_standard_imports` — a static WAT contract
//!    listing every import the current compiler is known to generate.

mod common;

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::WasmState;
use common::compile_cln;
use std::path::PathBuf;
use std::sync::Arc;
use wasmtime::{Engine, Module, Store};

// ---------------------------------------------------------------------------
// Helper: create a minimal WasmState + Store for instantiation attempts
// ---------------------------------------------------------------------------

fn make_store(engine: &Engine) -> Store<WasmState> {
    let router = Arc::new(Router::new());
    let state = WasmState::new(router);
    Store::new(engine, state)
}

// ---------------------------------------------------------------------------
// Helper: attempt to instantiate a pre-compiled .wasm file.
//
// Returns Ok(()) on success or Err with a detailed diagnostic message.
// ---------------------------------------------------------------------------

fn try_instantiate_wasm_file(engine: &Engine, wasm_path: &std::path::Path) -> Result<(), String> {
    let wasm_bytes = std::fs::read(wasm_path)
        .map_err(|e| format!("Failed to read {}: {}", wasm_path.display(), e))?;

    let module = Module::from_binary(engine, &wasm_bytes).map_err(|e| {
        format!(
            "Failed to parse WASM module at {}: {}",
            wasm_path.display(),
            e
        )
    })?;

    let linker =
        create_linker(engine).map_err(|e| format!("Failed to create server linker: {}", e))?;

    let mut store = make_store(engine);

    linker.instantiate(&mut store, &module).map_err(|e| {
        // Produce a clear message that identifies exactly which import failed.
        // Wasmtime's error message already names the missing/mismatched import.
        format!(
            "Instantiation failed for {}:\n  {}\n\n\
             This means the server linker does not provide (or has the wrong \
             signature for) a host function that the compiled binary imports. \
             Check the function name and signature against the server \
             implementation in src/bridge.rs and host-bridge/src/wasm_linker/.",
            wasm_path.display(),
            e
        )
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Test 1: Compile every example under examples/ and instantiate it.
//
// The examples are the programs users copy from, so each must still compile
// with the current compiler and load on this server.
// ---------------------------------------------------------------------------

#[test]
fn test_examples_compile_and_instantiate_with_cln() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let examples_dir = manifest_dir.join("examples");

    let sources = collect_files(&examples_dir, "cln");
    assert!(
        !sources.is_empty(),
        "no .cln examples found under {}",
        examples_dir.display()
    );

    let engine = Engine::default();
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp directory");
    let mut failures: Vec<String> = Vec::new();

    for (index, source_path) in sources.iter().enumerate() {
        let wasm_path = temp_dir.path().join(format!("example_{index}.wasm"));
        let result = compile_cln(source_path, &wasm_path, &[])
            .and_then(|()| try_instantiate_wasm_file(&engine, &wasm_path));
        if let Err(msg) = result {
            failures.push(format!("{}:\n{}", source_path.display(), msg));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} example(s) failed to compile or instantiate:\n\n{}",
        failures.len(),
        sources.len(),
        failures.join("\n\n---\n\n")
    );
}

/// Recursively collect all files with extension `ext` under `dir`.
fn collect_files(dir: &std::path::Path, ext: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            result.extend(collect_files(&path, ext));
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            result.push(path);
        }
    }
    result.sort();
    result
}

// ---------------------------------------------------------------------------
// Test 2: Compile a minimal Clean Language source then instantiate it.
//
// This is the highest-fidelity test: it uses the actual compiler to produce
// a binary and immediately checks that the server linker can satisfy all of
// its imports. It needs `~/.cleen/bin/cln`; CI runs the `_with_cln` tests in
// the e2e job, which installs the compiler.
// ---------------------------------------------------------------------------

#[test]
fn test_compile_and_instantiate_with_cln() {
    // Changed: one source in the current syntax (`start:` before
    // `functions:`, `_http_route` naming its handler export) replaces the
    // list of old/new syntax candidates; the `plugins: frame.server` block is
    // gone because the calls below are host bridge functions that need no
    // plugin, and CI has no plugins installed.
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/test", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		return _req_body()
"#;

    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp directory");
    let source_path = temp_dir.path().join("alignment_test.cln");
    let wasm_path = temp_dir.path().join("alignment_test.wasm");
    std::fs::write(&source_path, source).expect("Failed to write alignment_test.cln");

    if let Err(msg) = compile_cln(&source_path, &wasm_path, &[]) {
        panic!("test_compile_and_instantiate_with_cln: {msg}");
    }

    let engine = Engine::default();
    if let Err(msg) = try_instantiate_wasm_file(&engine, &wasm_path) {
        panic!(
            "test_compile_and_instantiate_with_cln: The compiled binary \
             could not be instantiated against the server linker.\n\n{}",
            msg
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Static WAT contract — every import the compiler is known to emit.
//
// This is a compile-time contract.  If the server linker ever stops providing
// one of these functions (or changes its signature), this test catches it
// immediately without needing a compiled binary or a running compiler.
//
// Signatures are derived directly from the host-bridge and bridge.rs
// implementations, verified against actual source code.
//
// Convention:
//   - length-prefixed string ptr  => single i32
//   - raw (ptr, len) string pair  => (i32 i32)
//   - integer                     => i64
//   - number/float                => f64
//   - boolean / status / index    => i32
// ---------------------------------------------------------------------------

/// WAT module that imports every function the Clean Language compiler is known
/// to generate.  The module itself does nothing — we only need the linker to
/// be able to satisfy all imports.
const FULL_CONTRACT_WAT: &str = r#"
(module
  ;; -----------------------------------------------------------------------
  ;; IMPORTANT: All imports must appear before any local definitions in the
  ;; WASM text format.  Local definitions (memory, global, func) come last.
  ;;
  ;; This WAT module lists every host function that the server linker
  ;; provides, with the exact signature used in the implementation.
  ;; Verified against:
  ;;   - host-bridge/src/wasm_linker/console.rs
  ;;   - host-bridge/src/wasm_linker/memory.rs
  ;;   - host-bridge/src/wasm_linker/string_ops.rs
  ;;   - host-bridge/src/wasm_linker/math.rs
  ;;   - host-bridge/src/wasm_linker/http_client.rs
  ;;   - host-bridge/src/wasm_linker/file_io.rs
  ;;   - host-bridge/src/wasm_linker/database.rs
  ;;   - src/bridge.rs (Layer 3)
  ;; -----------------------------------------------------------------------

  ;; -----------------------------------------------------------------------
  ;; Layer 2 — Console (module "env")
  ;; print/printl/print_string use raw (ptr: i32, len: i32) pairs.
  ;; print_integer uses i64 per spec.
  ;; -----------------------------------------------------------------------
  (import "env" "print"         (func (param i32 i32)))
  (import "env" "printl"        (func (param i32 i32)))
  (import "env" "print_string"  (func (param i32 i32)))
  (import "env" "print_integer" (func (param i64)))
  (import "env" "print_float"   (func (param f64)))
  (import "env" "print_boolean" (func (param i32)))
  (import "env" "console_log"   (func (param i32 i32)))
  (import "env" "console_error" (func (param i32 i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 2 — Memory runtime (module "memory_runtime")
  ;; -----------------------------------------------------------------------
  (import "memory_runtime" "mem_alloc"      (func (param i32 i32) (result i32)))
  (import "memory_runtime" "mem_retain"     (func (param i32)))
  (import "memory_runtime" "mem_release"    (func (param i32)))
  (import "memory_runtime" "mem_scope_push" (func))
  (import "memory_runtime" "mem_scope_pop"  (func))

  ;; -----------------------------------------------------------------------
  ;; Layer 2 — String operations (module "env")
  ;;
  ;; Single-string functions use a length-prefixed ptr (single i32).
  ;; Two-string functions use two length-prefixed ptrs (two i32 values).
  ;; Three-arg functions use three ptrs.
  ;; -----------------------------------------------------------------------

  ;; concat: (ptr1, ptr2) -> result_ptr
  (import "env" "string_concat"    (func (param i32 i32) (result i32)))
  (import "env" "string.concat"    (func (param i32 i32) (result i32)))

  ;; substring: (str_ptr, start: i32, end: i32) -> result_ptr
  (import "env" "string_substring" (func (param i32 i32 i32) (result i32)))
  (import "env" "string.substring" (func (param i32 i32 i32) (result i32)))

  ;; trim variants: (ptr) -> result_ptr
  (import "env" "string_trim"       (func (param i32) (result i32)))
  (import "env" "string.trim"       (func (param i32) (result i32)))
  (import "env" "string_trim_start" (func (param i32) (result i32)))
  (import "env" "string.trimStart"  (func (param i32) (result i32)))
  (import "env" "string_trim_end"   (func (param i32) (result i32)))
  (import "env" "string.trimEnd"    (func (param i32) (result i32)))

  ;; case: (ptr) -> result_ptr — registered as camelCase (not snake_case)
  (import "env" "string_toUpperCase" (func (param i32) (result i32)))
  (import "env" "string.toUpperCase" (func (param i32) (result i32)))
  (import "env" "string_toLowerCase" (func (param i32) (result i32)))
  (import "env" "string.toLowerCase" (func (param i32) (result i32)))

  ;; replace: (str_ptr, from_ptr, to_ptr) -> result_ptr
  (import "env" "string_replace" (func (param i32 i32 i32) (result i32)))
  (import "env" "string.replace" (func (param i32 i32 i32) (result i32)))

  ;; split: (str_ptr, delim_ptr) -> list_ptr (Clean list<string>: 16-byte header
  ;; + N*4-byte element pointers; behavior verified in string_split_test.rs)
  (import "env" "string_split"   (func (param i32 i32) (result i32)))
  (import "env" "string.split"   (func (param i32 i32) (result i32)))

  ;; index_of: (haystack_ptr, needle_ptr) -> i32
  (import "env" "string_index_of" (func (param i32 i32) (result i32)))

  ;; compare: (a_ptr, b_ptr) -> i32 (-1 / 0 / 1)
  (import "env" "string_compare"  (func (param i32 i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 2 — Type conversions (module "env")
  ;;
  ;; int_to_string takes i32 (NOT i64).
  ;; string_to_int / string_to_float take a single length-prefixed ptr.
  ;; -----------------------------------------------------------------------
  (import "env" "int_to_string"    (func (param i32) (result i32)))
  (import "env" "integer.toString" (func (param i32) (result i32)))
  (import "env" "float_to_string"  (func (param f64) (result i32)))
  (import "env" "number.toString"  (func (param f64) (result i32)))
  (import "env" "bool_to_string"   (func (param i32) (result i32)))
  (import "env" "boolean.toString" (func (param i32) (result i32)))
  (import "env" "string_to_int"    (func (param i32) (result i32)))
  (import "env" "string.toInteger" (func (param i32) (result i32)))
  (import "env" "string_to_float"  (func (param i32) (result f64)))
  (import "env" "string.toNumber"  (func (param i32) (result f64)))
  (import "env" "string_to_bool"   (func (param i32) (result i32)))
  (import "env" "string.toBoolean" (func (param i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 2 — Math (module "env")
  ;; Note: natural log is math_ln (NOT math_log).
  ;; -----------------------------------------------------------------------
  (import "env" "math_pow"    (func (param f64 f64) (result f64)))
  (import "env" "math_sqrt"   (func (param f64) (result f64)))
  (import "env" "math_abs"    (func (param f64) (result f64)))
  (import "env" "math_floor"  (func (param f64) (result f64)))
  (import "env" "math_ceil"   (func (param f64) (result f64)))
  (import "env" "math_round"  (func (param f64) (result f64)))
  (import "env" "math_trunc"  (func (param f64) (result f64)))
  (import "env" "math_sign"   (func (param f64) (result f64)))
  (import "env" "math_sin"    (func (param f64) (result f64)))
  (import "env" "math_cos"    (func (param f64) (result f64)))
  (import "env" "math_tan"    (func (param f64) (result f64)))
  (import "env" "math_asin"   (func (param f64) (result f64)))
  (import "env" "math_acos"   (func (param f64) (result f64)))
  (import "env" "math_atan"   (func (param f64) (result f64)))
  (import "env" "math_atan2"  (func (param f64 f64) (result f64)))
  (import "env" "math_sinh"   (func (param f64) (result f64)))
  (import "env" "math_cosh"   (func (param f64) (result f64)))
  (import "env" "math_tanh"   (func (param f64) (result f64)))
  (import "env" "math_ln"     (func (param f64) (result f64)))
  (import "env" "math_log10"  (func (param f64) (result f64)))
  (import "env" "math_log2"   (func (param f64) (result f64)))
  (import "env" "math_exp"    (func (param f64) (result f64)))
  (import "env" "math_exp2"   (func (param f64) (result f64)))
  (import "env" "math_min"    (func (param f64 f64) (result f64)))
  (import "env" "math_max"    (func (param f64 f64) (result f64)))
  (import "env" "math_pi"     (func (result f64)))
  (import "env" "math_e"      (func (result f64)))
  (import "env" "math_random" (func (result f64)))

  ;; -----------------------------------------------------------------------
  ;; Layer 2 — HTTP client (module "env")
  ;; All use raw (ptr: i32, len: i32) pairs for URL / body strings.
  ;; -----------------------------------------------------------------------
  (import "env" "http_get"       (func (param i32 i32) (result i32)))
  (import "env" "http_post"      (func (param i32 i32 i32 i32) (result i32)))
  (import "env" "http_put"       (func (param i32 i32 i32 i32) (result i32)))
  (import "env" "http_patch"     (func (param i32 i32 i32 i32) (result i32)))
  (import "env" "http_delete"    (func (param i32 i32) (result i32)))
  (import "env" "http_head"      (func (param i32 i32) (result i32)))
  (import "env" "http_options"   (func (param i32 i32) (result i32)))
  (import "env" "http_post_json" (func (param i32 i32 i32 i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 2 — File I/O (module "env")
  ;; file_read takes a mode parameter: (path_ptr, path_len, mode) -> ptr
  ;; -----------------------------------------------------------------------
  (import "env" "file_read"   (func (param i32 i32 i32) (result i32)))
  (import "env" "file_write"  (func (param i32 i32 i32 i32) (result i32)))
  (import "env" "file_exists" (func (param i32 i32) (result i32)))
  (import "env" "file_delete" (func (param i32 i32) (result i32)))
  (import "env" "file_append" (func (param i32 i32 i32 i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 2 — Database (module "env")
  ;; -----------------------------------------------------------------------
  (import "env" "_db_query"    (func (param i32 i32 i32 i32) (result i32)))
  (import "env" "_db_execute"  (func (param i32 i32 i32 i32) (result i32)))
  (import "env" "_db_begin"    (func (result i32)))
  (import "env" "_db_commit"   (func (result i32)))
  (import "env" "_db_rollback" (func (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 3 — HTTP server (module "env", src/bridge.rs)
  ;; String parameters use raw (ptr: i32, len: i32) pairs.
  ;; -----------------------------------------------------------------------

  ;; _http_listen: (port: i32) -> i32
  (import "env" "_http_listen" (func (param i32) (result i32)))

  ;; _http_route: (method_ptr, method_len, path_ptr, path_len, handler_ptr, handler_len) -> i32
  (import "env" "_http_route" (func (param i32 i32 i32 i32 i32 i32) (result i32)))

  ;; _http_route_protected: (method_ptr, method_len, path_ptr, path_len, handler_ptr, handler_len, role_ptr, role_len) -> i32
  (import "env" "_http_route_protected" (func (param i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))

  ;; _http_serve_static: (prefix_ptr, prefix_len, dir_ptr, dir_len) -> i32
  (import "env" "_http_serve_static" (func (param i32 i32 i32 i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 3 — Request context (module "env")
  ;; Key parameters use raw (ptr, len); zero-arg accessors return a ptr.
  ;; -----------------------------------------------------------------------
  (import "env" "_req_param"      (func (param i32 i32) (result i32)))
  (import "env" "_req_query"      (func (param i32 i32) (result i32)))
  (import "env" "_req_body"       (func (result i32)))
  (import "env" "_req_header"     (func (param i32 i32) (result i32)))
  (import "env" "_req_method"     (func (result i32)))
  (import "env" "_req_path"       (func (result i32)))
  (import "env" "_req_cookie"     (func (param i32 i32) (result i32)))
  (import "env" "_req_headers"    (func (result i32)))
  (import "env" "_req_form"       (func (result i32)))
  (import "env" "_req_ip"         (func (result i32)))
  (import "env" "_req_body_field" (func (param i32 i32) (result i32)))
  (import "env" "_req_param_int"  (func (param i32 i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 3 — Response (module "env")
  ;; -----------------------------------------------------------------------

  ;; _http_respond: (status, ct_ptr, ct_len, body_ptr, body_len) -> i32
  (import "env" "_http_respond"   (func (param i32 i32 i32 i32 i32) (result i32)))

  ;; _http_redirect: (status, url_ptr, url_len) -> i32
  (import "env" "_http_redirect"  (func (param i32 i32 i32) (result i32)))

  ;; _http_set_header: (name_ptr, name_len, val_ptr, val_len) -> void (per function-registry.toml)
  ;; _res_set_header:  (name_ptr, name_len, val_ptr, val_len) -> i32 (boolean)
  (import "env" "_http_set_header" (func (param i32 i32 i32 i32)))
  (import "env" "_res_set_header"  (func (param i32 i32 i32 i32) (result i32)))

  ;; _res_redirect: (url_ptr, url_len, status_code) -> i32
  (import "env" "_res_redirect"   (func (param i32 i32 i32) (result i32)))

  ;; _res_status: (code: i32)
  (import "env" "_res_status"     (func (param i32)))

  ;; _res_body: (body_ptr, body_len)
  (import "env" "_res_body"       (func (param i32 i32)))

  ;; _res_json: (json_ptr, json_len)
  (import "env" "_res_json"       (func (param i32 i32)))

  ;; _http_set_cache: (max_age: i32) -> i32
  (import "env" "_http_set_cache" (func (param i32) (result i32)))

  ;; _http_no_cache: () -> i32
  (import "env" "_http_no_cache"  (func (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 3 — JSON helpers (module "env", src/bridge.rs)
  ;; -----------------------------------------------------------------------
  ;; Note: _json_parse is NOT registered by the server — only _json_get,
  ;; _json_encode, _json_decode are available.
  ;; _json_get: (any: i32, path_lp_ptr: i32) -> any (i32)
  ;; Frame.server v2.8.4+ / compiler 0.33.54+ shape — see function-registry.toml.
  (import "env" "_json_get"    (func (param i32 i32) (result i32)))
  (import "env" "_json_encode" (func (param i32 i32) (result i32)))
  (import "env" "_json_decode" (func (param i32 i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 3 — Authentication (module "env")
  ;; -----------------------------------------------------------------------
  (import "env" "_auth_get_session"   (func (result i32)))
  (import "env" "_auth_require_auth"  (func (result i32)))
  (import "env" "_auth_require_role"  (func (param i32 i32) (result i32)))
  (import "env" "_auth_can"           (func (param i32 i32) (result i32)))
  (import "env" "_auth_has_any_role"  (func (param i32 i32) (result i32)))
  (import "env" "_auth_set_session"   (func (param i32 i32) (result i32)))
  (import "env" "_auth_clear_session" (func (result i32)))
  (import "env" "_auth_user_id"       (func (result i32)))
  (import "env" "_auth_user_role"     (func (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 3 — Session management (module "env")
  ;;
  ;; _session_store: session_id, key, value use length-prefixed ptrs (i32
  ;; each), plus two i32 scalar args (ttl, flags).
  ;; _session_exists / _session_set_csrf use raw (ptr, len) pairs.
  ;; _http_set_cookie uses two length-prefixed ptrs (name_ptr, value_ptr).
  ;; -----------------------------------------------------------------------
  (import "env" "_session_store"    (func (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "_session_get"      (func (result i32)))
  (import "env" "_session_delete"   (func (result i32)))
  (import "env" "_session_exists"   (func (param i32 i32) (result i32)))
  (import "env" "_session_set_csrf" (func (param i32 i32) (result i32)))
  (import "env" "_session_get_csrf" (func (result i32)))
  (import "env" "_http_set_cookie"  (func (param i32 i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Layer 3 — Roles (module "env")
  ;; -----------------------------------------------------------------------
  (import "env" "_roles_register"       (func (param i32 i32) (result i32)))
  (import "env" "_role_has_permission"  (func (param i32 i32 i32 i32) (result i32)))
  (import "env" "_role_get_permissions" (func (param i32 i32) (result i32)))

  ;; -----------------------------------------------------------------------
  ;; Local definitions — MUST appear AFTER all imports per WASM spec.
  ;; -----------------------------------------------------------------------
  (memory (export "memory") 1)
  (global $heap_ptr (export "__heap_ptr") (mut i32) (i32.const 1024))

  ;; Minimal bump-allocator malloc stub.  Host functions that write strings
  ;; back into WASM memory use the "memory" export and this global.
  (func (export "malloc") (param i32) (result i32)
    (local $old_ptr i32)
    (local.set $old_ptr (global.get $heap_ptr))
    (global.set $heap_ptr (i32.add (global.get $heap_ptr) (local.get 0)))
    (local.get $old_ptr)
  )
)
"#;

#[test]
fn test_linker_provides_all_standard_imports() {
    let engine = Engine::default();

    // Parse the WAT contract into a WASM module.
    let module = Module::new(&engine, FULL_CONTRACT_WAT).unwrap_or_else(|e| {
        panic!(
            "test_linker_provides_all_standard_imports: WAT contract failed to parse.\n\
             This is a bug in the test itself — fix the WAT syntax.\n\
             Error: {}",
            e
        )
    });

    // Create the full server linker (Layer 2 + Layer 3).
    let linker = create_linker(&engine).unwrap_or_else(|e| {
        panic!(
            "test_linker_provides_all_standard_imports: Failed to create server linker: {}",
            e
        )
    });

    let mut store = make_store(&engine);

    // If the linker cannot instantiate the module, wasmtime will report exactly
    // which import is missing or has the wrong signature.
    linker.instantiate(&mut store, &module).unwrap_or_else(|e| {
        panic!(
            "test_linker_provides_all_standard_imports: CONTRACT FAILURE\n\n\
             The server linker is missing a host function or has a signature \
             mismatch for a function that compiled WASM binaries import.\n\n\
             Wasmtime error (identifies the exact failing import):\n  {}\n\n\
             Fix the implementation in src/bridge.rs or \
             host-bridge/src/wasm_linker/ to match the contract, not the \
             other way around.",
            e
        )
    });

    // Count imports for the success message.
    let import_count = module.imports().count();
    eprintln!(
        "test_linker_provides_all_standard_imports: PASSED — linker satisfied \
         all {} imports in the standard contract.",
        import_count
    );
}

// ---------------------------------------------------------------------------
// Test: mem_alloc argument order honors compiler-emitted (type_id, size).
//
// Regression for RUNTIME-WASMTIME-DIVERGES-FROM-CLN-TEST (fingerprint
// d8c010f7a7b2). Before the fix, the linker bound mem_alloc's parameters as
// (size, align) — but the compiler emits `mem_alloc(type_id, size)`. Every
// allocation therefore had size=0, the bump allocator never advanced, and the
// next allocation returned the same pointer. json.encode of an object literal
// trapped because the three Any-box allocations all overlapped.
//
// This test asserts the second sequential allocation lands at a strictly
// greater pointer than the first (proving size was honored, not coerced to 0).
// ---------------------------------------------------------------------------

#[test]
fn test_mem_alloc_honors_compiler_size_arg() {
    let wat = r#"
        (module
            (import "memory_runtime" "mem_alloc"
                (func $mem_alloc (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (global $heap_ptr (mut i32) (i32.const 4152))
            (global (export "__heap_ptr") (mut i32) (i32.const 4152))
            ;; alloc_twice() calls mem_alloc(0, 12) twice and returns
            ;; (second_ptr - first_ptr). A working host returns >= 12;
            ;; the buggy host (which read size from the first arg, getting 0)
            ;; returns 0 because the bump allocator never advances.
            (func (export "alloc_twice") (result i32)
                (local $a i32)
                (local $b i32)
                (local.set $a (call $mem_alloc (i32.const 0) (i32.const 12)))
                (local.set $b (call $mem_alloc (i32.const 0) (i32.const 12)))
                (i32.sub (local.get $b) (local.get $a))
            )
        )
    "#;

    let engine = Engine::default();
    let module = Module::new(&engine, wat).expect("compile WAT");
    let linker = create_linker(&engine).expect("create linker");
    let mut store = make_store(&engine);

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate module");

    let alloc_twice = instance
        .get_typed_func::<(), i32>(&mut store, "alloc_twice")
        .expect("get alloc_twice export");

    let diff = alloc_twice.call(&mut store, ()).expect("call alloc_twice");

    assert!(
        diff >= 12,
        "mem_alloc argument order regression: two sequential 12-byte allocations \
         returned pointers differing by {} (expected >= 12). The host is reading \
         size from the wrong argument — compiler emits (type_id, size), the host \
         must consume (type_id, size). See RUNTIME-WASMTIME-DIVERGES-FROM-CLN-TEST.",
        diff
    );
}

// ---------------------------------------------------------------------------
// Test: _json_get tolerates a BARE length-prefixed string pointer as any_ptr.
//
// Regression for prompt f2c316ce (db.query response unreadable by json.get in
// clean-server 1.9.99). Root cause: _db_query returns a bare lp-string ptr
// ([4B len][bytes]) but _json_get expects a 12-byte boxed Any struct
// ([tag][value1][value2]). If the compiler forwards the bare pointer without
// boxing, _json_get was reading the first 4 bytes of the JSON as `tag` and
// the next 4 as `value1` — both garbage, returning empty for every query.
//
// This test writes a JSON envelope directly into WASM memory as a bare
// lp-string (mimicking what _db_query hands back), calls _json_get(str_ptr,
// path_ptr), then asserts the extracted result is non-empty and matches the
// expected value. Without the tolerant-fallback fix, this test fails because
// _json_get returns "".
// ---------------------------------------------------------------------------

#[test]
fn test_json_get_tolerates_bare_lp_string_source() {
    // WAT module:
    //  - imports _json_get, malloc
    //  - exports memory and __heap_ptr
    //  - build_and_call(): writes a JSON lp-string + path lp-string, calls
    //    _json_get with the JSON's bare lp-string ptr as any_ptr, then reads
    //    back the boxed-Any result. Returns the length of the extracted
    //    string. 0 means empty (bug still present), >0 means the fallback
    //    worked.
    //  - result_first_byte(): re-runs the same call and returns the first
    //    byte of the extracted string so the test can verify contents.
    let wat = r#"
(module
  (import "env" "_json_get" (func $json_get (param i32 i32) (result i32)))
  (memory (export "memory") 2)
  (global (export "__heap_ptr") (mut i32) (i32.const 4096))

  ;; Minimal bump malloc. Aligns to 4 bytes. Returns 0 on OOM (never here).
  (func (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get 0))
    (global.set 0
      (i32.and
        (i32.add
          (i32.add (global.get 0) (local.get $size))
          (i32.const 3))
        (i32.const -4)))
    (local.get $ptr)
  )

  ;; Write a length-prefixed string at the given ptr. Length is `len` bytes.
  ;; Data is copied byte-by-byte from `src`.
  (func $write_lp (param $ptr i32) (param $src i32) (param $len i32)
    (local $i i32)
    (i32.store (local.get $ptr) (local.get $len))
    (local.set $i (i32.const 0))
    (block $done
      (loop $copy
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (i32.store8
          (i32.add (i32.add (local.get $ptr) (i32.const 4)) (local.get $i))
          (i32.load8_u (i32.add (local.get $src) (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy)))
  )

  ;; JSON payload literal at address 1024:
  ;;   {"ok":true,"data":{"rows":[{"greeting":"hello"}],"count":1}}
  ;; (60 bytes)
  (data (i32.const 1024)
    "{\22ok\22:true,\22data\22:{\22rows\22:[{\22greeting\22:\22hello\22}],\22count\22:1}}")
  ;; Path literal at address 2048: "data.rows.0.greeting" (20 bytes)
  (data (i32.const 2048) "data.rows.0.greeting")

  ;; Build the lp-string envelope for the JSON at 3072, then the path
  ;; lp-string at 3200, call _json_get, and return the length of the
  ;; extracted string from the returned boxed Any.
  ;;
  ;; Returns: extracted string length (0 = empty, bug still present)
  (func (export "call_json_get_with_bare_lp") (result i32)
    (local $json_lp i32)
    (local $path_lp i32)
    (local $box_ptr i32)
    (local $val1 i32)
    (call $write_lp (i32.const 3072) (i32.const 1024) (i32.const 60))
    (local.set $json_lp (i32.const 3072))
    (call $write_lp (i32.const 3200) (i32.const 2048) (i32.const 20))
    (local.set $path_lp (i32.const 3200))
    ;; Call _json_get with the bare lp-string ptr as any_ptr.
    (local.set $box_ptr
      (call $json_get (local.get $json_lp) (local.get $path_lp)))
    ;; Read value1 from the returned box (offset +4).
    (local.set $val1
      (i32.load (i32.add (local.get $box_ptr) (i32.const 4))))
    ;; Read the length prefix of value1's lp-string.
    (i32.load (local.get $val1))
  )

  ;; Same call, but return the first byte of the extracted string. For
  ;; "hello" this is 'h' = 104. Lets the test check content, not just length.
  (func (export "call_json_get_first_byte") (result i32)
    (local $json_lp i32)
    (local $path_lp i32)
    (local $box_ptr i32)
    (local $val1 i32)
    (call $write_lp (i32.const 3072) (i32.const 1024) (i32.const 60))
    (local.set $json_lp (i32.const 3072))
    (call $write_lp (i32.const 3200) (i32.const 2048) (i32.const 20))
    (local.set $path_lp (i32.const 3200))
    (local.set $box_ptr
      (call $json_get (local.get $json_lp) (local.get $path_lp)))
    (local.set $val1
      (i32.load (i32.add (local.get $box_ptr) (i32.const 4))))
    ;; First byte of the payload = byte at value1 + 4 (past length prefix).
    (i32.load8_u (i32.add (local.get $val1) (i32.const 4)))
  )
)
"#;

    let engine = Engine::default();
    let module = Module::new(&engine, wat).expect("compile WAT");
    let linker = create_linker(&engine).expect("create linker");
    let mut store = make_store(&engine);
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate module");

    let call_len = instance
        .get_typed_func::<(), i32>(&mut store, "call_json_get_with_bare_lp")
        .expect("get call_json_get_with_bare_lp export");
    let extracted_len = call_len.call(&mut store, ()).expect("call bare-lp path");

    assert_eq!(
        extracted_len, 5,
        "_json_get did not extract 'hello' (5 bytes) when given a bare lp-string \
         pointer as any_ptr. Got extracted length = {}. This is the wire-drift bug \
         reported in prompt f2c316ce: _db_query returns a bare lp-string, and if \
         the compiler forwards it to _json_get without boxing, _json_get must \
         fall back to reading any_ptr as a bare lp-string. See src/bridge.rs \
         _json_get comment.",
        extracted_len
    );

    let call_byte = instance
        .get_typed_func::<(), i32>(&mut store, "call_json_get_first_byte")
        .expect("get call_json_get_first_byte export");
    let first_byte = call_byte.call(&mut store, ()).expect("call first-byte");

    assert_eq!(
        first_byte, b'h' as i32,
        "_json_get returned {} as first byte, expected 'h' (104). The fallback \
         resolved to a value but not the expected 'hello'.",
        first_byte
    );
}

// ---------------------------------------------------------------------------
// Test: _json_get walks an already-decoded boxed-Any tree.
//
// The compiler lowers every `json.get(data, path)` to `_json_get`
// (stdlib-reference §8), including `json.get(json.decode(text), path)`
// where the source is a JSON-tree object (tag 6, BOXED_ANY_ABI §3.3)
// rather than a string. For such a source `_json_get` returns the leaf as
// a typed boxed-Any (tag 1 for an integer, tag 4 for a string, ...) and a
// null-tag box when the path does not resolve (runtime-abi/v1.toml).
// A string source keeps its established result — the leaf as text, ""
// on a miss — which binaries already deployed depend on.
// ---------------------------------------------------------------------------

const JSON_GET_TREE_WAT: &str = r#"
(module
  (import "env" "_json_decode_v2" (func $decode (param i32 i32) (result i32)))
  (import "env" "_json_get" (func $json_get (param i32 i32) (result i32)))
  (memory (export "memory") 2)
  (global $heap (mut i32) (i32.const 8192))
  (export "__heap_ptr" (global $heap))

  (func $malloc (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap
      (i32.and (i32.add (i32.add (global.get $heap) (local.get $size)) (i32.const 7))
               (i32.const -8)))
    (local.get $ptr))

  ;; {"user":{"age":42,"name":"Ann","nick":null},"tags":["a","b"]} (61 bytes)
  (data (i32.const 1024)
    "{\22user\22:{\22age\22:42,\22name\22:\22Ann\22,\22nick\22:null},\22tags\22:[\22a\22,\22b\22]}")
  ;; LP paths: [len][bytes]
  (data (i32.const 2048) "\08\00\00\00user.age")
  (data (i32.const 2080) "\09\00\00\00user.name")
  (data (i32.const 2112) "\06\00\00\00tags.1")
  (data (i32.const 2144) "\0c\00\00\00user.missing")
  (data (i32.const 2176) "\09\00\00\00user.nick")
  ;; LP string holding the same JSON text, for the string-source case.
  (data (i32.const 3072) "\3d\00\00\00")
  (data (i32.const 3076)
    "{\22user\22:{\22age\22:42,\22name\22:\22Ann\22,\22nick\22:null},\22tags\22:[\22a\22,\22b\22]}")

  ;; _json_get on the decoded tree; returns the result box.
  (func (export "get_from_tree") (param $path i32) (result i32)
    (call $json_get (call $decode (i32.const 1024) (i32.const 61)) (local.get $path)))

  ;; _json_get on a String-tagged box holding the JSON text.
  (func (export "get_from_text") (param $path i32) (result i32)
    (local $box i32)
    (local.set $box (call $malloc (i32.const 12)))
    (i32.store (local.get $box) (i32.const 4))
    (i32.store offset=4 (local.get $box) (i32.const 3072))
    (i32.store offset=8 (local.get $box) (i32.const 0))
    (call $json_get (local.get $box) (local.get $path)))
)
"#;

struct JsonGetHarness {
    store: Store<WasmState>,
    instance: wasmtime::Instance,
}

impl JsonGetHarness {
    fn new() -> Self {
        let engine = Engine::default();
        let module = Module::new(&engine, JSON_GET_TREE_WAT).expect("compile WAT");
        let linker = create_linker(&engine).expect("create linker");
        let mut store = make_store(&engine);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate module");
        Self { store, instance }
    }

    fn call(&mut self, export: &str, path: i32) -> i32 {
        self.instance
            .get_typed_func::<i32, i32>(&mut self.store, export)
            .expect("export")
            .call(&mut self.store, path)
            .expect("call _json_get")
    }

    fn memory(&mut self) -> wasmtime::Memory {
        self.instance
            .get_memory(&mut self.store, "memory")
            .expect("memory")
    }

    fn i32_at(&mut self, ptr: i32) -> i32 {
        let memory = self.memory();
        let mut bytes = [0u8; 4];
        memory
            .read(&self.store, ptr as usize, &mut bytes)
            .expect("read i32");
        i32::from_le_bytes(bytes)
    }

    fn i64_at(&mut self, ptr: i32) -> i64 {
        let memory = self.memory();
        let mut bytes = [0u8; 8];
        memory
            .read(&self.store, ptr as usize, &mut bytes)
            .expect("read i64");
        i64::from_le_bytes(bytes)
    }

    fn lp_string_at(&mut self, ptr: i32) -> String {
        let len = self.i32_at(ptr) as usize;
        let memory = self.memory();
        let mut bytes = vec![0u8; len];
        memory
            .read(&self.store, ptr as usize + 4, &mut bytes)
            .expect("read string");
        String::from_utf8(bytes).expect("utf-8")
    }
}

#[test]
fn test_json_get_on_decoded_tree_returns_typed_leaf() {
    let mut h = JsonGetHarness::new();

    let age = h.call("get_from_tree", 2048);
    assert_eq!(h.i32_at(age), 1, "user.age must be an integer box (tag 1)");
    assert_eq!(h.i64_at(age + 4), 42);

    let name = h.call("get_from_tree", 2080);
    assert_eq!(h.i32_at(name), 4, "user.name must be a string box (tag 4)");
    let name_ptr = h.i32_at(name + 4);
    assert_eq!(h.lp_string_at(name_ptr), "Ann");

    let tag = h.call("get_from_tree", 2112);
    assert_eq!(h.i32_at(tag), 4, "tags.1 must be a string box (tag 4)");
    let tag_ptr = h.i32_at(tag + 4);
    assert_eq!(h.lp_string_at(tag_ptr), "b");
}

#[test]
fn test_json_get_on_decoded_tree_miss_is_null_tag() {
    let mut h = JsonGetHarness::new();
    for path in [2144, 2176] {
        let result = h.call("get_from_tree", path);
        assert_ne!(result, 0, "_json_get must return a box, not a null pointer");
        assert_eq!(
            h.i32_at(result),
            0,
            "a missing path or a JSON null must be a null-tag box (tag 0)"
        );
    }
}

#[test]
fn test_json_get_on_text_keeps_text_leaf() {
    let mut h = JsonGetHarness::new();

    let age = h.call("get_from_text", 2048);
    assert_eq!(h.i32_at(age), 4, "a string source yields the leaf as text");
    let age_ptr = h.i32_at(age + 4);
    assert_eq!(h.lp_string_at(age_ptr), "42");

    let missing = h.call("get_from_text", 2144);
    assert_eq!(h.i32_at(missing), 4);
    let missing_ptr = h.i32_at(missing + 4);
    assert_eq!(
        h.lp_string_at(missing_ptr),
        "",
        "a miss on a string source is \"\""
    );
}
