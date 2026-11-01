//! Host Bridge WASM Imports for Clean Server
//!
//! This module provides server-specific host functions that extend host-bridge:
//!
//! ## Shared Functions (from host-bridge)
//! - Console I/O (print, input)
//! - Math functions (sin, cos, tan, pow, etc.)
//! - String operations (concat, substring, etc.)
//! - Memory runtime (mem_alloc, etc.)
//! - Database (_db_query, _db_execute)
//! - File I/O (file_read, file_write)
//! - HTTP client (http_get, http_post)
//! - Crypto (password hashing)
//!
//! ## Server-Specific Functions (defined here)
//! - HTTP server (_http_listen, _http_route, _http_route_protected, _http_serve_static)
//! - Request context (_req_param, _req_query, _req_body, _req_header, _req_method, _req_path, _req_cookie)
//! - Response manipulation (_res_set_header, _res_redirect)
//! - Session management (_session_store, _session_get, _session_delete, _session_exists, _session_set_csrf, _session_get_csrf, _http_set_cookie)
//! - Session auth (_auth_get_session, _auth_require_auth, _auth_require_role, _auth_can, _auth_has_any_role)
//! - Roles (_roles_register, _role_has_permission, _role_get_permissions)
//! - UI templates (_ui_load_layout, _ui_load_page, _ui_render_page, _ui_inject_head_link, _ui_register_component_html)

use crate::error::{RuntimeError, RuntimeResult};
use crate::router::HttpMethod;
use crate::session::parse_cookies;
use crate::wasm::{
    IslandEntry, McpBridgeState, McpPendingRequest, McpTransport, TestResponse, WasmState,
};
use host_bridge::{read_raw_string, read_string_from_caller, write_string_to_caller};
use tracing::{debug, error, info, warn};
use wasmtime::{Caller, Engine, Linker};

/// Register a Layer 3 bridge function under its canonical `_namespace_fn` name
/// and automatically under its `namespace.fn` dot-notation alias.
///
/// Use this macro for all new Layer 3 bridge functions instead of calling
/// `.func_wrap(...).map_err(...)` directly.  It prevents silently omitting the
/// dot alias that the compiler (>= 0.30.120) also emits.
///
/// The macro calls `func_wrap` once (consuming the closure), then derives the
/// alias via `linker.alias()` — no second closure needed.
///
/// See `foundation/spec/platform/HOST_BRIDGE.md § Dual Naming`.
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

/// Create a linker with all host functions
///
/// This registers:
/// 1. All shared functions from host-bridge (console, math, string, db, file, http client, crypto)
/// 2. Server-specific functions (HTTP routing, request context)
pub fn create_linker(engine: &Engine) -> RuntimeResult<Linker<WasmState>> {
    let mut linker = Linker::new(engine);

    // Register all shared functions from host-bridge
    host_bridge::register_all_functions(&mut linker).map_err(|e| {
        RuntimeError::wasm(format!("Failed to register host-bridge functions: {}", e))
    })?;

    // Register server-specific functions
    register_http_server_functions(&mut linker)?;
    register_server_config_functions(&mut linker)?;
    register_request_context_functions(&mut linker)?;
    register_session_management_functions(&mut linker)?;
    register_session_auth_functions(&mut linker)?;
    register_roles_functions(&mut linker)?;
    register_response_functions(&mut linker)?;
    register_json_functions(&mut linker)?;
    register_islands_functions(&mut linker)?;
    register_ui_functions(&mut linker)?;
    register_async_functions(&mut linker)?;
    register_dev_capture_functions(&mut linker)?;
    register_mcp_functions(&mut linker)?;
    register_test_functions(&mut linker)?;
    register_sse_functions(&mut linker)?;
    register_websocket_functions(&mut linker)?;
    register_browser_stub_functions(&mut linker)?;
    register_email_functions(&mut linker)?;
    register_jobs_functions(&mut linker)?;
    register_locale_functions(&mut linker)?;
    crate::bridge_canvas_stubs::register_canvas_stubs(&mut linker)?;
    crate::bridge_ui_stubs::register_ui_stubs(&mut linker)?;
    crate::bridge_browser_stubs::register_browser_stubs(&mut linker)?;

    // Register dot-notation aliases (compiler >= 0.30.120 emits both forms).
    // See foundation/spec/platform/HOST_BRIDGE.md § Dual Naming.
    register_dot_aliases(&mut linker)?;

    Ok(linker)
}

/// Check whether the calling WASM module has permission to invoke the named
/// bridge function.
///
/// Returns `true` when the call is permitted. Returns `false` and emits a
/// `WARN`-level log entry when it is denied. Bridge function closures should
/// early-return their zero/error sentinel when this returns `false`.
#[inline]
fn check_bridge_permission(caller: &Caller<'_, WasmState>, func_name: &str) -> bool {
    caller.data().permission_gate.check(func_name)
}

/// Resolve the active session id for the current request: auth context first,
/// then the `session` / `sid` cookie. Used by every session/auth bridge that
/// implicitly addresses the caller's session.
fn current_session_id(state: &WasmState) -> Option<String> {
    state
        .auth_context
        .as_ref()
        .and_then(|ctx| ctx.session_id.clone())
        .or_else(|| {
            state.request_context.as_ref().and_then(|ctx| {
                ctx.headers
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == "cookie")
                    .and_then(|(_, cookie_header)| {
                        let cookies = parse_cookies(cookie_header);
                        cookies
                            .get("session")
                            .cloned()
                            .or_else(|| cookies.get("sid").cloned())
                    })
            })
        })
}

/// Format a JSON error body in the canonical `{ok:false, err:{code, message}}` shape.
fn err_body(code: &str, message: &str) -> String {
    serde_json::json!({
        "ok": false,
        "err": {"code": code, "message": message}
    })
    .to_string()
}

/// Register HTTP server functions (_http_listen, _http_route, _http_route_protected, _http_serve_static)
fn register_http_server_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _http_listen - Start listening on a port
    linker
        .func_wrap(
            "env",
            "_http_listen",
            |mut caller: Caller<'_, WasmState>, port: i32| -> i32 {
                let state = caller.data_mut();
                state.port = port as u16;
                info!("HTTP server configured to listen on port {}", port);
                0 // Return 0 for success
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_listen: {}", e)))?;

    // _http_route - Register a route handler
    // Signature: (method_ptr, method_len, path_ptr, path_len, handler_ptr, handler_len) -> i32
    // Strings use raw ptr+len pairs; handler is the WASM export name (e.g. "__route_handler_0")
    linker
        .func_wrap(
            "env",
            "_http_route",
            |mut caller: Caller<'_, WasmState>,
             method_ptr: i32,
             method_len: i32,
             path_ptr: i32,
             path_len: i32,
             handler_ptr: i32,
             handler_len: i32|
             -> i32 {
                let method_str = read_raw_string(&mut caller, method_ptr, method_len)
                    .unwrap_or_else(|| "GET".to_string());
                let path = read_raw_string(&mut caller, path_ptr, path_len)
                    .unwrap_or_else(|| "/".to_string());
                let handler_name = read_raw_string(&mut caller, handler_ptr, handler_len)
                    .unwrap_or_else(|| "__route_handler_0".to_string());

                debug!(
                    "_http_route: method={}, path={}, handler={}",
                    method_str, path, handler_name
                );

                let method = match HttpMethod::parse(&method_str) {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Invalid HTTP method '{}': {}", method_str, e);
                        return -1; // Error
                    }
                };

                let state = caller.data();
                let router = state.router.clone();
                // Not protected, no required role
                if let Err(e) =
                    router.register(method, path.clone(), handler_name, false, None, false)
                {
                    error!("Failed to register route {} {}: {}", method_str, path, e);
                    return -1; // Error
                }
                0 // Success
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_route: {}", e)))?;

    // _http_redirect_route - Register a static redirect route (no WASM handler required)
    // Signature: (method_ptr, method_len, from_ptr, from_len, to_ptr, to_len, status) -> i32
    // The server returns the redirect immediately when the route is matched.
    register_bridge_fn!(linker, "_http_redirect_route", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                         method_ptr: i32,
                                                         method_len: i32,
                                                         from_ptr: i32,
                                                         from_len: i32,
                                                         to_ptr: i32,
                                                         to_len: i32,
                                                         status: i32|
     -> i32 {
        let method_str = read_raw_string(&mut caller, method_ptr, method_len)
            .unwrap_or_else(|| "GET".to_string());
        let from_path =
            read_raw_string(&mut caller, from_ptr, from_len).unwrap_or_else(|| "/".to_string());
        let to_path =
            read_raw_string(&mut caller, to_ptr, to_len).unwrap_or_else(|| "/".to_string());
        let status_code = status as u16;

        debug!(
            "_http_redirect_route: {} {} -> {} ({})",
            method_str, from_path, to_path, status_code
        );

        let method = match HttpMethod::parse(&method_str) {
            Ok(m) => m,
            Err(e) => {
                error!("Invalid HTTP method '{}': {}", method_str, e);
                return -1;
            }
        };

        let router = caller.data().router.clone();
        if let Err(e) = router.register_redirect(method, from_path.clone(), to_path, status_code) {
            error!("Failed to register redirect route {}: {}", from_path, e);
            return -1;
        }
        0
    });

    // _http_route_protected - Register a protected route requiring authentication
    // Signature: (method_ptr, method_len, path_ptr, path_len, handler_ptr, handler_len, role_ptr, role_len) -> i32
    // Strings use raw ptr+len pairs; handler is the WASM export name (e.g. "__route_handler_0")
    linker
        .func_wrap(
            "env",
            "_http_route_protected",
            |mut caller: Caller<'_, WasmState>,
             method_ptr: i32,
             method_len: i32,
             path_ptr: i32,
             path_len: i32,
             handler_ptr: i32,
             handler_len: i32,
             role_ptr: i32,
             role_len: i32|
             -> i32 {
                let method_str = read_raw_string(&mut caller, method_ptr, method_len)
                    .unwrap_or_else(|| "GET".to_string());
                let path = read_raw_string(&mut caller, path_ptr, path_len)
                    .unwrap_or_else(|| "/".to_string());
                let handler_name = read_raw_string(&mut caller, handler_ptr, handler_len)
                    .unwrap_or_else(|| "__route_handler_0".to_string());
                let required_role =
                    read_raw_string(&mut caller, role_ptr, role_len).filter(|s| !s.is_empty());

                debug!(
                    "_http_route_protected: method={}, path={}, handler={}, role={:?}",
                    method_str, path, handler_name, required_role
                );

                let method = match HttpMethod::parse(&method_str) {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Invalid HTTP method '{}': {}", method_str, e);
                        return -1; // Error
                    }
                };

                let state = caller.data();
                let router = state.router.clone();
                // Protected route with optional role requirement
                if let Err(e) = router.register(
                    method,
                    path.clone(),
                    handler_name,
                    true,
                    required_role,
                    false,
                ) {
                    error!(
                        "Failed to register protected route {} {}: {}",
                        method_str, path, e
                    );
                    return -1; // Error
                }
                0 // Success
            },
        )
        .map_err(|e| {
            RuntimeError::wasm(format!("Failed to define _http_route_protected: {}", e))
        })?;

    // _http_serve_static - Mount filesystem directory as static file server
    linker
        .func_wrap(
            "env",
            "_http_serve_static",
            |mut caller: Caller<'_, WasmState>,
             prefix_ptr: i32,
             prefix_len: i32,
             dir_ptr: i32,
             dir_len: i32|
             -> i32 {
                let prefix = match read_raw_string(&mut caller, prefix_ptr, prefix_len) {
                    Some(s) => s,
                    None => {
                        error!("_http_serve_static: Failed to read prefix");
                        return 0;
                    }
                };
                let dir = match read_raw_string(&mut caller, dir_ptr, dir_len) {
                    Some(s) => s,
                    None => {
                        error!("_http_serve_static: Failed to read dir");
                        return 0;
                    }
                };

                debug!("_http_serve_static: prefix={}, dir={}", prefix, dir);

                let static_dirs = caller.data().static_dirs.clone();
                match static_dirs.write() {
                    Ok(mut dirs) => {
                        dirs.push((prefix, dir));
                        1 // success
                    }
                    Err(e) => {
                        error!("_http_serve_static: Failed to acquire lock: {}", e);
                        0
                    }
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_serve_static: {}", e)))?;

    // _http_sse_route - Register a STREAM (SSE) route handler
    // Signature: (method_ptr, method_len, path_ptr, path_len, handler_ptr, handler_len) -> i32
    // Always registers as GET; the method param is accepted for API symmetry with _http_route.
    register_bridge_fn!(linker, "_http_sse_route", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    _method_ptr: i32,
                                                    _method_len: i32,
                                                    path_ptr: i32,
                                                    path_len: i32,
                                                    handler_ptr: i32,
                                                    handler_len: i32|
     -> i32 {
        let path = match read_raw_string(&mut caller, path_ptr, path_len) {
            Some(s) => s,
            None => {
                error!("_http_sse_route: Failed to read path");
                return -1;
            }
        };
        let handler_name = match read_raw_string(&mut caller, handler_ptr, handler_len) {
            Some(s) => s,
            None => {
                error!("_http_sse_route: Failed to read handler name");
                return -1;
            }
        };

        info!("_http_sse_route: path={}, handler={}", path, handler_name);

        let router = caller.data().router.clone();
        if let Err(e) = router.register(
            HttpMethod::GET,
            path.clone(),
            handler_name,
            false,
            None,
            true,
        ) {
            error!(
                "_http_sse_route: Failed to register SSE route {}: {}",
                path, e
            );
            return -1;
        }
        0
    });

    Ok(())
}

/// Register `server:` block runtime-config bridges:
/// `_http_listen_on`, `_cors_configure`, `_rate_limit_configure`,
/// `_http_set_global_error_handler`. These write to the shared
/// `SharedRuntimeConfig` that the server reads after WASM init to build the
/// axum router.
fn register_server_config_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    use crate::runtime_config::{CorsConfig, RateLimitConfig, RateLimitStrategy, split_csv};

    // _http_listen_on - Configure host AND port; supersedes _http_listen for
    // modules declaring `server: host: ...`. Returns 0 on success, -1 if the
    // host string can't be read or port is out of range.
    register_bridge_fn!(linker, "_http_listen_on", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    host_ptr: i32,
                                                    host_len: i32,
                                                    port: i32|
     -> i32 {
        let host = match read_raw_string(&mut caller, host_ptr, host_len) {
            Some(s) => s,
            None => {
                error!("_http_listen_on: failed to read host string");
                return -1;
            }
        };
        if !(1..=65535).contains(&port) {
            error!("_http_listen_on: port {} out of range", port);
            return -1;
        }
        let state = caller.data();
        {
            let mut cfg = state.runtime_config.write();
            if !host.trim().is_empty() {
                cfg.listen_host = Some(host.clone());
            }
            cfg.listen_port = Some(port as u16);
        }
        // Keep the legacy per-instance field in sync for any caller that
        // still reads it.
        caller.data_mut().port = port as u16;
        info!("_http_listen_on: configured to listen on {}:{}", host, port);
        0
    });

    // _cors_configure - Install real CORS settings honoring the `server: cors:`
    // block. All four list parameters are comma-separated UTF-8 strings; pass
    // "*" or empty to allow any. Returns 0 on success, -1 on parameter error.
    register_bridge_fn!(linker, "_cors_configure", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    origins_ptr: i32,
                                                    origins_len: i32,
                                                    methods_ptr: i32,
                                                    methods_len: i32,
                                                    headers_ptr: i32,
                                                    headers_len: i32,
                                                    max_age_secs: i32,
                                                    allow_credentials: i32|
     -> i32 {
        let origins = read_raw_string(&mut caller, origins_ptr, origins_len).unwrap_or_default();
        let methods = read_raw_string(&mut caller, methods_ptr, methods_len).unwrap_or_default();
        let headers = read_raw_string(&mut caller, headers_ptr, headers_len).unwrap_or_default();
        if max_age_secs < 0 {
            error!("_cors_configure: max_age_secs cannot be negative");
            return -1;
        }
        let cfg = CorsConfig {
            allowed_origins: split_csv(&origins),
            allowed_methods: split_csv(&methods),
            allowed_headers: split_csv(&headers),
            max_age_secs: max_age_secs as u32,
            allow_credentials: allow_credentials != 0,
        };
        info!(
            "_cors_configure: origins={:?} methods={:?} headers={:?} max_age={}s credentials={}",
            cfg.allowed_origins,
            cfg.allowed_methods,
            cfg.allowed_headers,
            cfg.max_age_secs,
            cfg.allow_credentials
        );
        caller.data().runtime_config.write().cors = Some(cfg);
        0
    });

    // _rate_limit_configure - Install per-key token-bucket rate limiting.
    // `strategy` is "ip" or "user". Returns 0 on success, -1 if per_window or
    // window_secs are non-positive.
    register_bridge_fn!(linker, "_rate_limit_configure", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                          per_window: i32,
                                                          window_secs: i32,
                                                          strategy_ptr: i32,
                                                          strategy_len: i32|
     -> i32 {
        if per_window <= 0 || window_secs <= 0 {
            error!(
                "_rate_limit_configure: per_window ({}) and window_secs ({}) must be positive",
                per_window, window_secs
            );
            return -1;
        }
        let strategy_str = read_raw_string(&mut caller, strategy_ptr, strategy_len)
            .unwrap_or_else(|| "ip".to_string());
        let strategy = RateLimitStrategy::parse(&strategy_str);
        let cfg = RateLimitConfig {
            per_window: per_window as u32,
            window_secs: window_secs as u32,
            strategy,
        };
        info!(
            "_rate_limit_configure: {} per {}s by {:?}",
            cfg.per_window, cfg.window_secs, cfg.strategy
        );
        caller.data().runtime_config.write().rate_limit = Some(cfg);
        0
    });

    // _http_set_global_error_handler - Register the WASM export name to invoke
    // when a route handler errors. The server falls back to this handler before
    // returning a default 500. Returns 0 on success, -1 if the handler name is
    // empty.
    register_bridge_fn!(
        linker,
        "_http_set_global_error_handler",
        |mut caller: Caller<'_, WasmState>, name_ptr: i32, name_len: i32| -> i32 {
            let name = match read_raw_string(&mut caller, name_ptr, name_len) {
                Some(s) if !s.trim().is_empty() => s,
                _ => {
                    error!("_http_set_global_error_handler: handler name is empty");
                    return -1;
                }
            };
            info!(
                "_http_set_global_error_handler: registered handler '{}'",
                name
            );
            caller.data().runtime_config.write().global_error_handler = Some(name);
            0
        }
    );

    Ok(())
}

/// Register request context functions (_req_param, _req_query, _req_body, etc.)
fn register_request_context_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _req_param - Get a path parameter by name
    linker
        .func_wrap(
            "env",
            "_req_param",
            |mut caller: Caller<'_, WasmState>, name_ptr: i32, name_len: i32| -> i32 {
                let param_name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s,
                    None => {
                        error!("_req_param: Failed to read param name");
                        return write_string_to_caller(&mut caller, "");
                    }
                };

                debug!("_req_param: Looking for param '{}'", param_name);

                let value = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .and_then(|ctx| ctx.params.get(&param_name).cloned())
                        .unwrap_or_default()
                };

                debug!(
                    "_req_param: Returning value '{}' for param '{}'",
                    value, param_name
                );
                write_string_to_caller(&mut caller, &value)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_param: {}", e)))?;

    // _req_query - Get a query parameter by name
    linker
        .func_wrap(
            "env",
            "_req_query",
            |mut caller: Caller<'_, WasmState>, name_ptr: i32, name_len: i32| -> i32 {
                let query_name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s,
                    None => return write_string_to_caller(&mut caller, ""),
                };

                let value = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .and_then(|ctx| ctx.query.get(&query_name).cloned())
                        .unwrap_or_default()
                };

                write_string_to_caller(&mut caller, &value)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_query: {}", e)))?;

    // _req_body - Get the request body
    linker
        .func_wrap(
            "env",
            "_req_body",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                let body = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .map(|ctx| ctx.body.clone())
                        .unwrap_or_default()
                };

                write_string_to_caller(&mut caller, &body)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_body: {}", e)))?;

    // _req_body_bytes - Get the raw request body as opaque bytes.
    //
    // Returns a length-prefixed buffer ([4-byte LE length][bytes]) — the same
    // layout consumed by `_fs_write_bytes` and produced by `write_string_to_caller`
    // for text bodies. When `body_bytes` is populated (HTTP entry point + test
    // dispatcher), the raw wire bytes flow through verbatim so binary payloads
    // (gzip tarballs, invalid-UTF-8 sequences, 0x00 / 0xFF bytes) survive with
    // their SHA-256 intact. When absent (background jobs, cron, WS handlers
    // that don't own a request body), we fall back to the UTF-8 bytes of the
    // `body` string surface — keeps existing text-only handlers working.
    //
    // Additive to `_req_body`: same request, both bridges coexist. Registry
    // entry: function-registry.toml `_req_body_bytes`, hosts = ["server"].
    linker
        .func_wrap(
            "env",
            "_req_body_bytes",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                let bytes: Vec<u8> = {
                    let state = caller.data();
                    match state.request_context.as_ref() {
                        Some(ctx) => match &ctx.body_bytes {
                            Some(b) => b.clone(),
                            None => ctx.body.as_bytes().to_vec(),
                        },
                        None => Vec::new(),
                    }
                };

                // `write_bytes_to_caller` allocates via WASM malloc and emits
                // the [4-byte LE length][bytes] layout. Empty body → a valid
                // pointer to a header with length 0 (never 0/null).
                host_bridge::write_bytes_to_caller(&mut caller, &bytes)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_body_bytes: {}", e)))?;
    // Dot-notation alias `req.body_bytes` is registered by
    // `register_dot_aliases` at end-of-registration (compiler >= 0.30.120
    // emits both forms).

    // _req_body_sha256_hex - Server-computed SHA-256 (lowercase hex, 64 chars)
    // of the raw pre-parse request body. Byte source matches _req_body_bytes:
    // ctx.body_bytes when raw middleware buffered them, else UTF-8 of ctx.body.
    // Returns a length-prefixed string (write_string_to_caller). One-shot for
    // integrity checks against X-Tarball-SHA256 etc. — obviates the two-step
    // _req_body_bytes → _crypto_sha256_bytes path. Registry entry:
    // function-registry.toml `_req_body_sha256_hex`, hosts = ["server"].
    linker
        .func_wrap(
            "env",
            "_req_body_sha256_hex",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                use sha2::Digest;
                let hex_digest = {
                    let state = caller.data();
                    let bytes: &[u8] = match state.request_context.as_ref() {
                        Some(ctx) => match &ctx.body_bytes {
                            Some(b) => b.as_slice(),
                            None => ctx.body.as_bytes(),
                        },
                        None => &[],
                    };
                    hex::encode(sha2::Sha256::digest(bytes))
                };

                write_string_to_caller(&mut caller, &hex_digest)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_body_sha256_hex: {}", e)))?;

    // _req_body_field - Get a field from JSON request body
    linker
        .func_wrap(
            "env",
            "_req_body_field",
            |mut caller: Caller<'_, WasmState>, field_ptr: i32, field_len: i32| -> i32 {
                let field_name = match read_raw_string(&mut caller, field_ptr, field_len) {
                    Some(s) => s,
                    None => return write_string_to_caller(&mut caller, ""),
                };

                let value = {
                    let state = caller.data();
                    if let Some(ctx) = state.request_context.as_ref() {
                        let content_type = ctx
                            .headers
                            .iter()
                            .find(|(k, _)| k.to_lowercase() == "content-type")
                            .map(|(_, v)| v.to_lowercase())
                            .unwrap_or_default();
                        if content_type.contains("application/x-www-form-urlencoded") {
                            use url::form_urlencoded;
                            form_urlencoded::parse(ctx.body.as_bytes())
                                .into_owned()
                                .find(|(k, _)| k == &field_name)
                                .map(|(_, v)| v)
                                .unwrap_or_default()
                        } else {
                            serde_json::from_str::<serde_json::Value>(&ctx.body)
                                .ok()
                                .and_then(|json| {
                                    json.get(&field_name).map(|v| match v {
                                        serde_json::Value::String(s) => s.clone(),
                                        serde_json::Value::Null => String::new(),
                                        other => other.to_string(),
                                    })
                                })
                                .unwrap_or_default()
                        }
                    } else {
                        String::new()
                    }
                };

                debug!("_req_body_field({}): {}", field_name, value);
                write_string_to_caller(&mut caller, &value)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_body_field: {}", e)))?;

    // _req_param_int - Get a path parameter as integer
    linker
        .func_wrap(
            "env",
            "_req_param_int",
            |mut caller: Caller<'_, WasmState>, name_ptr: i32, name_len: i32| -> i32 {
                let param_name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s,
                    None => return 0,
                };

                let value = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .and_then(|ctx| ctx.params.get(&param_name))
                        .and_then(|v| v.parse::<i32>().ok())
                        .unwrap_or(0)
                };

                debug!("_req_param_int({}): {}", param_name, value);
                value
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_param_int: {}", e)))?;

    // _req_header - Get a request header by name
    linker
        .func_wrap(
            "env",
            "_req_header",
            |mut caller: Caller<'_, WasmState>, name_ptr: i32, name_len: i32| -> i32 {
                let header_name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s.to_lowercase(),
                    None => return write_string_to_caller(&mut caller, ""),
                };

                let value = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .and_then(|ctx| {
                            ctx.headers
                                .iter()
                                .find(|(k, _)| k.to_lowercase() == header_name)
                                .map(|(_, v)| v.clone())
                        })
                        .unwrap_or_default()
                };

                write_string_to_caller(&mut caller, &value)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_header: {}", e)))?;

    // _req_method - Get the HTTP method
    linker
        .func_wrap(
            "env",
            "_req_method",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                let method = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .map(|ctx| ctx.method.clone())
                        .unwrap_or_else(|| "GET".to_string())
                };

                write_string_to_caller(&mut caller, &method)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_method: {}", e)))?;

    // _req_path - Get the request path
    linker
        .func_wrap(
            "env",
            "_req_path",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                let path = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .map(|ctx| ctx.path.clone())
                        .unwrap_or_else(|| "/".to_string())
                };

                write_string_to_caller(&mut caller, &path)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_path: {}", e)))?;

    // _req_cookie - Get a cookie value by name
    linker
        .func_wrap(
            "env",
            "_req_cookie",
            |mut caller: Caller<'_, WasmState>, name_ptr: i32, name_len: i32| -> i32 {
                let cookie_name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s,
                    None => return write_string_to_caller(&mut caller, ""),
                };

                debug!("_req_cookie: Looking for cookie '{}'", cookie_name);

                let value = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .and_then(|ctx| {
                            // Find Cookie header
                            ctx.headers
                                .iter()
                                .find(|(k, _)| k.to_lowercase() == "cookie")
                                .and_then(|(_, cookie_header)| {
                                    let cookies = parse_cookies(cookie_header);
                                    cookies.get(&cookie_name).cloned()
                                })
                        })
                        .unwrap_or_default()
                };

                write_string_to_caller(&mut caller, &value)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_cookie: {}", e)))?;

    // _req_headers - Get all request headers as JSON
    linker
        .func_wrap(
            "env",
            "_req_headers",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                let headers_json = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .map(|ctx| {
                            let mut map = serde_json::Map::new();
                            for (key, value) in &ctx.headers {
                                map.insert(key.clone(), serde_json::Value::String(value.clone()));
                            }
                            serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
                        })
                        .unwrap_or_else(|| "{}".to_string())
                };

                debug!("_req_headers: Returning all headers");
                write_string_to_caller(&mut caller, &headers_json)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_headers: {}", e)))?;

    // _req_form - Parse form-urlencoded body as JSON
    linker
        .func_wrap(
            "env",
            "_req_form",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                let form_json = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .map(|ctx| {
                            use url::form_urlencoded;
                            let params: std::collections::HashMap<String, String> =
                                form_urlencoded::parse(ctx.body.as_bytes())
                                    .into_owned()
                                    .collect();
                            serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string())
                        })
                        .unwrap_or_else(|| "{}".to_string())
                };

                debug!("_req_form: Parsed form data");
                write_string_to_caller(&mut caller, &form_json)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_form: {}", e)))?;

    // _req_ip - Get client IP address
    linker
        .func_wrap(
            "env",
            "_req_ip",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                let ip = {
                    let state = caller.data();
                    state
                        .request_context
                        .as_ref()
                        .and_then(|ctx| {
                            // Try X-Forwarded-For first (can be comma-separated list)
                            ctx.headers
                                .iter()
                                .find(|(k, _)| k.to_lowercase() == "x-forwarded-for")
                                .and_then(|(_, v)| {
                                    v.split(',').next().map(|s| s.trim().to_string())
                                })
                                .or_else(|| {
                                    // Try X-Real-IP
                                    ctx.headers
                                        .iter()
                                        .find(|(k, _)| k.to_lowercase() == "x-real-ip")
                                        .map(|(_, v)| v.clone())
                                })
                        })
                        .unwrap_or_else(|| "unknown".to_string())
                };

                debug!("_req_ip: {}", ip);
                write_string_to_caller(&mut caller, &ip)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _req_ip: {}", e)))?;

    // =========================================
    // PHASE 3 REQUEST CONTEXT EXTRAS
    // =========================================

    // _req_params() -> ptr (JSON map of path params)
    register_bridge_fn!(linker, "_req_params", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let json = caller
            .data()
            .request_context
            .as_ref()
            .map(|ctx| serde_json::to_string(&ctx.params).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());
        write_string_to_caller(&mut caller, &json)
    });

    // _req_queries() -> ptr (JSON map of query params)
    register_bridge_fn!(linker, "_req_queries", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let json = caller
            .data()
            .request_context
            .as_ref()
            .map(|ctx| serde_json::to_string(&ctx.query).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());
        write_string_to_caller(&mut caller, &json)
    });

    // _req_cookies() -> ptr (JSON map parsed from Cookie header)
    register_bridge_fn!(linker, "_req_cookies", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let json = caller
            .data()
            .request_context
            .as_ref()
            .and_then(|ctx| {
                ctx.headers
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == "cookie")
                    .map(|(_, v)| parse_cookies(v))
            })
            .map(|m| serde_json::to_string(&m).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());
        write_string_to_caller(&mut caller, &json)
    });

    // _req_json() -> ptr — body if Content-Type contains application/json, "" otherwise
    register_bridge_fn!(
        linker,
        "_req_json",
        |mut caller: Caller<'_, WasmState>| -> i32 {
            let body = caller
                .data()
                .request_context
                .as_ref()
                .and_then(|ctx| {
                    let is_json = ctx
                        .headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == "content-type")
                        .map(|(_, v)| v.to_lowercase().contains("application/json"))
                        .unwrap_or(false);
                    if is_json {
                        Some(ctx.body.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            write_string_to_caller(&mut caller, &body)
        }
    );

    // _req_is_json() -> boolean
    register_bridge_fn!(
        linker,
        "_req_is_json",
        |caller: Caller<'_, WasmState>| -> i32 {
            let is_json = caller
                .data()
                .request_context
                .as_ref()
                .map(|ctx| {
                    ctx.headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == "content-type")
                        .map(|(_, v)| v.to_lowercase().contains("application/json"))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if is_json { 1 } else { 0 }
        }
    );

    // _req_content_type() -> ptr
    register_bridge_fn!(linker, "_req_content_type", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let v = caller
            .data()
            .request_context
            .as_ref()
            .and_then(|ctx| {
                ctx.headers
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == "content-type")
                    .map(|(_, val)| val.clone())
            })
            .unwrap_or_default();
        write_string_to_caller(&mut caller, &v)
    });

    // _req_auth_token() -> ptr — Bearer token without "Bearer " prefix
    register_bridge_fn!(linker, "_req_auth_token", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let v = caller
            .data()
            .request_context
            .as_ref()
            .and_then(|ctx| {
                ctx.headers
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == "authorization")
                    .map(|(_, val)| val.clone())
            })
            .map(|val| {
                let trimmed = val.trim();
                if let Some(rest) = trimmed.strip_prefix("Bearer ") {
                    rest.trim().to_string()
                } else if let Some(rest) = trimmed.strip_prefix("bearer ") {
                    rest.trim().to_string()
                } else {
                    trimmed.to_string()
                }
            })
            .unwrap_or_default();
        write_string_to_caller(&mut caller, &v)
    });

    // _req_has_auth() -> boolean (Authorization header present and non-empty)
    register_bridge_fn!(
        linker,
        "_req_has_auth",
        |caller: Caller<'_, WasmState>| -> i32 {
            let has = caller
                .data()
                .request_context
                .as_ref()
                .map(|ctx| {
                    ctx.headers
                        .iter()
                        .any(|(k, v)| k.to_lowercase() == "authorization" && !v.is_empty())
                })
                .unwrap_or(false);
            if has { 1 } else { 0 }
        }
    );

    // _req_has_header(name) -> boolean (case-insensitive)
    register_bridge_fn!(linker, "_req_has_header", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    p: i32,
                                                    l: i32|
     -> i32 {
        let name = match read_raw_string(&mut caller, p, l) {
            Some(s) => s.to_lowercase(),
            None => return 0,
        };
        let has = caller
            .data()
            .request_context
            .as_ref()
            .map(|ctx| ctx.headers.iter().any(|(k, _)| k.to_lowercase() == name))
            .unwrap_or(false);
        if has { 1 } else { 0 }
    });

    // _req_has_query(name) -> boolean
    register_bridge_fn!(linker, "_req_has_query", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                   p: i32,
                                                   l: i32|
     -> i32 {
        let name = match read_raw_string(&mut caller, p, l) {
            Some(s) => s,
            None => return 0,
        };
        let has = caller
            .data()
            .request_context
            .as_ref()
            .map(|ctx| ctx.query.contains_key(&name))
            .unwrap_or(false);
        if has { 1 } else { 0 }
    });

    // _req_has_cookie(name) -> boolean
    register_bridge_fn!(linker, "_req_has_cookie", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    p: i32,
                                                    l: i32|
     -> i32 {
        let name = match read_raw_string(&mut caller, p, l) {
            Some(s) => s,
            None => return 0,
        };
        let has = caller
            .data()
            .request_context
            .as_ref()
            .and_then(|ctx| {
                ctx.headers
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == "cookie")
                    .map(|(_, v)| parse_cookies(v).contains_key(&name))
            })
            .unwrap_or(false);
        if has { 1 } else { 0 }
    });

    Ok(())
}

/// Register session management functions (_session_store, _session_get, _session_delete, etc.)
/// These align with the frame.auth plugin API using key-value session storage.
fn register_session_management_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _session_store - Store data in session (key-value storage)
    // Args: session_id_ptr, key_ptr, value_ptr, ttl, flags
    // Returns: 1 on success, 0 on error
    linker
        .func_wrap(
            "env",
            "_session_store",
            |mut caller: Caller<'_, WasmState>,
             session_id_ptr: i32,
             key_ptr: i32,
             value_ptr: i32,
             _ttl: i32,
             _flags: i32|
             -> i32 {
                if !check_bridge_permission(&caller, "_session_store") {
                    return 0;
                }
                let session_id = match read_string_from_caller(&mut caller, session_id_ptr) {
                    Some(s) => s,
                    None => {
                        error!("_session_store: Failed to read session ID");
                        return 0;
                    }
                };
                let key = match read_string_from_caller(&mut caller, key_ptr) {
                    Some(s) => s,
                    None => {
                        error!("_session_store: Failed to read key");
                        return 0;
                    }
                };
                let value = match read_string_from_caller(&mut caller, value_ptr) {
                    Some(s) => s,
                    None => {
                        error!("_session_store: Failed to read value");
                        return 0;
                    }
                };

                debug!(
                    "_session_store: id={}, key={}, value_len={}",
                    session_id,
                    key,
                    value.len()
                );

                // Store as key=value under the session ID
                let data = if key.is_empty() {
                    value
                } else {
                    // Build JSON object with key-value pair
                    let session_store = caller.data().session_store.clone();
                    let existing = {
                        let mut store = session_store.write().expect("session store lock poisoned");
                        store.get_raw(&session_id)
                    };

                    let mut obj: serde_json::Value = existing
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_else(|| serde_json::json!({}));

                    if let Some(map) = obj.as_object_mut() {
                        map.insert(key, serde_json::Value::String(value));
                    }
                    serde_json::to_string(&obj).unwrap_or_default()
                };

                let session_store = caller.data().session_store.clone();
                let mut store = session_store.write().expect("session store lock poisoned");
                if store.store_raw(&session_id, &data) {
                    1
                } else {
                    0
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _session_store: {}", e)))?;

    // _session_get - Get current session data (returns stored data or empty string)
    // Args: none (session ID comes from request context)
    // Returns: pointer to data string (length-prefixed)
    linker
        .func_wrap(
            "env",
            "_session_get",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                if !check_bridge_permission(&caller, "_session_get") {
                    return write_string_to_caller(&mut caller, "");
                }
                // Get session ID from auth context or cookie
                let session_id = {
                    let state = caller.data();
                    state
                        .auth_context
                        .as_ref()
                        .and_then(|ctx| ctx.session_id.clone())
                        .or_else(|| {
                            state.request_context.as_ref().and_then(|ctx| {
                                ctx.headers
                                    .iter()
                                    .find(|(k, _)| k.to_lowercase() == "cookie")
                                    .and_then(|(_, cookie_header)| {
                                        let cookies = parse_cookies(cookie_header);
                                        cookies
                                            .get("session")
                                            .cloned()
                                            .or_else(|| cookies.get("sid").cloned())
                                    })
                            })
                        })
                };

                let session_id = match session_id {
                    Some(id) => id,
                    None => {
                        debug!("_session_get: No active session");
                        return write_string_to_caller(&mut caller, "");
                    }
                };

                debug!("_session_get: Looking up session {}", session_id);

                let session_store = caller.data().session_store.clone();
                let data = {
                    let mut store = session_store.write().expect("session store lock poisoned");
                    store.get_raw(&session_id)
                };

                match data {
                    Some(d) => {
                        debug!("_session_get: Found data for {}", session_id);
                        write_string_to_caller(&mut caller, &d)
                    }
                    None => {
                        debug!("_session_get: No data found for {}", session_id);
                        write_string_to_caller(&mut caller, "")
                    }
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _session_get: {}", e)))?;

    // _session_delete - Delete current session data
    // Args: none (session ID comes from request context)
    // Returns: 1 if deleted, 0 if not found
    linker
        .func_wrap(
            "env",
            "_session_delete",
            |caller: Caller<'_, WasmState>| -> i32 {
                if !check_bridge_permission(&caller, "_session_delete") {
                    return 0;
                }
                // Get session ID from auth context or cookie
                let session_id = {
                    let state = caller.data();
                    state
                        .auth_context
                        .as_ref()
                        .and_then(|ctx| ctx.session_id.clone())
                        .or_else(|| {
                            state.request_context.as_ref().and_then(|ctx| {
                                ctx.headers
                                    .iter()
                                    .find(|(k, _)| k.to_lowercase() == "cookie")
                                    .and_then(|(_, cookie_header)| {
                                        let cookies = parse_cookies(cookie_header);
                                        cookies
                                            .get("session")
                                            .cloned()
                                            .or_else(|| cookies.get("sid").cloned())
                                    })
                            })
                        })
                };

                let session_id = match session_id {
                    Some(id) => id,
                    None => {
                        debug!("_session_delete: No active session");
                        return 0;
                    }
                };

                info!("_session_delete: Deleting session {}", session_id);

                let session_store = caller.data().session_store.clone();
                let deleted = {
                    let mut store = session_store.write().expect("session store lock poisoned");
                    store.delete_raw(&session_id)
                };

                if deleted { 1 } else { 0 }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _session_delete: {}", e)))?;

    // _session_exists - Check if a session exists
    // Args: id_ptr, id_len
    // Returns: 1 if exists, 0 if not
    linker
        .func_wrap(
            "env",
            "_session_exists",
            |mut caller: Caller<'_, WasmState>, id_ptr: i32, id_len: i32| -> i32 {
                if !check_bridge_permission(&caller, "_session_exists") {
                    return 0;
                }
                let session_id = match read_raw_string(&mut caller, id_ptr, id_len) {
                    Some(s) => s,
                    None => return 0,
                };

                let session_store = caller.data().session_store.clone();
                let store = session_store.read().expect("session store lock poisoned");
                if store.exists_raw(&session_id) { 1 } else { 0 }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _session_exists: {}", e)))?;

    // _session_set_csrf - Store CSRF token for the current session
    // Args: token_ptr, token_len
    // Returns: 1 on success, 0 if no current session
    linker
        .func_wrap(
            "env",
            "_session_set_csrf",
            |mut caller: Caller<'_, WasmState>, token_ptr: i32, token_len: i32| -> i32 {
                let token = match read_raw_string(&mut caller, token_ptr, token_len) {
                    Some(s) => s,
                    None => {
                        error!("_session_set_csrf: Failed to read token");
                        return 0;
                    }
                };

                // Get current session ID from auth context or cookie
                let session_id = {
                    let state = caller.data();
                    state
                        .auth_context
                        .as_ref()
                        .and_then(|ctx| ctx.session_id.clone())
                        .or_else(|| {
                            state.request_context.as_ref().and_then(|ctx| {
                                ctx.headers
                                    .iter()
                                    .find(|(k, _)| k.to_lowercase() == "cookie")
                                    .and_then(|(_, cookie_header)| {
                                        let cookies = parse_cookies(cookie_header);
                                        cookies
                                            .get("session")
                                            .cloned()
                                            .or_else(|| cookies.get("sid").cloned())
                                    })
                            })
                        })
                };

                let session_id = match session_id {
                    Some(id) => id,
                    None => {
                        debug!("_session_set_csrf: No active session");
                        return 0;
                    }
                };

                debug!("_session_set_csrf: Setting CSRF for session {}", session_id);
                let session_store = caller.data().session_store.clone();
                let mut store = session_store.write().expect("session store lock poisoned");
                store.set_csrf(&session_id, &token);
                1
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _session_set_csrf: {}", e)))?;

    // _session_get_csrf - Get CSRF token for the current session
    // Returns: pointer to CSRF token string (or empty if none)
    linker
        .func_wrap(
            "env",
            "_session_get_csrf",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                // Get current session ID from auth context or cookie
                let session_id = {
                    let state = caller.data();
                    state
                        .auth_context
                        .as_ref()
                        .and_then(|ctx| ctx.session_id.clone())
                        .or_else(|| {
                            state.request_context.as_ref().and_then(|ctx| {
                                ctx.headers
                                    .iter()
                                    .find(|(k, _)| k.to_lowercase() == "cookie")
                                    .and_then(|(_, cookie_header)| {
                                        let cookies = parse_cookies(cookie_header);
                                        cookies
                                            .get("session")
                                            .cloned()
                                            .or_else(|| cookies.get("sid").cloned())
                                    })
                            })
                        })
                };

                let session_id = match session_id {
                    Some(id) => id,
                    None => {
                        debug!("_session_get_csrf: No active session");
                        return write_string_to_caller(&mut caller, "");
                    }
                };

                let session_store = caller.data().session_store.clone();
                let store = session_store.read().expect("session store lock poisoned");
                let token = store.get_csrf(&session_id).unwrap_or_default();
                write_string_to_caller(&mut caller, &token)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _session_get_csrf: {}", e)))?;

    // _http_set_cookie - Set a cookie with name and value
    // Args: name_ptr, value_ptr (length-prefixed string pointers)
    // Returns: 1 on success, 0 on error
    linker
        .func_wrap(
            "env",
            "_http_set_cookie",
            |mut caller: Caller<'_, WasmState>, name_ptr: i32, value_ptr: i32| -> i32 {
                if !check_bridge_permission(&caller, "_http_set_cookie") {
                    return 0;
                }
                let name = match read_string_from_caller(&mut caller, name_ptr) {
                    Some(s) => s,
                    None => {
                        error!("_http_set_cookie: Failed to read cookie name");
                        return 0;
                    }
                };
                let value = match read_string_from_caller(&mut caller, value_ptr) {
                    Some(s) => s,
                    None => {
                        error!("_http_set_cookie: Failed to read cookie value");
                        return 0;
                    }
                };

                // Build cookie string: name=value with sensible defaults
                let cookie = format!("{}={}; Path=/; HttpOnly", name, value);

                debug!("_http_set_cookie: {}", cookie);
                caller.data_mut().pending_set_cookie = Some(cookie);
                1
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_set_cookie: {}", e)))?;

    Ok(())
}

/// Register role-based permission functions (_roles_register, _role_has_permission, _role_get_permissions)
fn register_roles_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _roles_register - Register role definitions from JSON
    // Args: json_ptr, json_len (JSON object: { "admin": ["read", "write", "delete"], "user": ["read"] })
    // Returns: 1 on success, 0 on error
    linker
        .func_wrap(
            "env",
            "_roles_register",
            |mut caller: Caller<'_, WasmState>, json_ptr: i32, json_len: i32| -> i32 {
                if !check_bridge_permission(&caller, "_roles_register") {
                    return 0;
                }
                let config_json = match read_raw_string(&mut caller, json_ptr, json_len) {
                    Some(s) => s,
                    None => {
                        error!("_roles_register: Failed to read JSON");
                        return 0;
                    }
                };

                debug!("_roles_register: {}", config_json);

                let roles_store = caller.data().roles_store.clone();
                let mut store = roles_store.write().expect("roles store lock poisoned");
                if store.register(&config_json) { 1 } else { 0 }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _roles_register: {}", e)))?;

    // _role_has_permission - Check if a role has a specific permission
    // Args: role_ptr, role_len, perm_ptr, perm_len
    // Returns: 1 if has permission, 0 if not
    linker
        .func_wrap(
            "env",
            "_role_has_permission",
            |mut caller: Caller<'_, WasmState>,
             role_ptr: i32,
             role_len: i32,
             perm_ptr: i32,
             perm_len: i32|
             -> i32 {
                if !check_bridge_permission(&caller, "_role_has_permission") {
                    return 0;
                }
                let role = match read_raw_string(&mut caller, role_ptr, role_len) {
                    Some(s) => s,
                    None => return 0,
                };
                let permission = match read_raw_string(&mut caller, perm_ptr, perm_len) {
                    Some(s) => s,
                    None => return 0,
                };

                let roles_store = caller.data().roles_store.clone();
                let store = roles_store.read().expect("roles store lock poisoned");
                if store.has_permission(&role, &permission) {
                    1
                } else {
                    0
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _role_has_permission: {}", e)))?;

    // _role_get_permissions - Get all permissions for a role as JSON array
    // Args: role_ptr, role_len
    // Returns: pointer to JSON array string (e.g., '["read","write"]')
    linker
        .func_wrap(
            "env",
            "_role_get_permissions",
            |mut caller: Caller<'_, WasmState>, role_ptr: i32, role_len: i32| -> i32 {
                if !check_bridge_permission(&caller, "_role_get_permissions") {
                    return write_string_to_caller(&mut caller, "[]");
                }
                let role = match read_raw_string(&mut caller, role_ptr, role_len) {
                    Some(s) => s,
                    None => return write_string_to_caller(&mut caller, "[]"),
                };

                let roles_store = caller.data().roles_store.clone();
                let store = roles_store.read().expect("roles store lock poisoned");
                let permissions = store.get_permissions(&role);
                let json = serde_json::to_string(&permissions).unwrap_or_else(|_| "[]".to_string());

                debug!("_role_get_permissions: role={} -> {}", role, json);
                write_string_to_caller(&mut caller, &json)
            },
        )
        .map_err(|e| {
            RuntimeError::wasm(format!("Failed to define _role_get_permissions: {}", e))
        })?;

    Ok(())
}

/// Register session-based authentication functions
fn register_session_auth_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _auth_get_session - Get session info from current request
    linker
        .func_wrap(
            "env",
            "_auth_get_session",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                if !check_bridge_permission(&caller, "_auth_get_session") {
                    return write_string_to_caller(&mut caller, "null");
                }
                let session_json = {
                    let state = caller.data();
                    if let Some(auth) = &state.auth_context {
                        serde_json::json!({
                            "user_id": auth.user_id,
                            "role": auth.role,
                            "session_id": auth.session_id
                        })
                        .to_string()
                    } else {
                        "null".to_string()
                    }
                };

                write_string_to_caller(&mut caller, &session_json)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_get_session: {}", e)))?;

    // _auth_require_auth - Check if user is authenticated
    linker
        .func_wrap(
            "env",
            "_auth_require_auth",
            |caller: Caller<'_, WasmState>| -> i32 {
                let state = caller.data();
                if state.auth_context.is_some() { 1 } else { 0 }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_require_auth: {}", e)))?;

    // _auth_require_role - Check if user has a specific role
    linker
        .func_wrap(
            "env",
            "_auth_require_role",
            |mut caller: Caller<'_, WasmState>, role_ptr: i32, role_len: i32| -> i32 {
                let required_role = match read_raw_string(&mut caller, role_ptr, role_len) {
                    Some(s) => s,
                    None => return 0,
                };

                let state = caller.data();
                if let Some(auth) = &state.auth_context {
                    if auth.role == required_role { 1 } else { 0 }
                } else {
                    0
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_require_role: {}", e)))?;

    // _auth_can - Check if user has permission (role-based)
    linker
        .func_wrap(
            "env",
            "_auth_can",
            |mut caller: Caller<'_, WasmState>, permission_ptr: i32, permission_len: i32| -> i32 {
                let permission = match read_raw_string(&mut caller, permission_ptr, permission_len)
                {
                    Some(s) => s,
                    None => return 0,
                };

                let state = caller.data();
                if let Some(auth) = &state.auth_context {
                    // Simple role-based check: admin can do anything
                    if auth.role == "admin" || auth.role == permission {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_can: {}", e)))?;

    // _auth_has_any_role - Check if user has any of the specified roles
    linker
        .func_wrap(
            "env",
            "_auth_has_any_role",
            |mut caller: Caller<'_, WasmState>, roles_ptr: i32, roles_len: i32| -> i32 {
                let roles_json = match read_raw_string(&mut caller, roles_ptr, roles_len) {
                    Some(s) => s,
                    None => return 0,
                };

                let roles: Vec<String> = serde_json::from_str(&roles_json).unwrap_or_default();

                let state = caller.data();
                if let Some(auth) = &state.auth_context {
                    if roles.contains(&auth.role) { 1 } else { 0 }
                } else {
                    0
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_has_any_role: {}", e)))?;

    // _auth_set_session - Set session data from JSON
    // Args: data_ptr, data_len (JSON with user_id, role, claims)
    // Returns: 1 on success, 0 on error
    linker
        .func_wrap(
            "env",
            "_auth_set_session",
            |mut caller: Caller<'_, WasmState>, data_ptr: i32, data_len: i32| -> i32 {
                if !check_bridge_permission(&caller, "_auth_set_session") {
                    return 0;
                }
                let data_json = match read_raw_string(&mut caller, data_ptr, data_len) {
                    Some(s) => s,
                    None => return 0,
                };

                debug!("_auth_set_session: data={}", data_json);

                let parsed: serde_json::Value = match serde_json::from_str(&data_json) {
                    Ok(v) => v,
                    Err(e) => {
                        error!("_auth_set_session: Invalid JSON: {}", e);
                        return 0;
                    }
                };

                let user_id = parsed.get("user_id").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let role = parsed
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("user")
                    .to_string();
                let claims = parsed
                    .get("claims")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".to_string());

                info!("_auth_set_session: user_id={}, role={}", user_id, role);

                let session_store = caller.data().session_store.clone();

                let session = {
                    let mut store = session_store.write().expect("session store lock poisoned");
                    store.create(user_id, &role, &claims)
                };

                let session_id = session.session_id.clone();

                let set_cookie = {
                    let store = session_store.read().expect("session store lock poisoned");
                    store.format_cookie(&session_id)
                };

                caller.data_mut().pending_set_cookie = Some(set_cookie);
                caller
                    .data_mut()
                    .set_auth_from_session(user_id, role, session_id);

                1
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_set_session: {}", e)))?;

    // _auth_clear_session - Clear the current session
    // Returns: 1 on success, 0 if no session
    linker
        .func_wrap(
            "env",
            "_auth_clear_session",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                if !check_bridge_permission(&caller, "_auth_clear_session") {
                    return 0;
                }
                let session_id = {
                    let state = caller.data();
                    state
                        .auth_context
                        .as_ref()
                        .and_then(|ctx| ctx.session_id.clone())
                        .or_else(|| {
                            state.request_context.as_ref().and_then(|ctx| {
                                ctx.headers
                                    .iter()
                                    .find(|(k, _)| k.to_lowercase() == "cookie")
                                    .and_then(|(_, cookie_header)| {
                                        let cookies = parse_cookies(cookie_header);
                                        cookies
                                            .get("session")
                                            .cloned()
                                            .or_else(|| cookies.get("sid").cloned())
                                    })
                            })
                        })
                };

                let session_id = match session_id {
                    Some(id) => id,
                    None => {
                        debug!("_auth_clear_session: No session to clear");
                        return 0;
                    }
                };

                info!("_auth_clear_session: Clearing session {}", session_id);

                let session_store = caller.data().session_store.clone();

                {
                    let mut store = session_store.write().expect("session store lock poisoned");
                    store.delete(&session_id);
                }

                let clear_cookie = {
                    let store = session_store.read().expect("session store lock poisoned");
                    store.format_clear_cookie()
                };

                caller.data_mut().pending_set_cookie = Some(clear_cookie);
                caller.data_mut().auth_context = None;

                1
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_clear_session: {}", e)))?;

    // _auth_user_id - Get the current user's ID
    linker
        .func_wrap(
            "env",
            "_auth_user_id",
            |caller: Caller<'_, WasmState>| -> i32 {
                let state = caller.data();
                state.auth_context.as_ref().map(|a| a.user_id).unwrap_or(0)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_user_id: {}", e)))?;

    // _auth_user_role - Get the current user's role
    linker
        .func_wrap(
            "env",
            "_auth_user_role",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                let role = {
                    let state = caller.data();
                    state
                        .auth_context
                        .as_ref()
                        .map(|a| a.role.clone())
                        .unwrap_or_default()
                };
                write_string_to_caller(&mut caller, &role)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _auth_user_role: {}", e)))?;

    // =========================================
    // PHASE 3 AUTH EXTRAS
    // =========================================

    // _auth_check() -> boolean
    register_bridge_fn!(
        linker,
        "_auth_check",
        |caller: Caller<'_, WasmState>| -> i32 {
            if caller.data().auth_context.is_some() {
                1
            } else {
                0
            }
        }
    );

    // _auth_user() -> ptr — JSON {id, role, claims} or ""
    register_bridge_fn!(
        linker,
        "_auth_user",
        |mut caller: Caller<'_, WasmState>| -> i32 {
            let sid = match current_session_id(caller.data()) {
                Some(s) => s,
                None => return write_string_to_caller(&mut caller, ""),
            };
            let session_store = caller.data().session_store.clone();
            let session = {
                let mut store = session_store.write().expect("session store lock poisoned");
                store.get(&sid)
            };
            let payload = match session {
                Some(sd) => {
                    let claims: serde_json::Value = serde_json::from_str(&sd.claims)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                    serde_json::json!({
                        "id": sd.user_id,
                        "role": sd.role,
                        "claims": claims,
                    })
                    .to_string()
                }
                None => match &caller.data().auth_context {
                    Some(a) => serde_json::json!({
                        "id": a.user_id,
                        "role": a.role,
                        "claims": {},
                    })
                    .to_string(),
                    None => String::new(),
                },
            };
            write_string_to_caller(&mut caller, &payload)
        }
    );

    // _auth_is_admin() -> boolean
    register_bridge_fn!(
        linker,
        "_auth_is_admin",
        |caller: Caller<'_, WasmState>| -> i32 {
            match &caller.data().auth_context {
                Some(a) if a.role == "admin" => 1,
                _ => 0,
            }
        }
    );

    // _auth_is_owner(user_id) -> boolean — auth.user_id == arg OR is_admin
    register_bridge_fn!(linker, "_auth_is_owner", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                   p: i32,
                                                   l: i32|
     -> i32 {
        let arg = match read_raw_string(&mut caller, p, l) {
            Some(s) => s,
            None => return 0,
        };
        match &caller.data().auth_context {
            Some(a) if a.role == "admin" => 1,
            Some(a) => {
                let parsed = arg.trim().parse::<i32>().ok();
                match parsed {
                    Some(id) if id == a.user_id => 1,
                    // Allow string match too (in case the resource id is a string).
                    _ => {
                        if arg == a.user_id.to_string() {
                            1
                        } else {
                            0
                        }
                    }
                }
            }
            _ => 0,
        }
    });

    // _auth_require_any_role(roles_json) -> boolean
    register_bridge_fn!(linker, "_auth_require_any_role", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                           p: i32,
                                                           l: i32|
     -> i32 {
        let roles_json = match read_raw_string(&mut caller, p, l) {
            Some(s) => s,
            None => return 0,
        };
        let roles: Vec<String> = serde_json::from_str(&roles_json).unwrap_or_default();
        match &caller.data().auth_context {
            Some(a) if a.role == "admin" => 1,
            Some(a) if roles.iter().any(|r| r == &a.role) => 1,
            _ => 0,
        }
    });

    // =========================================
    // PHASE 3 SESSION EXTRAS
    // =========================================

    // _session_create(user_id, role, claims_json) -> ptr (session id; sets cookie)
    register_bridge_fn!(linker, "_session_create", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    up: i32,
                                                    ul: i32,
                                                    rp: i32,
                                                    rl: i32,
                                                    cp: i32,
                                                    cl: i32|
     -> i32 {
        let user_id_str = read_raw_string(&mut caller, up, ul).unwrap_or_default();
        let role = read_raw_string(&mut caller, rp, rl).unwrap_or_else(|| "user".to_string());
        let claims = read_raw_string(&mut caller, cp, cl).unwrap_or_else(|| "{}".to_string());
        let user_id: i32 = user_id_str.trim().parse().unwrap_or(0);

        let session_store = caller.data().session_store.clone();
        let session = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.create(user_id, &role, &claims)
        };
        let sid = session.session_id.clone();
        let cookie = {
            let store = session_store.read().expect("session store lock poisoned");
            store.format_cookie(&sid)
        };
        caller.data_mut().pending_set_cookie = Some(cookie);
        caller
            .data_mut()
            .set_auth_from_session(user_id, role, sid.clone());
        write_string_to_caller(&mut caller, &sid)
    });

    // _session_create_with_ttl(user_id, role, claims_json, ttl_seconds) -> ptr
    register_bridge_fn!(linker, "_session_create_with_ttl", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                             up: i32,
                                                             ul: i32,
                                                             rp: i32,
                                                             rl: i32,
                                                             cp: i32,
                                                             cl: i32,
                                                             ttl: i32|
     -> i32 {
        let user_id_str = read_raw_string(&mut caller, up, ul).unwrap_or_default();
        let role = read_raw_string(&mut caller, rp, rl).unwrap_or_else(|| "user".to_string());
        let claims = read_raw_string(&mut caller, cp, cl).unwrap_or_else(|| "{}".to_string());
        let user_id: i32 = user_id_str.trim().parse().unwrap_or(0);
        let ttl = if ttl > 0 { ttl as u64 } else { 0 };

        let session_store = caller.data().session_store.clone();
        let session = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.create(user_id, &role, &claims)
        };
        let sid = session.session_id.clone();
        // Take the default cookie format and substitute the Max-Age with the requested ttl.
        let base = {
            let store = session_store.read().expect("session store lock poisoned");
            store.format_cookie(&sid)
        };
        let cookie = if ttl > 0 {
            replace_cookie_max_age(&base, ttl)
        } else {
            base
        };
        caller.data_mut().pending_set_cookie = Some(cookie);
        caller
            .data_mut()
            .set_auth_from_session(user_id, role, sid.clone());
        write_string_to_caller(&mut caller, &sid)
    });

    // _session_destroy() -> boolean
    register_bridge_fn!(linker, "_session_destroy", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let sid = match current_session_id(caller.data()) {
            Some(s) => s,
            None => return 0,
        };
        let session_store = caller.data().session_store.clone();
        let removed = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.delete_raw(&sid)
        };
        let clear = {
            let store = session_store.read().expect("session store lock poisoned");
            store.format_clear_cookie()
        };
        caller.data_mut().pending_set_cookie = Some(clear);
        caller.data_mut().auth_context = None;
        if removed { 1 } else { 0 }
    });

    // _session_extend(ttl_seconds) -> boolean — touches session, refreshes cookie max-age
    register_bridge_fn!(linker, "_session_extend", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    ttl: i32|
     -> i32 {
        let sid = match current_session_id(caller.data()) {
            Some(s) => s,
            None => return 0,
        };
        let session_store = caller.data().session_store.clone();
        let exists = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.get(&sid).is_some() || store.get_raw(&sid).is_some()
        };
        if !exists {
            return 0;
        }
        let base = {
            let store = session_store.read().expect("session store lock poisoned");
            store.format_cookie(&sid)
        };
        let cookie = if ttl > 0 {
            replace_cookie_max_age(&base, ttl as u64)
        } else {
            base
        };
        caller.data_mut().pending_set_cookie = Some(cookie);
        1
    });

    // _session_user_id() -> ptr (string of current user's id, "" if none)
    register_bridge_fn!(linker, "_session_user_id", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let v = caller
            .data()
            .auth_context
            .as_ref()
            .map(|a| a.user_id.to_string())
            .unwrap_or_default();
        write_string_to_caller(&mut caller, &v)
    });

    // _session_role() -> ptr
    register_bridge_fn!(linker, "_session_role", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let v = caller
            .data()
            .auth_context
            .as_ref()
            .map(|a| a.role.clone())
            .unwrap_or_default();
        write_string_to_caller(&mut caller, &v)
    });

    // _session_claim(key) -> ptr — single claim from the typed session's claims JSON
    register_bridge_fn!(linker, "_session_claim", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                   p: i32,
                                                   l: i32|
     -> i32 {
        let key = match read_raw_string(&mut caller, p, l) {
            Some(s) => s,
            None => return write_string_to_caller(&mut caller, ""),
        };
        let sid = match current_session_id(caller.data()) {
            Some(s) => s,
            None => return write_string_to_caller(&mut caller, ""),
        };
        let session_store = caller.data().session_store.clone();
        let session = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.get(&sid)
        };
        let value = session
            .and_then(|sd| {
                serde_json::from_str::<serde_json::Value>(&sd.claims)
                    .ok()
                    .and_then(|v| v.get(&key).cloned())
                    .map(|v| match v {
                        serde_json::Value::String(s) => s,
                        serde_json::Value::Null => String::new(),
                        other => other.to_string(),
                    })
            })
            .unwrap_or_default();
        write_string_to_caller(&mut caller, &value)
    });

    // _session_get_value(key) -> ptr — value previously stored via _session_store (key=value JSON)
    register_bridge_fn!(linker, "_session_get_value", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                       p: i32,
                                                       l: i32|
     -> i32 {
        let key = match read_raw_string(&mut caller, p, l) {
            Some(s) => s,
            None => return write_string_to_caller(&mut caller, ""),
        };
        let sid = match current_session_id(caller.data()) {
            Some(s) => s,
            None => return write_string_to_caller(&mut caller, ""),
        };
        let session_store = caller.data().session_store.clone();
        let raw = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.get_raw(&sid)
        };
        let value = raw
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get(&key).cloned())
            .map(|v| match v {
                serde_json::Value::String(s) => s,
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        write_string_to_caller(&mut caller, &value)
    });

    // _session_has_key(key) -> boolean
    register_bridge_fn!(linker, "_session_has_key", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                     p: i32,
                                                     l: i32|
     -> i32 {
        let key = match read_raw_string(&mut caller, p, l) {
            Some(s) => s,
            None => return 0,
        };
        let sid = match current_session_id(caller.data()) {
            Some(s) => s,
            None => return 0,
        };
        let session_store = caller.data().session_store.clone();
        let raw = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.get_raw(&sid)
        };
        let present = raw
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|v| v.get(&key).is_some())
            .unwrap_or(false);
        if present { 1 } else { 0 }
    });

    // _auth_create_reset_token(user_id, ttl_seconds) -> ptr
    //
    // Generates a cryptographically random opaque token, persists
    // (sha256(token), user_id, expires_at) in the SessionStore, and returns
    // the plaintext token as a length-prefixed string. The store never sees
    // the plaintext, so a memory dump does not leak usable tokens.
    //
    // Backs `auth.createResetToken(userId, ttlSeconds)` per frame.auth spec §16.
    // Returns empty string on invalid inputs (non-positive ttl or user_id ≤ 0).
    register_bridge_fn!(linker, "_auth_create_reset_token", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                             user_id: i64,
                                                             ttl_seconds: i64|
     -> i32 {
        if !check_bridge_permission(&caller, "_auth_create_reset_token") {
            return write_string_to_caller(&mut caller, "");
        }
        if user_id <= 0 || ttl_seconds <= 0 {
            return write_string_to_caller(&mut caller, "");
        }
        let user_id_i32 = if user_id > i32::MAX as i64 {
            warn!(
                "_auth_create_reset_token: user_id {} exceeds i32 range",
                user_id
            );
            return write_string_to_caller(&mut caller, "");
        } else {
            user_id as i32
        };

        // 32 bytes of entropy from two v4 UUIDs (v4 uses OS RNG). Hex-encoded
        // to a 64-char opaque token that is safe to embed in a URL.
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(a.as_bytes());
        buf[16..].copy_from_slice(b.as_bytes());
        let token = hex::encode(buf);

        use sha2::Digest;
        let token_hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));

        let ttl = std::time::Duration::from_secs(ttl_seconds as u64);
        let session_store = caller.data().session_store.clone();
        let inserted = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.store_reset_token(&token_hash, user_id_i32, ttl)
        };
        if !inserted {
            // Cryptographic collision is impossible in practice; only happens
            // if the store already had a matching hash from a prior call
            // this same nanosecond. Refuse rather than issue a duplicate.
            warn!(
                "_auth_create_reset_token: hash collision for user {}",
                user_id_i32
            );
            return write_string_to_caller(&mut caller, "");
        }

        write_string_to_caller(&mut caller, &token)
    });

    // _auth_consume_reset_token(token) -> i64
    //
    // Atomically validates the presented token and, on success, removes the
    // stored row so the token cannot be used again. Returns the associated
    // user_id, or 0 if the token is invalid, expired, or already consumed.
    //
    // Backs `auth.consumeResetToken(token)` per frame.auth spec §16.
    // Atomicity is provided by the SessionStore write lock — a second caller
    // racing on the same token sees the row already gone.
    register_bridge_fn!(linker, "_auth_consume_reset_token", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                              token_ptr: i32,
                                                              token_len: i32|
     -> i64 {
        if !check_bridge_permission(&caller, "_auth_consume_reset_token") {
            return 0;
        }
        let token = match read_raw_string(&mut caller, token_ptr, token_len) {
            Some(s) if !s.is_empty() => s,
            _ => return 0,
        };

        use sha2::Digest;
        let token_hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));

        let session_store = caller.data().session_store.clone();
        let user_id = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.consume_reset_token(&token_hash)
        };
        match user_id {
            Some(id) => id as i64,
            None => 0,
        }
    });

    // _jwt_refresh_and_rotate(token, secret, algorithm, new_ttl_seconds) -> ptr
    //
    // Verifies the supplied refresh token, atomically marks its `jti` as
    // consumed in the revocation list, and returns a freshly-signed token that
    // carries a new `jti`, `iat`, and `exp = now + new_ttl_seconds`. Enforces
    // AUTH-J007 / AUTH-J009 single-use rotation: replaying a token that has
    // already been rotated returns empty string.
    //
    // Returns empty string on any of: invalid signature, expired token, missing
    // `jti`/`sub` claims, or replay of an already-consumed token.
    register_bridge_fn!(linker, "_jwt_refresh_and_rotate", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                            token_ptr: i32,
                                                            token_len: i32,
                                                            secret_ptr: i32,
                                                            secret_len: i32,
                                                            algo_ptr: i32,
                                                            algo_len: i32,
                                                            new_ttl_seconds: i64|
     -> i32 {
        if !check_bridge_permission(&caller, "_jwt_refresh_and_rotate") {
            return write_string_to_caller(&mut caller, "");
        }

        let token = match read_raw_string(&mut caller, token_ptr, token_len) {
            Some(s) if !s.is_empty() => s,
            _ => return write_string_to_caller(&mut caller, ""),
        };
        let secret = match read_raw_string(&mut caller, secret_ptr, secret_len) {
            Some(s) if !s.is_empty() => s,
            _ => return write_string_to_caller(&mut caller, ""),
        };
        let algorithm = read_raw_string(&mut caller, algo_ptr, algo_len)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "HS256".to_string());

        let new_ttl_seconds = if new_ttl_seconds > 0 {
            new_ttl_seconds as u64
        } else {
            return write_string_to_caller(&mut caller, "");
        };

        // 1. Verify signature and expiry. `verify_jwt` also enforces `exp`.
        let verify_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut crypto = host_bridge::CryptoBridge::new();
                crypto
                    .call(
                        "verify_jwt",
                        serde_json::json!({
                            "token": token,
                            "secret": secret
                        }),
                    )
                    .await
            })
        });

        let payload = match verify_result {
            Ok(v) if v["ok"] == true => v["data"].clone(),
            _ => return write_string_to_caller(&mut caller, ""),
        };

        let mut payload_obj = match payload {
            serde_json::Value::Object(map) => map,
            _ => return write_string_to_caller(&mut caller, ""),
        };

        // 2. Extract `jti`. A refresh token without a jti cannot be tracked
        // for replay; reject rather than issue a rotation the framework
        // cannot enforce single-use on.
        let old_jti = match payload_obj.get("jti").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                warn!("_jwt_refresh_and_rotate: token has no jti claim; refusing rotation");
                return write_string_to_caller(&mut caller, "");
            }
        };

        // 3. Compute remaining lifetime so the revocation entry lasts at
        // least as long as the original token would have been valid.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let old_exp = payload_obj.get("exp").and_then(|v| v.as_i64()).unwrap_or(0);
        let remaining = (old_exp - now_secs).max(0) as u64;
        // Cap the revocation entry lifetime so a maliciously long-lived
        // token cannot pin the entry in memory forever.
        let revoke_ttl = std::time::Duration::from_secs(remaining.min(30 * 24 * 3600));

        // 4. Atomically check-and-consume the old jti under the write lock.
        let session_store = caller.data().session_store.clone();
        let first_use = {
            let mut store = session_store.write().expect("session store lock poisoned");
            store.mark_jti_consumed(&old_jti, revoke_ttl)
        };
        if !first_use {
            // Replay attempt (AUTH-J009). Do not issue a new token.
            warn!(
                "_jwt_refresh_and_rotate: replay of consumed jti={}",
                old_jti
            );
            return write_string_to_caller(&mut caller, "");
        }

        // 5. Build the new payload: keep original claims, refresh iat/exp/jti.
        let new_jti = uuid::Uuid::new_v4().to_string();
        let new_exp = now_secs.saturating_add(new_ttl_seconds as i64);
        payload_obj.insert("iat".to_string(), serde_json::Value::from(now_secs));
        payload_obj.insert("exp".to_string(), serde_json::Value::from(new_exp));
        payload_obj.insert("jti".to_string(), serde_json::Value::from(new_jti));

        let new_payload = serde_json::Value::Object(payload_obj);

        // 6. Sign the new token.
        let sign_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut crypto = host_bridge::CryptoBridge::new();
                crypto
                    .call(
                        "sign",
                        serde_json::json!({
                            "payload": new_payload,
                            "secret": secret,
                            "algorithm": algorithm
                        }),
                    )
                    .await
            })
        });

        let new_token = match sign_result {
            Ok(v) if v["ok"] == true => v["data"].as_str().unwrap_or("").to_string(),
            _ => String::new(),
        };

        write_string_to_caller(&mut caller, &new_token)
    });

    Ok(())
}

/// Replace the `Max-Age=<n>` directive in a Set-Cookie string with a new value.
/// If no `Max-Age` is present, append one. Used by `_session_create_with_ttl`
/// and `_session_extend` to override the session-store-default lifetime.
fn replace_cookie_max_age(cookie: &str, ttl_seconds: u64) -> String {
    let needle_lower = "max-age=";
    let lower = cookie.to_ascii_lowercase();
    if let Some(start) = lower.find(needle_lower) {
        // Find the end of the existing Max-Age value (next `;` or end of string).
        let val_start = start + needle_lower.len();
        let end = cookie[val_start..]
            .find([';', ' '])
            .map(|off| val_start + off)
            .unwrap_or(cookie.len());
        let mut out = String::with_capacity(cookie.len() + 8);
        out.push_str(&cookie[..val_start]);
        out.push_str(&ttl_seconds.to_string());
        out.push_str(&cookie[end..]);
        out
    } else {
        format!("{}; Max-Age={}", cookie.trim_end_matches(';'), ttl_seconds)
    }
}

/// Register response manipulation functions (_res_set_header, _res_redirect)
fn register_response_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _http_respond - Send an HTTP response with status, content type, and body
    // Args: status, content_type_ptr, content_type_len, body_ptr, body_len
    // Returns: pointer to body (for chaining)
    linker
        .func_wrap(
            "env",
            "_http_respond",
            |mut caller: Caller<'_, WasmState>,
             status: i32,
             content_type_ptr: i32,
             content_type_len: i32,
             body_ptr: i32,
             body_len: i32|
             -> i32 {
                let content_type = read_raw_string(&mut caller, content_type_ptr, content_type_len)
                    .unwrap_or_else(|| "text/plain".to_string());
                let body = read_raw_string(&mut caller, body_ptr, body_len).unwrap_or_default();

                debug!(
                    "_http_respond: status={}, content_type={}, body_len={}",
                    status,
                    content_type,
                    body.len()
                );

                // Set response properties
                let state = caller.data_mut();
                state.set_status(status as u16);
                state.add_header("Content-Type".to_string(), content_type);
                state.set_body(body.clone());

                // Return pointer to body for chaining
                write_string_to_caller(&mut caller, &body)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_respond: {}", e)))?;

    // _http_redirect - Send an HTTP redirect (alternative signature)
    // Args: status, url_ptr, url_len
    // Returns: pointer to url
    linker
        .func_wrap(
            "env",
            "_http_redirect",
            |mut caller: Caller<'_, WasmState>, status: i32, url_ptr: i32, url_len: i32| -> i32 {
                let url = match read_raw_string(&mut caller, url_ptr, url_len) {
                    Some(s) => s,
                    None => return write_string_to_caller(&mut caller, ""),
                };

                debug!("_http_redirect: status={}, url={}", status, url);

                caller.data_mut().set_redirect(status as u16, url.clone());

                write_string_to_caller(&mut caller, &url)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_redirect: {}", e)))?;

    // _http_set_header - fire-and-forget header setter
    // Args: name_ptr, name_len, value_ptr, value_len
    // Returns: void (matches function-registry.toml)
    linker
        .func_wrap(
            "env",
            "_http_set_header",
            |mut caller: Caller<'_, WasmState>,
             name_ptr: i32,
             name_len: i32,
             value_ptr: i32,
             value_len: i32| {
                let header_name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s,
                    None => return,
                };
                let header_value =
                    read_raw_string(&mut caller, value_ptr, value_len).unwrap_or_default();

                debug!("_http_set_header: {}={}", header_name, header_value);
                caller.data_mut().add_header(header_name, header_value);
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_set_header: {}", e)))?;

    // _res_set_header - Set a custom response header
    // Args: name_ptr, name_len, value_ptr, value_len
    // Returns: 1 on success, 0 on error
    linker
        .func_wrap(
            "env",
            "_res_set_header",
            |mut caller: Caller<'_, WasmState>,
             name_ptr: i32,
             name_len: i32,
             value_ptr: i32,
             value_len: i32|
             -> i32 {
                if !check_bridge_permission(&caller, "_res_set_header") {
                    return 0;
                }
                let header_name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s,
                    None => {
                        error!("_res_set_header: Failed to read header name");
                        return 0;
                    }
                };

                let header_value = match read_raw_string(&mut caller, value_ptr, value_len) {
                    Some(s) => s,
                    None => {
                        error!("_res_set_header: Failed to read header value");
                        return 0;
                    }
                };

                debug!("_res_set_header: {}={}", header_name, header_value);
                caller.data_mut().add_header(header_name, header_value);
                1
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _res_set_header: {}", e)))?;

    // _res_redirect - Set a redirect response
    // Args: url_ptr, url_len, status_code (301, 302, 307, 308)
    // Returns: 1 on success, 0 on error
    // Note: Status codes:
    //   301 = Moved Permanently (cacheable, may change method to GET)
    //   302 = Found (temporary, may change method to GET)
    //   307 = Temporary Redirect (preserves method)
    //   308 = Permanent Redirect (preserves method)
    linker
        .func_wrap(
            "env",
            "_res_redirect",
            |mut caller: Caller<'_, WasmState>,
             url_ptr: i32,
             url_len: i32,
             status_code: i32|
             -> i32 {
                if !check_bridge_permission(&caller, "_res_redirect") {
                    return 0;
                }
                let url = match read_raw_string(&mut caller, url_ptr, url_len) {
                    Some(s) => s,
                    None => {
                        error!("_res_redirect: Failed to read URL");
                        return 0;
                    }
                };

                // Validate status code
                let status = match status_code {
                    301 | 302 | 303 | 307 | 308 => status_code as u16,
                    _ => {
                        debug!(
                            "_res_redirect: Invalid status code {}, defaulting to 302",
                            status_code
                        );
                        302
                    }
                };

                info!("_res_redirect: {} -> {}", status, url);
                caller.data_mut().set_redirect(status, url);
                1
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _res_redirect: {}", e)))?;

    // _res_status - Set response status code
    // Args: code (i32)
    linker
        .func_wrap(
            "env",
            "_res_status",
            |mut caller: Caller<'_, WasmState>, code: i32| {
                debug!("_res_status: {}", code);
                caller.data_mut().set_status(code as u16);
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _res_status: {}", e)))?;

    // _res_body - Set response body
    // Args: body_ptr, body_len
    linker
        .func_wrap(
            "env",
            "_res_body",
            |mut caller: Caller<'_, WasmState>, body_ptr: i32, body_len: i32| {
                let body = read_raw_string(&mut caller, body_ptr, body_len).unwrap_or_default();
                debug!("_res_body: {} bytes", body.len());
                caller.data_mut().set_body(body);
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _res_body: {}", e)))?;

    // _res_json - Set JSON response (sets body + Content-Type header)
    // Args: json_ptr, json_len
    linker
        .func_wrap(
            "env",
            "_res_json",
            |mut caller: Caller<'_, WasmState>, json_ptr: i32, json_len: i32| {
                let json_body =
                    read_raw_string(&mut caller, json_ptr, json_len).unwrap_or_default();
                debug!("_res_json: {} bytes", json_body.len());
                caller
                    .data_mut()
                    .add_header("Content-Type".to_string(), "application/json".to_string());
                caller.data_mut().set_body(json_body);
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _res_json: {}", e)))?;

    // _http_set_cache - Set Cache-Control max-age header
    linker
        .func_wrap(
            "env",
            "_http_set_cache",
            |mut caller: Caller<'_, WasmState>, max_age: i32| -> i32 {
                let cache_value = if max_age > 0 {
                    format!("public, max-age={}", max_age)
                } else {
                    "no-cache, no-store, must-revalidate".to_string()
                };

                debug!("_http_set_cache: {}", cache_value);
                caller
                    .data_mut()
                    .add_header("Cache-Control".to_string(), cache_value);
                1
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_set_cache: {}", e)))?;

    // _http_no_cache - Disable caching completely
    linker
        .func_wrap(
            "env",
            "_http_no_cache",
            |mut caller: Caller<'_, WasmState>| -> i32 {
                debug!("_http_no_cache: Disabling cache");
                caller.data_mut().add_header(
                    "Cache-Control".to_string(),
                    "no-cache, no-store, must-revalidate".to_string(),
                );
                caller
                    .data_mut()
                    .add_header("Pragma".to_string(), "no-cache".to_string());
                caller
                    .data_mut()
                    .add_header("Expires".to_string(), "0".to_string());
                1
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_no_cache: {}", e)))?;

    // _res_download - Set Content-Disposition: attachment header for file downloads
    // Args: filename_ptr, filename_len
    // Returns: void
    register_bridge_fn!(
        linker,
        "_res_download",
        |mut caller: Caller<'_, WasmState>, filename_ptr: i32, filename_len: i32| {
            let filename =
                read_raw_string(&mut caller, filename_ptr, filename_len).unwrap_or_default();
            let value = if filename.is_empty() {
                "attachment".to_string()
            } else {
                format!("attachment; filename=\"{}\"", filename.replace('"', "\\\""))
            };
            debug!("_res_download: Content-Disposition: {}", value);
            caller
                .data_mut()
                .add_header("Content-Disposition".to_string(), value);
        }
    );

    // =========================================
    // PHASE 3 RESPONSE EXTRAS
    // =========================================

    // _http_set_status(i32) -> void  (companion to _res_status; no header touch)
    register_bridge_fn!(
        linker,
        "_http_set_status",
        |mut caller: Caller<'_, WasmState>, code: i32| {
            caller.data_mut().set_status(code as u16);
        }
    );

    // _http_set_body(string) -> void  (raw body, no Content-Type touch)
    register_bridge_fn!(
        linker,
        "_http_set_body",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let body = read_raw_string(&mut caller, p, l).unwrap_or_default();
            caller.data_mut().set_body(body);
        }
    );

    // _http_json(string) -> void  (body + Content-Type: application/json)
    register_bridge_fn!(
        linker,
        "_http_json",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let body = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let state = caller.data_mut();
            state.add_header("Content-Type".to_string(), "application/json".to_string());
            state.set_body(body);
        }
    );

    // _http_html(string) -> void  (body + Content-Type: text/html; charset=utf-8)
    register_bridge_fn!(
        linker,
        "_http_html",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let body = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let state = caller.data_mut();
            state.add_header(
                "Content-Type".to_string(),
                "text/html; charset=utf-8".to_string(),
            );
            state.set_body(body);
        }
    );

    // _http_text(string) -> void  (body + Content-Type: text/plain; charset=utf-8)
    register_bridge_fn!(
        linker,
        "_http_text",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let body = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let state = caller.data_mut();
            state.add_header(
                "Content-Type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            );
            state.set_body(body);
        }
    );

    // Status-coded JSON error helpers. All share the same {ok:false, err:{code, message}} shape.
    register_bridge_fn!(
        linker,
        "_http_not_found",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let msg = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let body = err_body("NOT_FOUND", &msg);
            let state = caller.data_mut();
            state.set_status(404);
            state.add_header("Content-Type".to_string(), "application/json".to_string());
            state.set_body(body);
        }
    );

    register_bridge_fn!(
        linker,
        "_http_bad_request",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let msg = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let body = err_body("BAD_REQUEST", &msg);
            let state = caller.data_mut();
            state.set_status(400);
            state.add_header("Content-Type".to_string(), "application/json".to_string());
            state.set_body(body);
        }
    );

    register_bridge_fn!(
        linker,
        "_http_unauthorized",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let msg = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let body = err_body("AUTH_ERROR", &msg);
            let state = caller.data_mut();
            state.set_status(401);
            state.add_header("Content-Type".to_string(), "application/json".to_string());
            state.set_body(body);
        }
    );

    register_bridge_fn!(
        linker,
        "_http_forbidden",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let msg = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let body = err_body("PERMISSION_DENIED", &msg);
            let state = caller.data_mut();
            state.set_status(403);
            state.add_header("Content-Type".to_string(), "application/json".to_string());
            state.set_body(body);
        }
    );

    register_bridge_fn!(
        linker,
        "_http_server_error",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let msg = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let body = err_body("INTERNAL_ERROR", &msg);
            let state = caller.data_mut();
            state.set_status(500);
            state.add_header("Content-Type".to_string(), "application/json".to_string());
            state.set_body(body);
        }
    );

    // _http_delete_cookie(name) -> void — clear cookie via max-age=0
    register_bridge_fn!(
        linker,
        "_http_delete_cookie",
        |mut caller: Caller<'_, WasmState>, p: i32, l: i32| {
            let name = read_raw_string(&mut caller, p, l).unwrap_or_default();
            if name.is_empty() {
                return;
            }
            let cookie = format!("{}=; Path=/; Max-Age=0; HttpOnly", name);
            caller.data_mut().pending_set_cookie = Some(cookie);
        }
    );

    Ok(())
}

/// Read a boxed Any struct at `ptr`. Layout: `[tag@0:i32][value1@4:i32][value2@8:i32]`.
/// Tag values match `AnyTypeTag` in the compiler: 0=Null, 1=Integer, 2=Boolean,
/// 3=Number, 4=String, 5=List, 6=Object.
fn read_boxed_any(caller: &mut Caller<'_, WasmState>, ptr: i32) -> Option<(i32, i32, i32)> {
    let memory = caller.get_export("memory").and_then(|e| e.into_memory())?;
    let data = memory.data(&*caller);
    let p = ptr as usize;
    if p + 12 > data.len() {
        error!(
            "read_boxed_any: ptr {} out of bounds (memory size: {})",
            ptr,
            data.len()
        );
        return None;
    }
    let tag = i32::from_le_bytes(data[p..p + 4].try_into().ok()?);
    let v1 = i32::from_le_bytes(data[p + 4..p + 8].try_into().ok()?);
    let v2 = i32::from_le_bytes(data[p + 8..p + 12].try_into().ok()?);
    Some((tag, v1, v2))
}

/// Allocate a 12-byte boxed Any of tag=String (4) whose value1 points at the
/// given length-prefixed string pointer, returning the box pointer.
///
/// Uses the WASM module's exported `malloc` so the allocation is coordinated
/// with the guest heap (same convention as `write_bytes_to_caller`).
fn write_boxed_any_string(caller: &mut Caller<'_, WasmState>, s: &str) -> i32 {
    // First allocate the length-prefixed string payload.
    let str_ptr = write_string_to_caller(caller, s);
    if str_ptr == 0 {
        return 0;
    }

    // Allocate the 12-byte box via WASM malloc.
    let malloc = match caller.get_export("malloc").and_then(|e| e.into_func()) {
        Some(f) => f,
        None => {
            error!("write_boxed_any_string: no malloc export found");
            return 0;
        }
    };
    let typed_malloc = match malloc.typed::<i32, i32>(&*caller) {
        Ok(t) => t,
        Err(e) => {
            error!("write_boxed_any_string: malloc type mismatch: {}", e);
            return 0;
        }
    };
    let box_ptr = match typed_malloc.call(&mut *caller, 12) {
        Ok(p) if p > 0 => p,
        Ok(p) => {
            error!("write_boxed_any_string: malloc returned invalid ptr {}", p);
            return 0;
        }
        Err(e) => {
            error!("write_boxed_any_string: malloc call failed: {}", e);
            return 0;
        }
    };

    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => {
            error!("write_boxed_any_string: no memory export found");
            return 0;
        }
    };

    // Write [tag=4 (String), value1=str_ptr, value2=0].
    let mut buf = [0u8; 12];
    buf[0..4].copy_from_slice(&4i32.to_le_bytes());
    buf[4..8].copy_from_slice(&str_ptr.to_le_bytes());
    buf[8..12].copy_from_slice(&0i32.to_le_bytes());
    if let Err(e) = memory.write(&mut *caller, box_ptr as usize, &buf) {
        error!("write_boxed_any_string: failed to write box: {}", e);
        return 0;
    }
    box_ptr
}

/// Register JSON encode/decode/query functions (_json_encode, _json_decode, _json_get)
fn register_json_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _json_encode - Serialize value to JSON string
    linker
        .func_wrap(
            "env",
            "_json_encode",
            |mut caller: Caller<'_, WasmState>, value_ptr: i32, value_len: i32| -> i32 {
                let value = match read_raw_string(&mut caller, value_ptr, value_len) {
                    Some(s) => s,
                    None => return write_string_to_caller(&mut caller, "null"),
                };

                // Try to parse as JSON first to validate and re-serialize
                match serde_json::from_str::<serde_json::Value>(&value) {
                    Ok(json_value) => {
                        let encoded = serde_json::to_string(&json_value)
                            .unwrap_or_else(|_| "null".to_string());
                        debug!("_json_encode: encoded {} bytes", encoded.len());
                        write_string_to_caller(&mut caller, &encoded)
                    }
                    Err(_) => {
                        // If not valid JSON, treat as a string value and encode it
                        let json_str = serde_json::Value::String(value);
                        let encoded =
                            serde_json::to_string(&json_str).unwrap_or_else(|_| "\"\"".to_string());
                        debug!("_json_encode: encoded string as JSON");
                        write_string_to_caller(&mut caller, &encoded)
                    }
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _json_encode: {}", e)))?;

    // _json_decode - Parse JSON string to value
    linker
        .func_wrap(
            "env",
            "_json_decode",
            |mut caller: Caller<'_, WasmState>, json_ptr: i32, json_len: i32| -> i32 {
                let json_str = match read_raw_string(&mut caller, json_ptr, json_len) {
                    Some(s) => s,
                    None => return write_string_to_caller(&mut caller, "null"),
                };

                match serde_json::from_str::<serde_json::Value>(&json_str) {
                    Ok(value) => {
                        let decoded =
                            serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
                        debug!("_json_decode: decoded {} bytes", json_str.len());
                        write_string_to_caller(&mut caller, &decoded)
                    }
                    Err(e) => {
                        error!("_json_decode: parse error: {}", e);
                        let error_json = serde_json::json!({
                            "error": "JSON parse error",
                            "message": e.to_string()
                        })
                        .to_string();
                        write_string_to_caller(&mut caller, &error_json)
                    }
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _json_decode: {}", e)))?;

    // _json_get - Extract value from JSON by dot-separated path.
    //
    // Signature (frame.server v2.8.4+ / compiler 0.33.54+):
    //   _json_get(any: i32, path: i32) -> any (i32)
    //
    // The first parameter is a pointer to a boxed Any value
    // ([tag@0:i32][value1@4:i32][value2@8:i32]). Common shapes:
    //   - tag=4 (String):  value1 is a length-prefixed string pointer holding
    //                       the JSON payload (the case from `json.get("{}", k)`
    //                       and from `_db_query` results boxed as Any-string).
    //   - tag=0 (Null):    the source was null — return a null-boxed Any.
    //   - other tags:      treat value1 as an lp-str pointer as a best-effort
    //                       fallback so the runtime does not trap on unfamiliar
    //                       shapes. Parse failures return "" boxed as String.
    //
    // The second parameter is a bare length-prefixed string pointer (single
    // i32) holding the dot-separated path. Returns a boxed Any whose tag is
    // String (4) and whose value1 points at the extracted value, serialised
    // as a string (matching the previous ptr-returning behaviour so downstream
    // string coercion continues to work).
    linker
        .func_wrap(
            "env",
            "_json_get",
            |mut caller: Caller<'_, WasmState>, any_ptr: i32, path_ptr: i32| -> i32 {
                // DIAG-JSON-GET-ENTRY: emitted UNCONDITIONALLY at closure entry
                // so we can prove the host bridge is even being called. Prompt
                // eafc9b47 reports zero DIAG-JSON-GET lines in real logs while
                // db.query envelopes are demonstrably being parsed — pointing
                // to either (a) the emit at the success path is unreachable
                // because every real call takes an early-return branch, or
                // (b) _json_get is never being called and json.get is going
                // through a different path (compiler stdlib json.decode).
                // This entry log fires no matter what happens inside.
                info!(
                    "DIAG-JSON-GET-ENTRY any_ptr={} path_ptr={}",
                    any_ptr, path_ptr
                );

                // Wire-format detection. Prompt f2c316ce reports every db.query
                // result being unreadable via json.get in clean-server 1.9.99+.
                // Root cause: _db_query returns a BARE length-prefixed string
                // pointer, but _json_get expects a 12-byte boxed-Any struct
                // ([tag@0][value1@4][value2@8]). If the compiler emits code
                // that forwards the bare pointer without boxing, we read the
                // first 4 bytes of the JSON as `tag` and the next 4 as
                // `value1` — both garbage, and the fallback path never sees
                // the actual JSON.
                //
                // Fix: try the spec-compliant boxed path first; if the payload
                // it points at is not parseable JSON, fall back to treating
                // any_ptr itself as a bare lp-string pointer. This rescues the
                // drift case while keeping the boxed path unchanged for callers
                // that box correctly.
                let (tag, value1, _value2) = match read_boxed_any(&mut caller, any_ptr) {
                    Some(t) => t,
                    None => return write_boxed_any_string(&mut caller, ""),
                };

                if tag == 0 {
                    debug!("_json_get: source is Null-boxed, returning empty");
                    return write_boxed_any_string(&mut caller, "");
                }

                let boxed_str = read_string_from_caller(&mut caller, value1);
                let bare_str = read_string_from_caller(&mut caller, any_ptr);

                let json_str = match (boxed_str, bare_str) {
                    (Some(s), _) if serde_json::from_str::<serde_json::Value>(&s).is_ok() => s,
                    (_, Some(s)) if serde_json::from_str::<serde_json::Value>(&s).is_ok() => {
                        debug!(
                            "_json_get: any_ptr was a bare lp-string pointer, not a boxed Any \
                             (tag={} did not resolve to JSON via value1={}). Falling back to \
                             bare-pointer wire format. See prompt f2c316ce.",
                            tag, value1
                        );
                        s
                    }
                    _ => {
                        debug!(
                            "_json_get: could not read source string as either boxed-Any or \
                             bare lp-string (tag={}, value1={}, any_ptr={})",
                            tag, value1, any_ptr
                        );
                        return write_boxed_any_string(&mut caller, "");
                    }
                };

                let path = match read_string_from_caller(&mut caller, path_ptr) {
                    Some(s) => s,
                    None => return write_boxed_any_string(&mut caller, ""),
                };

                let parsed: serde_json::Value = match serde_json::from_str(&json_str) {
                    Ok(v) => v,
                    Err(_) => return write_boxed_any_string(&mut caller, ""),
                };

                let mut current = &parsed;
                for part in path.split('.') {
                    // Support both numeric array indices (e.g. "rows.0.slug") and object keys
                    let next = if let Ok(idx) = part.parse::<usize>() {
                        current.get(idx)
                    } else {
                        current.get(part)
                    };
                    match next {
                        Some(v) => current = v,
                        None => return write_boxed_any_string(&mut caller, ""),
                    }
                }

                let result = match current {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    other => other.to_string(),
                };

                // DIAG-JSON-GET: log the tuple (tag, path, source_bytes, result)
                // at info level so we can reconcile _db_query envelope shape
                // against _json_get traversal without RUST_LOG=debug. Companion
                // to DIAG-DB-QUERY-SHAPE. See prompt 1c2d7544.
                info!(
                    "DIAG-JSON-GET tag={} path={:?} source_bytes={} source={} result={:?}",
                    tag,
                    path,
                    json_str.len(),
                    if json_str.len() > 200 {
                        format!("{}...", &json_str[..200])
                    } else {
                        json_str.clone()
                    },
                    result
                );
                debug!("_json_get: path='{}' -> '{}'", path, result);
                write_boxed_any_string(&mut caller, &result)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _json_get: {}", e)))?;

    // ------------------------------------------------------------------
    // V2 bridges (Delivery-2 / [P3a-reissue-2])
    //
    // These operate on the compiler's boxed-Any wire format per
    // foundation/spec/platform/BOXED_ANY_ABI.md (§3.2 Array, §3.3 Object).
    // Container fragments are JSON-tree layouts (4-byte count header, then
    // count × 4-byte pointers for Arrays / count × 8-byte (key,val) pairs
    // for Objects) — NOT Clean list/pairs structures.
    //
    // Byte-for-byte reference: clean-language-compiler/src/stdlib/json_class.rs
    //   __json_from_cln_list  (lines 4056–4104)
    //   __json_from_cln_pairs (lines 4262–4297)
    //
    // Legacy _json_encode / _json_decode / _json_get above are unchanged for
    // frame.server 2.9.5 backward compatibility (dropped in [P5]).
    // ------------------------------------------------------------------
    linker
        .func_wrap(
            "env",
            "_json_encode_v2",
            |mut caller: Caller<'_, WasmState>, boxed_ptr: i32| -> i32 {
                let value = match read_boxed_any_recursive(&mut caller, boxed_ptr) {
                    Ok(v) => v,
                    Err(e) => {
                        // D5: malformed boxed-Any traps (panic → wasmtime Trap).
                        panic!("_json_encode_v2: malformed boxed-Any: {}", e);
                    }
                };
                let encoded = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
                write_string_to_caller(&mut caller, &encoded)
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _json_encode_v2: {}", e)))?;

    linker
        .func_wrap(
            "env",
            "_json_encode_pretty_v2",
            |mut caller: Caller<'_, WasmState>, boxed_ptr: i32| -> i32 {
                let value = match read_boxed_any_recursive(&mut caller, boxed_ptr) {
                    Ok(v) => v,
                    Err(e) => {
                        panic!("_json_encode_pretty_v2: malformed boxed-Any: {}", e);
                    }
                };
                let encoded =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string());
                write_string_to_caller(&mut caller, &encoded)
            },
        )
        .map_err(|e| {
            RuntimeError::wasm(format!("Failed to define _json_encode_pretty_v2: {}", e))
        })?;

    linker
        .func_wrap(
            "env",
            "_json_decode_v2",
            |mut caller: Caller<'_, WasmState>, bytes_ptr: i32, len: i32| -> i32 {
                let json_str = match read_raw_string(&mut caller, bytes_ptr, len) {
                    Some(s) => s,
                    None => return 0, // sentinel: parse failure per BOXED_ANY_ABI §6
                };
                let value: serde_json::Value = match serde_json::from_str(&json_str) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("_json_decode_v2: parse error: {}", e);
                        return 0;
                    }
                };
                match write_boxed_any_recursive(&mut caller, &value) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("_json_decode_v2: allocation failure: {}", e);
                        0
                    }
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _json_decode_v2: {}", e)))?;

    Ok(())
}

// ============================================================================
// Boxed-Any ↔ serde_json::Value marshalling for v2 JSON bridges.
// See foundation/spec/platform/BOXED_ANY_ABI.md §3 for the wire format.
// ============================================================================

/// Call the WASM module's exported `malloc(size: i32) -> i32` and return the
/// allocated offset. Returns Err on any failure so the caller can propagate.
fn wasm_malloc(caller: &mut Caller<'_, WasmState>, size: i32) -> Result<i32, String> {
    let malloc = caller
        .get_export("malloc")
        .and_then(|e| e.into_func())
        .ok_or_else(|| "no malloc export".to_string())?;
    let typed = malloc
        .typed::<i32, i32>(&*caller)
        .map_err(|e| format!("malloc type mismatch: {}", e))?;
    let ptr = typed
        .call(&mut *caller, size)
        .map_err(|e| format!("malloc call failed: {}", e))?;
    if ptr <= 0 {
        return Err(format!("malloc returned invalid ptr {}", ptr));
    }
    Ok(ptr)
}

/// Read a length-prefixed string at `ptr` (LP layout: 4-byte little-endian
/// length header followed by UTF-8 content).
fn read_lp_string(caller: &mut Caller<'_, WasmState>, ptr: i32) -> Result<String, String> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| "no memory export".to_string())?;
    let data = memory.data(&*caller);
    let p = ptr as usize;
    if p + 4 > data.len() {
        return Err(format!("LP string header out of bounds at {}", ptr));
    }
    let len = i32::from_le_bytes(
        data[p..p + 4]
            .try_into()
            .map_err(|_| "LP length read failed".to_string())?,
    ) as usize;
    if p + 4 + len > data.len() {
        return Err(format!("LP string content out of bounds at {}", ptr));
    }
    let bytes = &data[p + 4..p + 4 + len];
    String::from_utf8(bytes.to_vec()).map_err(|e| format!("LP string not valid UTF-8: {}", e))
}

/// Allocate an LP string in guest memory via WASM malloc and return its offset.
fn alloc_lp_string(caller: &mut Caller<'_, WasmState>, s: &str) -> Result<i32, String> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let ptr = wasm_malloc(caller, (4 + len) as i32)?;
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| "no memory export".to_string())?;
    let mut header = [0u8; 4];
    header.copy_from_slice(&(len as i32).to_le_bytes());
    memory
        .write(&mut *caller, ptr as usize, &header)
        .map_err(|e| format!("LP header write failed: {}", e))?;
    if len > 0 {
        memory
            .write(&mut *caller, ptr as usize + 4, bytes)
            .map_err(|e| format!("LP content write failed: {}", e))?;
    }
    Ok(ptr)
}

/// Read a 32-bit little-endian integer at absolute offset `off` in guest memory.
fn read_i32_at(caller: &mut Caller<'_, WasmState>, off: usize) -> Result<i32, String> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| "no memory export".to_string())?;
    let data = memory.data(&*caller);
    if off + 4 > data.len() {
        return Err(format!("i32 read out of bounds at {}", off));
    }
    Ok(i32::from_le_bytes(
        data[off..off + 4]
            .try_into()
            .map_err(|_| "i32 read failed".to_string())?,
    ))
}

/// Read 8 bytes of a 12-byte boxed-Any's value slot (offset 4..12) as a raw
/// buffer so callers can decode as i64 / f64 / (i32, i32) as needed.
fn read_boxed_value_bytes(
    caller: &mut Caller<'_, WasmState>,
    ptr: i32,
) -> Result<(i32, [u8; 8]), String> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| "no memory export".to_string())?;
    let data = memory.data(&*caller);
    let p = ptr as usize;
    if p + 12 > data.len() {
        return Err(format!("boxed-Any read out of bounds at {}", ptr));
    }
    let tag = i32::from_le_bytes(
        data[p..p + 4]
            .try_into()
            .map_err(|_| "tag read failed".to_string())?,
    );
    let mut val = [0u8; 8];
    val.copy_from_slice(&data[p + 4..p + 12]);
    Ok((tag, val))
}

/// Walk a boxed-Any tree rooted at `ptr` per BOXED_ANY_ABI §3 and produce a
/// `serde_json::Value`. Container payloads use the JSON-tree fragment layout
/// (§3.2 Array, §3.3 Object). Errors on invalid tags or OOB reads (caller maps
/// to a wasmtime trap per D5).
fn read_boxed_any_recursive(
    caller: &mut Caller<'_, WasmState>,
    ptr: i32,
) -> Result<serde_json::Value, String> {
    let (tag, val_bytes) = read_boxed_value_bytes(caller, ptr)?;
    match tag {
        0 => Ok(serde_json::Value::Null),
        1 => {
            // i64 split across offsets 4..12 (little-endian whole i64)
            let n = i64::from_le_bytes(val_bytes);
            Ok(serde_json::Value::Number(n.into()))
        }
        2 => {
            // Boolean at offset 4 (low 4 bytes)
            let b = i32::from_le_bytes([val_bytes[0], val_bytes[1], val_bytes[2], val_bytes[3]]);
            Ok(serde_json::Value::Bool(b != 0))
        }
        3 => {
            // f64 at offsets 4..12
            let f = f64::from_le_bytes(val_bytes);
            Ok(serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null))
        }
        4 => {
            // LP-string pointer at offset 4
            let str_ptr =
                i32::from_le_bytes([val_bytes[0], val_bytes[1], val_bytes[2], val_bytes[3]]);
            let s = read_lp_string(caller, str_ptr)?;
            Ok(serde_json::Value::String(s))
        }
        5 => {
            // JSON-tree Array pointer at offset 4 (§3.2)
            //   [count:i32][elem_ptr_0:i32][elem_ptr_1:i32]…
            let arr_ptr =
                i32::from_le_bytes([val_bytes[0], val_bytes[1], val_bytes[2], val_bytes[3]]);
            let count = read_i32_at(caller, arr_ptr as usize)?;
            if count < 0 {
                return Err(format!("negative Array count {}", count));
            }
            let mut items = Vec::with_capacity(count as usize);
            for i in 0..count {
                let slot = arr_ptr as usize + 4 + (i as usize) * 4;
                let elem_ptr = read_i32_at(caller, slot)?;
                items.push(read_boxed_any_recursive(caller, elem_ptr)?);
            }
            Ok(serde_json::Value::Array(items))
        }
        6 => {
            // JSON-tree Object pointer at offset 4 (§3.3)
            //   [count:i32] then count × [key_ptr:i32 (LP str), val_ptr:i32 (boxed)]
            let obj_ptr =
                i32::from_le_bytes([val_bytes[0], val_bytes[1], val_bytes[2], val_bytes[3]]);
            let count = read_i32_at(caller, obj_ptr as usize)?;
            if count < 0 {
                return Err(format!("negative Object count {}", count));
            }
            // preserve_order is enabled on serde_json (Cargo.toml) so insertion
            // order is retained per D3.
            let mut map = serde_json::Map::new();
            for i in 0..count {
                let entry_off = obj_ptr as usize + 4 + (i as usize) * 8;
                let key_ptr = read_i32_at(caller, entry_off)?;
                let val_ptr = read_i32_at(caller, entry_off + 4)?;
                let key = read_lp_string(caller, key_ptr)?;
                let val = read_boxed_any_recursive(caller, val_ptr)?;
                map.insert(key, val);
            }
            Ok(serde_json::Value::Object(map))
        }
        other => Err(format!("invalid boxed-Any tag {}", other)),
    }
}

/// Materialize a `serde_json::Value` into a freshly-allocated boxed-Any tree in
/// guest memory per BOXED_ANY_ABI §3 and return the root pointer. Container
/// payloads use the JSON-tree fragment layout to match __json_from_cln_list /
/// __json_from_cln_pairs byte-for-byte.
fn write_boxed_any_recursive(
    caller: &mut Caller<'_, WasmState>,
    value: &serde_json::Value,
) -> Result<i32, String> {
    // 12-byte outer box first, so children live at higher offsets when malloc
    // is a bump allocator. The order does not affect correctness; only
    // that the returned root points at a valid 12-byte block.
    let box_ptr = wasm_malloc(caller, 12)?;
    let mut buf = [0u8; 12];

    match value {
        serde_json::Value::Null => {
            buf[0..4].copy_from_slice(&0i32.to_le_bytes());
        }
        serde_json::Value::Bool(b) => {
            buf[0..4].copy_from_slice(&2i32.to_le_bytes());
            buf[4..8].copy_from_slice(&(if *b { 1i32 } else { 0i32 }).to_le_bytes());
        }
        serde_json::Value::Number(n) => {
            // D1: integers in JS-safe range (|n| ≤ 2^53) → tag 1; else tag 3.
            const JS_SAFE: i64 = 1i64 << 53;
            if let Some(i) = n.as_i64() {
                if i.abs() <= JS_SAFE {
                    buf[0..4].copy_from_slice(&1i32.to_le_bytes());
                    buf[4..12].copy_from_slice(&i.to_le_bytes());
                } else {
                    let f = i as f64;
                    buf[0..4].copy_from_slice(&3i32.to_le_bytes());
                    buf[4..12].copy_from_slice(&f.to_le_bytes());
                }
            } else if let Some(u) = n.as_u64() {
                if u <= JS_SAFE as u64 {
                    buf[0..4].copy_from_slice(&1i32.to_le_bytes());
                    buf[4..12].copy_from_slice(&(u as i64).to_le_bytes());
                } else {
                    let f = u as f64;
                    buf[0..4].copy_from_slice(&3i32.to_le_bytes());
                    buf[4..12].copy_from_slice(&f.to_le_bytes());
                }
            } else if let Some(f) = n.as_f64() {
                buf[0..4].copy_from_slice(&3i32.to_le_bytes());
                buf[4..12].copy_from_slice(&f.to_le_bytes());
            } else {
                return Err(format!("unhandled JSON number {:?}", n));
            }
        }
        serde_json::Value::String(s) => {
            let str_ptr = alloc_lp_string(caller, s)?;
            buf[0..4].copy_from_slice(&4i32.to_le_bytes());
            buf[4..8].copy_from_slice(&str_ptr.to_le_bytes());
        }
        serde_json::Value::Array(items) => {
            // First materialize every child boxed-Any so we know their pointers,
            // then allocate the JSON-tree Array (4 + count*4) and store them.
            let mut child_ptrs = Vec::with_capacity(items.len());
            for item in items {
                child_ptrs.push(write_boxed_any_recursive(caller, item)?);
            }
            let arr_size = 4 + items.len() as i32 * 4;
            let arr_ptr = wasm_malloc(caller, arr_size)?;
            let memory = caller
                .get_export("memory")
                .and_then(|e| e.into_memory())
                .ok_or_else(|| "no memory export".to_string())?;
            let mut arr_buf = vec![0u8; arr_size as usize];
            arr_buf[0..4].copy_from_slice(&(items.len() as i32).to_le_bytes());
            for (i, cp) in child_ptrs.iter().enumerate() {
                let off = 4 + i * 4;
                arr_buf[off..off + 4].copy_from_slice(&cp.to_le_bytes());
            }
            memory
                .write(&mut *caller, arr_ptr as usize, &arr_buf)
                .map_err(|e| format!("Array write failed: {}", e))?;
            buf[0..4].copy_from_slice(&5i32.to_le_bytes());
            buf[4..8].copy_from_slice(&arr_ptr.to_le_bytes());
        }
        serde_json::Value::Object(map) => {
            // Materialize keys and children up front to know pointers before
            // writing the entry table. Iteration order = insertion order (D3)
            // because serde_json is compiled with preserve_order.
            let mut entries: Vec<(i32, i32)> = Vec::with_capacity(map.len());
            for (k, v) in map.iter() {
                let key_ptr = alloc_lp_string(caller, k)?;
                let val_ptr = write_boxed_any_recursive(caller, v)?;
                entries.push((key_ptr, val_ptr));
            }
            let obj_size = 4 + map.len() as i32 * 8;
            let obj_ptr = wasm_malloc(caller, obj_size)?;
            let memory = caller
                .get_export("memory")
                .and_then(|e| e.into_memory())
                .ok_or_else(|| "no memory export".to_string())?;
            let mut obj_buf = vec![0u8; obj_size as usize];
            obj_buf[0..4].copy_from_slice(&(map.len() as i32).to_le_bytes());
            for (i, (kp, vp)) in entries.iter().enumerate() {
                let off = 4 + i * 8;
                obj_buf[off..off + 4].copy_from_slice(&kp.to_le_bytes());
                obj_buf[off + 4..off + 8].copy_from_slice(&vp.to_le_bytes());
            }
            memory
                .write(&mut *caller, obj_ptr as usize, &obj_buf)
                .map_err(|e| format!("Object write failed: {}", e))?;
            buf[0..4].copy_from_slice(&6i32.to_le_bytes());
            buf[4..8].copy_from_slice(&obj_ptr.to_le_bytes());
        }
    }

    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| "no memory export".to_string())?;
    memory
        .write(&mut *caller, box_ptr as usize, &buf)
        .map_err(|e| format!("box write failed: {}", e))?;
    Ok(box_ptr)
}

/// Register island component functions (_island_register)
fn register_islands_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _island_register - Register an island component for client-side hydration
    //
    // Parameters:
    //   component_ptr/len - Component name identifier (e.g. "Counter")
    //   module_ptr/len    - URL path to the island's WASM module (e.g. "/islands/counter.wasm")
    //   hydration_ptr/len - Hydration strategy: "on" | "visible" | "idle" | "only"
    //
    // Returns: 0 on success, -1 if the hydration mode is invalid
    linker
        .func_wrap(
            "env",
            "_island_register",
            |mut caller: Caller<'_, WasmState>,
             component_ptr: i32,
             component_len: i32,
             module_ptr: i32,
             module_len: i32,
             hydration_ptr: i32,
             hydration_len: i32|
             -> i32 {
                let component = match read_raw_string(&mut caller, component_ptr, component_len) {
                    Some(s) if !s.is_empty() => s,
                    _ => {
                        error!("_island_register: Failed to read component name or component name is empty");
                        return -1;
                    }
                };

                let module_path = match read_raw_string(&mut caller, module_ptr, module_len) {
                    Some(s) if !s.is_empty() => s,
                    _ => {
                        error!("_island_register: Failed to read module path or module path is empty");
                        return -1;
                    }
                };

                let hydration = match read_raw_string(&mut caller, hydration_ptr, hydration_len) {
                    Some(s) => s,
                    None => {
                        error!("_island_register: Failed to read hydration mode");
                        return -1;
                    }
                };

                // Validate hydration strategy — only recognised modes are permitted
                match hydration.as_str() {
                    "on" | "visible" | "idle" | "only" => {}
                    other => {
                        error!(
                            "_island_register: Invalid hydration mode '{}' for component '{}'. \
                             Must be one of: on, visible, idle, only",
                            other, component
                        );
                        return -1;
                    }
                }

                info!(
                    "_island_register: component='{}', module='{}', hydration='{}'",
                    component, module_path, hydration
                );

                let entry = IslandEntry {
                    component,
                    module: module_path,
                    hydration,
                };

                let islands_store = caller.data().islands_store.clone();
                match islands_store.write() {
                    Ok(mut store) => {
                        store.islands.push(entry);
                        0 // success
                    }
                    Err(e) => {
                        error!("_island_register: Failed to acquire islands store lock: {}", e);
                        -1
                    }
                }
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _island_register: {}", e)))?;

    Ok(())
}

/// Register UI template bridge functions (_ui_load_layout, _ui_load_page, _ui_render_page, _ui_inject_head_link)
fn register_ui_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _ui_load_layout - Load an HTML layout file. Caller provides the full relative path
    // (e.g. "app/ui/layouts/main.html"). Path resolution is the caller's responsibility.
    register_bridge_fn!(linker, "_ui_load_layout", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    name_ptr: i32,
                                                    name_len: i32|
     -> i32 {
        let path_str = match read_raw_string(&mut caller, name_ptr, name_len) {
            Some(s) if !s.is_empty() => s,
            _ => {
                error!("_ui_load_layout: Failed to read path");
                return write_string_to_caller(&mut caller, "");
            }
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let path = cwd.join(&path_str);
        debug!("_ui_load_layout: loading {:?}", path);

        match std::fs::read_to_string(&path) {
            Ok(contents) => write_string_to_caller(&mut caller, &contents),
            Err(e) => {
                error!("_ui_load_layout: Failed to read '{:?}': {}", path, e);
                write_string_to_caller(&mut caller, "")
            }
        }
    });

    // _ui_load_page - Load an HTML page template. Caller provides the full relative path
    // (e.g. "app/ui/pages/index.html"). Path resolution is the caller's responsibility.
    register_bridge_fn!(linker, "_ui_load_page", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                  name_ptr: i32,
                                                  name_len: i32|
     -> i32 {
        let path_str = match read_raw_string(&mut caller, name_ptr, name_len) {
            Some(s) if !s.is_empty() => s,
            _ => {
                error!("_ui_load_page: Failed to read path");
                return write_string_to_caller(&mut caller, "");
            }
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let path = cwd.join(&path_str);
        debug!("_ui_load_page: loading {:?}", path);

        match std::fs::read_to_string(&path) {
            Ok(contents) => write_string_to_caller(&mut caller, &contents),
            Err(e) => {
                error!("_ui_load_page: Failed to read '{:?}': {}", path, e);
                write_string_to_caller(&mut caller, "")
            }
        }
    });

    // _ui_render_page - Render an HTML template with {{ key }} substitution.
    // Caller provides the full relative path (e.g. "app/ui/pages/index.html").
    // data is a JSON string; missing keys produce empty string.
    register_bridge_fn!(linker, "_ui_render_page", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    name_ptr: i32,
                                                    name_len: i32,
                                                    data_ptr: i32,
                                                    data_len: i32|
     -> i32 {
        let path_str = match read_raw_string(&mut caller, name_ptr, name_len) {
            Some(s) if !s.is_empty() => s,
            _ => {
                error!("_ui_render_page: Failed to read path");
                return write_string_to_caller(&mut caller, "");
            }
        };

        let data_str = read_raw_string(&mut caller, data_ptr, data_len).unwrap_or_default();

        let cwd = std::env::current_dir().unwrap_or_default();
        let path = cwd.join(&path_str);
        debug!("_ui_render_page: rendering {:?}", path);

        let template = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(e) => {
                error!("_ui_render_page: Failed to read '{:?}': {}", path, e);
                return write_string_to_caller(&mut caller, "");
            }
        };

        let data: serde_json::Value = if data_str.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&data_str).unwrap_or_else(|e| {
                error!(
                    "_ui_render_page: Failed to parse JSON data for '{}': {}",
                    path_str, e
                );
                serde_json::Value::Object(serde_json::Map::new())
            })
        };

        // Extract <page layout="..."> wrapper if present
        let (page_content, layout_name) = extract_page_layout(&template);

        // Evaluate server-side directives FIRST so that cl-iterate sees
        // the raw `{items}` collection in `data` (a top-level substitute
        // pass would stringify the array). Each directive expands its
        // body with a scoped data context where the iteration variable
        // is bound; remaining top-level `{var}` placeholders are then
        // resolved by the substitute pass below.
        let with_directives = process_directives(&page_content, &data);

        // Substitute remaining top-level template variables ({key} format)
        let substituted = substitute_template(&with_directives, &data);

        // Expand registered custom component tags to their server-side HTML
        let component_map = {
            let reg = caller.data().component_registry.clone();
            reg.read().map(|m| m.clone()).unwrap_or_default()
        };
        let with_components = expand_component_tags(&substituted, &component_map);

        // SRV001: v2 callback-based dispatch. If a plugin declared
        // `callback = { purpose = "component_tag_render", ... }` on
        // `_ui_render_page` in its plugin.toml, scan the HTML for
        // remaining custom (kebab-case) tags and substitute each one
        // with the output of the matching `<tagname>_render` export.
        // See foundation/spec/plugins/contracts/bridge-host-classes.md §4.1.
        let contract = {
            let cbs = caller.data().callbacks.clone();
            cbs.iter()
                .find(|c| {
                    c.bridge == "_ui_render_page"
                        && c.purpose
                            == crate::build_manifest::callback_purpose::COMPONENT_TAG_RENDER
                })
                .cloned()
        };
        let dispatched = match contract {
            // Only `exports_matching` is implemented here. The spec
            // (§4) reserves `manifest_lookup` and `explicit_argument`
            // for future discovery modes — handling them silently with
            // an exports_matching loop would mis-route their dispatch,
            // so skip with a warning instead.
            Some(c) if c.discovery == "exports_matching" => {
                dispatch_component_tags(&mut caller, &with_components, &c)
            }
            Some(c) => {
                warn!(
                    "_ui_render_page: unsupported callback discovery '{}' for purpose '{}' \
                         (declared by '{}'); skipping dispatch",
                    c.discovery, c.purpose, c.declared_by_plugin
                );
                with_components
            }
            None => with_components,
        };

        // Wrap in layout if specified
        let wrapped = match layout_name {
            Some(ref name) => apply_layout(&dispatched, name, &cwd),
            None => dispatched,
        };

        // SRV003: If the final document contains hydration islands, inject the
        // frame.ui loader.js <script> tag so the client runtime mounts components.
        let with_loader = inject_loader_script(&wrapped);

        // SRV004: Inject the full translation bundle as `window.__CLEAN_I18N__`
        // so the browser-side i18n runtime (frame.ui/runtime/loader.js) can
        // resolve translation keys without an additional network round-trip.
        // Reads are done under a blocking lock since this bridge function is
        // synchronous (not an async wasmtime func_wrap_async).
        let locale_state = caller.data().locale_state.clone();
        let i18n_json = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { locale_state.read().await.bundle_as_json() })
        });
        let rendered = match i18n_json {
            Some(json) => inject_i18n_bundle(&with_loader, &json),
            None => with_loader,
        };

        caller.data_mut().add_header(
            "Content-Type".to_string(),
            "text/html; charset=utf-8".to_string(),
        );
        write_string_to_caller(&mut caller, &rendered)
    });

    // _ui_inject_head_link - Inject <link rel="stylesheet" href="..."> into response <head>
    // Deduplicated: the same href injected multiple times produces a single <link> tag.
    register_bridge_fn!(linker, "_ui_inject_head_link", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                         href_ptr: i32,
                                                         href_len: i32|
     -> i32 {
        let href = match read_raw_string(&mut caller, href_ptr, href_len) {
            Some(s) => s,
            None => {
                error!("_ui_inject_head_link: Failed to read href string");
                return 0;
            }
        };

        let links = &mut caller.data_mut().pending_head_links;
        if !links.contains(&href) {
            links.push(href);
        }
        1
    });

    // _ui_register_component_html - Register a component's server-side HTML template.
    // Called during WASM init so that _ui_render_page can expand custom element tags.
    // First arg: hyphenated tag name (e.g. "my-widget").
    // Second arg: rendered HTML string for the component's default state.
    // Returns 1 on success, 0 on error.
    register_bridge_fn!(
        linker,
        "_ui_register_component_html",
        |mut caller: Caller<'_, WasmState>,
         tag_ptr: i32,
         tag_len: i32,
         html_ptr: i32,
         html_len: i32|
         -> i32 {
            let tag = match read_raw_string(&mut caller, tag_ptr, tag_len) {
                Some(s) if !s.is_empty() => s,
                _ => {
                    error!("_ui_register_component_html: empty or missing tag name");
                    return 0;
                }
            };
            let html = read_raw_string(&mut caller, html_ptr, html_len).unwrap_or_default();
            let registry = caller.data().component_registry.clone();
            match registry.write() {
                Ok(mut map) => {
                    info!("_ui_register_component_html: registered <{}>", tag);
                    map.insert(tag, html);
                    1
                }
                Err(e) => {
                    error!("_ui_register_component_html: registry lock poisoned: {}", e);
                    0
                }
            }
        }
    );

    Ok(())
}

/// Replace `{key}` tokens in `template` with values from `data` per the
/// `_ui_render_page` spec (`HOST_BRIDGE.md` §UI templates). Dotted paths
/// (`{user.name}`) are looked up against the JSON `data`. Missing paths
/// produce an empty string. To emit a literal brace in output, escape with
/// `{{` (renders as `{`) or `}}` (renders as `}`). Malformed placeholders
/// (empty key, newline inside braces) are preserved as literals.
fn substitute_template(template: &str, data: &serde_json::Value) -> String {
    // `{`, `}`, `\n`, `\r` are all ASCII, so byte-level scanning at char
    // boundaries is safe inside a `&str`: every ASCII byte is its own char.
    let bytes = template.as_bytes();
    let mut result = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'{' {
            // Escape: `{{` → literal `{`
            if bytes.get(i + 1) == Some(&b'{') {
                result.push('{');
                i += 2;
                continue;
            }
            // Find the matching close brace; reject nested `{` or newlines.
            let body_start = i + 1;
            let mut j = body_start;
            let mut bad = false;
            while j < bytes.len() {
                match bytes[j] {
                    b'}' => break,
                    b'{' | b'\n' | b'\r' => {
                        bad = true;
                        break;
                    }
                    _ => j += 1,
                }
            }
            if bad || j >= bytes.len() {
                // Unclosed or malformed — leave `{` as a literal, advance one ASCII byte.
                result.push('{');
                i += 1;
                continue;
            }
            let key = template[body_start..j].trim();
            if key.is_empty() {
                // `{}` or `{   }` is not a placeholder — preserve literally.
                result.push_str(&template[i..=j]);
                i = j + 1;
                continue;
            }
            let replacement = lookup_path(data, key)
                .map(json_value_to_string)
                .unwrap_or_default();
            result.push_str(&replacement);
            i = j + 1;
        } else if c == b'}' && bytes.get(i + 1) == Some(&b'}') {
            // Escape: `}}` → literal `}`
            result.push('}');
            i += 2;
        } else {
            // Non-meta byte: copy the full UTF-8 char starting here. Using
            // `chars().next()` works because `i` is always on a char boundary.
            let ch = template[i..]
                .chars()
                .next()
                .expect("non-empty by loop guard");
            let len = ch.len_utf8();
            result.push(ch);
            i += len;
        }
    }
    result
}

/// Look up a dot-separated path against the data. Returns None when any path
/// segment is missing. Indexing into arrays by numeric segment is supported
/// (`items.0.name`).
fn lookup_path<'a>(data: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = data;
    for part in path.split('.') {
        current = match current {
            serde_json::Value::Object(map) => map.get(part)?,
            serde_json::Value::Array(arr) => {
                let idx: usize = part.parse().ok()?;
                arr.get(idx)?
            }
            _ => return None,
        };
    }
    Some(current)
}

fn json_value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Replace custom component element tags with their registered server-side HTML.
///
/// For each `<tag-name [attrs]>...</tag-name>` where `tag-name` contains a hyphen:
/// - If registered: emit `<div data-island="tag-name" data-client="MODE">HTML</div>`
/// - If unregistered but has `client` attr: emit `<div data-island="tag-name" data-client="MODE"></div>`
///
/// Tags without a `client` attribute and without a registration are left unchanged.
fn expand_component_tags(
    html: &str,
    registry: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = html.to_string();
    let mut offset = 0;

    loop {
        // Find next '<' that starts a potential custom element (must contain a hyphen before '>')
        let remaining = &result[offset..];
        let rel_open = match remaining.find('<') {
            Some(pos) => pos,
            None => break,
        };
        let abs_open = offset + rel_open;

        // Extract tag name: must contain '-' (custom element convention)
        let after_open = &result[abs_open + 1..];
        let tag_end_in_name = after_open.find([' ', '>', '/']);
        let tag_name = match tag_end_in_name {
            Some(n) => after_open[..n].trim().to_string(),
            None => {
                offset = abs_open + 1;
                continue;
            }
        };

        if !tag_name.contains('-') || tag_name.starts_with('/') {
            offset = abs_open + 1;
            continue;
        }

        // Find end of opening tag
        let close_bracket = match result[abs_open..].find('>') {
            Some(pos) => abs_open + pos,
            None => {
                offset = abs_open + 1;
                continue;
            }
        };

        let opening_tag = &result[abs_open..=close_bracket];
        let self_closing = opening_tag.ends_with("/>");

        // Extract `client` attribute value
        let client_val = extract_attr_value_from_tag(opening_tag, "client");

        // Find closing tag if not self-closing
        let close_tag = format!("</{}>", tag_name);
        let (inner_html, element_end) = if self_closing {
            (String::new(), close_bracket + 1)
        } else {
            match result[close_bracket + 1..].find(close_tag.as_str()) {
                Some(rel) => {
                    let inner_start = close_bracket + 1;
                    let inner_end = inner_start + rel;
                    let end = inner_end + close_tag.len();
                    (result[inner_start..inner_end].to_string(), end)
                }
                None => {
                    offset = abs_open + 1;
                    continue;
                }
            }
        };

        // Decide what to emit
        let replacement = if let Some(registered_html) = registry.get(&tag_name) {
            let mode = client_val.as_deref().unwrap_or("on");
            format!(
                "<div data-island=\"{}\" data-client=\"{}\">{}</div>",
                tag_name, mode, registered_html
            )
        } else if let Some(mode) = client_val {
            // Unregistered but has client attr — emit island wrapper with inner content
            format!(
                "<div data-island=\"{}\" data-client=\"{}\">{}</div>",
                tag_name, mode, inner_html
            )
        } else {
            // No registration, no client attr — leave unchanged
            offset = abs_open + 1;
            continue;
        };

        result.replace_range(abs_open..element_end, &replacement);
        offset = abs_open + replacement.len();
    }

    result
}

/// Resolve a custom HTML tag name to the WASM export name declared by the
/// callback's `export_pattern`. The pattern's `{tagname}` placeholder is
/// substituted with the tag name with hyphens removed — matching the
/// compiler's convention for `<tagname>_render` exports
/// (see contracts/bridge-host-classes.md §4 + frame.ui plugin.toml).
fn resolve_render_export_name(tag_name: &str, pattern: &str) -> String {
    let normalized = tag_name.replace('-', "");
    pattern.replace("{tagname}", &normalized)
}

/// Build a JSON object string from the attributes inside an opening tag —
/// the argument a `component_tag_render` export receives.
fn marshal_attrs_as_json(opening_tag: &str, tag_name: &str) -> String {
    // Strip `<tag_name` prefix and trailing `>` (or `/>`)
    let after_name = match opening_tag.find(tag_name) {
        Some(p) => &opening_tag[p + tag_name.len()..],
        None => opening_tag,
    };
    let attrs_section = after_name
        .trim_start()
        .trim_end_matches('>')
        .trim_end_matches('/')
        .trim();
    if attrs_section.is_empty() {
        return "{}".to_string();
    }

    let mut map = serde_json::Map::new();
    let bytes = attrs_section.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Read attribute name (up to '=', whitespace, or end)
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = String::from_utf8_lossy(&bytes[name_start..i]).to_string();
        // Read optional value
        let value = if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let val_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                let v = String::from_utf8_lossy(&bytes[val_start..i]).to_string();
                if i < bytes.len() {
                    i += 1;
                }
                v
            } else {
                let val_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                String::from_utf8_lossy(&bytes[val_start..i]).to_string()
            }
        } else {
            // HTML boolean attribute — empty string per the prompt convention
            String::new()
        };
        if !name.is_empty() {
            map.insert(name, serde_json::Value::String(value));
        }
    }
    serde_json::Value::Object(map).to_string()
}

/// Scan `html` for custom (kebab-case) element tags and dispatch each one to
/// the matching `<tagname>_render` export, substituting the tag with the
/// returned HTML.
///
/// Implements `purpose = "component_tag_render"` from
/// `foundation/spec/plugins/contracts/bridge-host-classes.md` §4.1.
///
/// Resolution rules:
/// - Custom = the tag name contains a hyphen (HTML custom-element convention).
/// - The export name is built from the contract's `export_pattern` (default
///   `"{tagname}_render"`) with hyphens stripped from the tag name.
/// - If the export exists with signature `(i32) -> i32`, call it with the
///   tag's attributes JSON-encoded as a Clean string (length-prefixed
///   pointer); the result is a length-prefixed UTF-8 string pointer.
/// - If the export is missing OR has an unexpected signature OR traps, apply
///   the contract's `fallback`:
///   - `"passthrough"`: leave the original tag in place (default; matches
///     today's behavior — no regression).
///   - `"empty"`: substitute an empty string.
///   - `"error"`: substitute an HTML comment marker noting the failure.
///
/// Single-pass: the rendered HTML is NOT re-scanned, so a component whose
/// output contains another custom tag will not see that inner tag dispatched.
/// This is deliberate — multi-pass dispatch invites infinite loops and adds
/// per-request cost. Future revisions may add a bounded-depth iteration.
///
/// Attribute marshaling: the attributes are passed as one JSON object of
/// attribute name → string value (bridge-host-classes.md §4.1, "call the
/// matching export with the tag's attributes JSON-encoded").
fn dispatch_component_tags(
    caller: &mut Caller<'_, WasmState>,
    html: &str,
    contract: &crate::build_manifest::CallbackContract,
) -> String {
    let pattern = contract
        .export_pattern
        .as_deref()
        .unwrap_or("{tagname}_render");
    let fallback = contract.fallback.as_str();

    let mut result = html.to_string();
    let mut offset = 0;

    loop {
        let remaining = &result[offset..];
        let rel_open = match remaining.find('<') {
            Some(pos) => pos,
            None => break,
        };
        let abs_open = offset + rel_open;

        let after_open = &result[abs_open + 1..];
        let name_end = match after_open.find([' ', '>', '/', '\t', '\n']) {
            Some(n) => n,
            None => {
                offset = abs_open + 1;
                continue;
            }
        };
        let tag_name = after_open[..name_end].trim().to_string();

        // Skip closing tags, comments, DOCTYPE, processing instructions,
        // and non-custom tags (no hyphen).
        if tag_name.is_empty()
            || tag_name.starts_with('/')
            || tag_name.starts_with('!')
            || tag_name.starts_with('?')
            || !tag_name.contains('-')
        {
            offset = abs_open + 1;
            continue;
        }

        let close_bracket = match result[abs_open..].find('>') {
            Some(rel) => abs_open + rel,
            None => {
                offset = abs_open + 1;
                continue;
            }
        };
        let opening_tag = result[abs_open..=close_bracket].to_string();
        let self_closing = opening_tag.ends_with("/>");

        let close_tag = format!("</{}>", tag_name);
        let element_end = if self_closing {
            close_bracket + 1
        } else {
            match result[close_bracket + 1..].find(close_tag.as_str()) {
                Some(rel) => close_bracket + 1 + rel + close_tag.len(),
                None => {
                    // Unclosed custom tag — leave it alone.
                    offset = abs_open + 1;
                    continue;
                }
            }
        };

        let export_name = resolve_render_export_name(&tag_name, pattern);
        let attrs_json = marshal_attrs_as_json(&opening_tag, &tag_name);

        let func_opt = caller.get_export(&export_name).and_then(|e| e.into_func());
        let replacement: Option<String> = match func_opt {
            Some(func) => match func.typed::<i32, i32>(&*caller) {
                Ok(typed) => match {
                    let attrs_ptr = write_string_to_caller(caller, &attrs_json);
                    typed.call(&mut *caller, attrs_ptr)
                } {
                    Ok(lp_ptr) if lp_ptr > 0 => {
                        host_bridge::read_string_from_caller(caller, lp_ptr)
                    }
                    Ok(lp_ptr) => {
                        debug!(
                            "dispatch_component_tags: export '{}' returned invalid pointer {}",
                            export_name, lp_ptr
                        );
                        None
                    }
                    Err(e) => {
                        error!(
                            "dispatch_component_tags: export '{}' trapped: {}",
                            export_name, e
                        );
                        None
                    }
                },
                Err(e) => {
                    debug!(
                        "dispatch_component_tags: export '{}' has unexpected signature (expected `(i32) -> i32`): {}",
                        export_name, e
                    );
                    None
                }
            },
            None => {
                debug!(
                    "dispatch_component_tags: no export '{}' for <{}>; applying fallback '{}'",
                    export_name, tag_name, fallback
                );
                None
            }
        };

        match replacement {
            Some(rendered) => {
                result.replace_range(abs_open..element_end, &rendered);
                offset = abs_open + rendered.len();
            }
            None => match fallback {
                crate::build_manifest::callback_fallback::EMPTY => {
                    result.replace_range(abs_open..element_end, "");
                    offset = abs_open;
                }
                crate::build_manifest::callback_fallback::ERROR => {
                    let marker = format!(
                        "<!-- component-tag-render: no export '{}' for <{}> -->",
                        export_name, tag_name
                    );
                    result.replace_range(abs_open..element_end, &marker);
                    offset = abs_open + marker.len();
                }
                // PASSTHROUGH or any unknown fallback — leave tag in place.
                _ => {
                    offset = abs_open + 1;
                }
            },
        }
    }

    result
}

/// Inject the frame.ui runtime loader `<script src="/loader.js" defer></script>`
/// into the rendered document when at least one hydration island is present.
///
/// Idempotent: if the document already references `/loader.js`, this is a no-op.
/// Placement: inserted before the last `</body>` tag (case-insensitive). If no
/// `</body>` exists, the script is appended to the end of the document.
fn inject_loader_script(html: &str) -> String {
    if !html.contains("data-island=\"") {
        return html.to_string();
    }
    if html.contains("/loader.js") {
        return html.to_string();
    }

    const SCRIPT_TAG: &str =
        "<script src=\"/loader.js\" defer data-wasm=\"/frontend.wasm\"></script>";

    let lower = html.to_ascii_lowercase();
    if let Some(pos) = lower.rfind("</body>") {
        let mut out = String::with_capacity(html.len() + SCRIPT_TAG.len());
        out.push_str(&html[..pos]);
        out.push_str(SCRIPT_TAG);
        out.push_str(&html[pos..]);
        out
    } else {
        let mut out = String::with_capacity(html.len() + SCRIPT_TAG.len());
        out.push_str(html);
        out.push_str(SCRIPT_TAG);
        out
    }
}

/// Inject a `<script id="cl-i18n-bundle">window.__CLEAN_I18N__ = {...};</script>` tag into
/// `html` so the browser-side i18n runtime (`frame.ui/runtime/loader.js`) has the full
/// translation bundle available without a separate network request.
///
/// Insertion point: immediately after the first `<head>` or `<head ...>` opening tag
/// (matched case-insensitively with a simple scan rather than a regex, avoiding the
/// `regex` crate dependency). If no `<head>` tag is present (fragment rendering), the
/// script tag is appended at the very end of the string.
///
/// The `json` argument must already be HTML-safe (produced by `LocaleState::bundle_as_json`).
fn inject_i18n_bundle(html: &str, json: &str) -> String {
    let script = format!(
        r#"<script id="cl-i18n-bundle">window.__CLEAN_I18N__ = {};</script>"#,
        json
    );

    // Find `<head` in a case-insensitive manner by scanning the lowercased copy, then
    // use the original string for the actual slicing so we preserve the original casing.
    let lower = html.to_ascii_lowercase();
    let insert_pos: Option<usize> = lower.find("<head").and_then(|head_start| {
        // Advance past the `>` that closes the opening tag (handles `<head class="...">`).
        html[head_start..].find('>').map(|rel| head_start + rel + 1)
    });

    match insert_pos {
        Some(pos) => {
            let mut out = String::with_capacity(html.len() + script.len());
            out.push_str(&html[..pos]);
            out.push_str(&script);
            out.push_str(&html[pos..]);
            out
        }
        None => {
            // Fragment (no <head>): append at the end.
            let mut out = String::with_capacity(html.len() + script.len());
            out.push_str(html);
            out.push_str(&script);
            out
        }
    }
}

/// Extract attribute value from an HTML tag string, e.g. `client="on"` → `Some("on")`.
fn extract_attr_value_from_tag(tag: &str, attr: &str) -> Option<String> {
    let search = format!("{}=\"", attr);
    let start = tag.find(search.as_str())?;
    let val_start = start + search.len();
    let val_end = tag[val_start..].find('"')? + val_start;
    Some(tag[val_start..val_end].to_string())
}

/// Extract `<page layout="name">...</page>` wrapper.
/// Returns (inner_content, Some(layout_name)) or (full_html, None).
fn extract_page_layout(html: &str) -> (String, Option<String>) {
    const PAGE_TAG: &str = "<page";
    let Some(tag_start) = html.find(PAGE_TAG) else {
        return (html.to_string(), None);
    };

    let after_tag = &html[tag_start + PAGE_TAG.len()..];

    let layout = after_tag.find(" layout=\"").map(|attr_pos| {
        let val_start = attr_pos + " layout=\"".len();
        let val_end = after_tag[val_start..].find('"').unwrap_or(0);
        after_tag[val_start..val_start + val_end].to_string()
    });

    let open_end = html[tag_start..]
        .find('>')
        .map(|p| tag_start + p + 1)
        .unwrap_or(tag_start);

    let close_tag = "</page>";
    let inner_end = html[open_end..]
        .find(close_tag)
        .map(|p| open_end + p)
        .unwrap_or(html.len());
    let inner = html[open_end..inner_end].trim().to_string();

    (inner, layout)
}

/// Load a layout file and inject `content` into its `<slot>`.
fn apply_layout(content: &str, layout_name: &str, cwd: &std::path::Path) -> String {
    let candidates = [
        cwd.join(format!("app/ui/web/layouts/{}.html", layout_name)),
        cwd.join(format!("app/ui/layouts/{}.html", layout_name)),
        cwd.join(format!("ui/layouts/{}.html", layout_name)),
        cwd.join(format!("layouts/{}.html", layout_name)),
        cwd.join(format!("{}.html", layout_name)),
    ];

    for path in &candidates {
        if let Ok(layout_html) = std::fs::read_to_string(path) {
            return layout_html
                .replace("<slot />", content)
                .replace("<slot/>", content)
                .replace("<slot></slot>", content);
        }
    }

    debug!(
        "apply_layout: layout '{}' not found, returning content as-is",
        layout_name
    );
    content.to_string()
}

/// Evaluate server-side directives: cl-iterate, cl-if/cl-else, cl-show.
fn process_directives(html: &str, data: &serde_json::Value) -> String {
    let mut result = process_iterate_directive(html, data);
    result = process_if_directive(&result, data);
    result = process_show_directive(&result, data);
    result
}

/// Look up a dot-separated path in a JSON value.
fn get_nested_value<'a>(data: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = data;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

/// Evaluate a simple boolean condition against data (key path lookup).
fn evaluate_condition(condition: &str, data: &serde_json::Value) -> bool {
    match get_nested_value(data, condition.trim()) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Null) | None => false,
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(serde_json::Value::Array(arr)) => !arr.is_empty(),
        Some(serde_json::Value::Object(obj)) => !obj.is_empty(),
    }
}

/// Find the opening tag start (`<`) that contains position `attr_pos` inside `html`.
fn find_tag_start(html: &str, attr_pos: usize) -> Option<usize> {
    html[..attr_pos].rfind('<')
}

/// Extract the tag name from an opening tag starting at `tag_start` in `html`.
fn extract_tag_name(html: &str, tag_start: usize) -> String {
    let after = &html[tag_start + 1..];
    let end = after
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(after.len());
    after[..end].to_string()
}

/// Find the end position (exclusive) of the element that begins with an opening tag at `tag_start`.
/// Handles nesting by counting open/close pairs for `tag_name`.
fn find_element_end(html: &str, tag_start: usize, tag_name: &str) -> Option<usize> {
    let open_pattern = format!("<{}", tag_name);
    let close_pattern = format!("</{}>", tag_name);

    // Skip past the opening tag itself
    let open_tag_end = html[tag_start..].find('>')? + tag_start + 1;
    let mut depth = 1usize;
    let mut pos = open_tag_end;

    while pos < html.len() {
        let rest = &html[pos..];
        let next_open = rest.find(&open_pattern);
        let next_close = rest.find(&close_pattern);

        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                pos += o + open_pattern.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    return Some(pos + c + close_pattern.len());
                }
                pos += c + close_pattern.len();
            }
            (Some(o), None) => {
                depth += 1;
                pos += o + open_pattern.len();
            }
            (None, None) => break,
        }
    }
    None
}

/// Process `cl-iterate="item in array_path"` directives. The wrapper element
/// is preserved for every iteration; only the `cl-iterate` attribute itself
/// is stripped from the rendered output. Inside the body, `{item}` and
/// `{item.field}` resolve against the current iteration value via a scoped
/// data context that overlays the iteration variable on top of `data`.
fn process_iterate_directive(html: &str, data: &serde_json::Value) -> String {
    const MARKER: &str = " cl-iterate=\"";
    let mut result = html.to_string();

    while let Some(attr_pos) = result.find(MARKER) {
        let tag_start = match find_tag_start(&result, attr_pos) {
            Some(p) => p,
            None => break,
        };
        let tag_name = extract_tag_name(&result, tag_start);

        // Extract attribute value
        let val_start = attr_pos + MARKER.len();
        let val_end = match result[val_start..].find('"') {
            Some(p) => val_start + p,
            None => break,
        };
        let attr_value = result[val_start..val_end].to_string();
        let attr_full = format!(" cl-iterate=\"{}\"", attr_value);

        // Parse "item in array_path"
        let parts: Vec<&str> = attr_value.splitn(3, ' ').collect();
        if parts.len() != 3 || parts[1] != "in" {
            break;
        }
        let item_var = parts[0];
        let array_path = parts[2];

        // Find element bounds
        let element_end = match find_element_end(&result, tag_start, &tag_name) {
            Some(e) => e,
            None => break,
        };
        let open_tag_end = match result[tag_start..].find('>') {
            Some(p) => tag_start + p + 1,
            None => break,
        };
        let close_tag_len = tag_name.len() + 3; // </name>

        // Capture the opening tag (e.g. `<li class="x" cl-iterate="...">`)
        // and strip the `cl-iterate` attribute from it. All other attributes
        // (class, id, data-*, etc.) are preserved on every iteration.
        let opening_tag = result[tag_start..open_tag_end].replacen(&attr_full, "", 1);
        let closing_tag = format!("</{}>", tag_name);
        let inner = result[open_tag_end..element_end - close_tag_len].to_string();

        // Get array from data; non-arrays render as zero iterations.
        let items: Vec<serde_json::Value> = match get_nested_value(data, array_path) {
            Some(serde_json::Value::Array(arr)) => arr.clone(),
            _ => vec![],
        };

        // Build a scoped data context for each iteration. The base object is
        // the original `data` (so `{topLevelVar}` still resolves); the item
        // is bound under `item_var`. For object items, every field is also
        // bound under `item_var.field` via the lookup_path dot-walk.
        let mut expanded = String::new();
        for item in &items {
            let mut scope = match data {
                serde_json::Value::Object(map) => map.clone(),
                _ => serde_json::Map::new(),
            };
            scope.insert(item_var.to_string(), item.clone());
            let scoped = serde_json::Value::Object(scope);

            // Recursively process nested directives in the body, then run
            // the placeholder substitution. The recursion lets `cl-iterate`
            // nest inside another `cl-iterate` (e.g. matrix rows).
            let body_with_directives = process_directives(&inner, &scoped);
            let body_substituted = substitute_template(&body_with_directives, &scoped);

            expanded.push_str(&opening_tag);
            expanded.push_str(&body_substituted);
            expanded.push_str(&closing_tag);
        }

        result = format!(
            "{}{}{}",
            &result[..tag_start],
            expanded,
            &result[element_end..]
        );
    }

    result
}

/// Process `cl-if="condition"` / `cl-else` directives.
fn process_if_directive(html: &str, data: &serde_json::Value) -> String {
    const MARKER: &str = " cl-if=\"";
    let mut result = html.to_string();

    while let Some(attr_pos) = result.find(MARKER) {
        let tag_start = match find_tag_start(&result, attr_pos) {
            Some(p) => p,
            None => break,
        };
        let tag_name = extract_tag_name(&result, tag_start);

        let val_start = attr_pos + MARKER.len();
        let val_end = match result[val_start..].find('"') {
            Some(p) => val_start + p,
            None => break,
        };
        let condition = result[val_start..val_end].to_string();
        let is_truthy = evaluate_condition(&condition, data);

        let element_end = match find_element_end(&result, tag_start, &tag_name) {
            Some(e) => e,
            None => break,
        };
        let attr_full = format!(" cl-if=\"{}\"", condition);

        // Check for a cl-else element immediately following (ignoring whitespace)
        let rest_trimmed_offset =
            element_end + result[element_end..].len() - result[element_end..].trim_start().len();
        let rest = &result[rest_trimmed_offset..];
        let has_else = rest.starts_with('<') && {
            let tag_end = rest.find('>').unwrap_or(0);
            rest[..tag_end].contains(" cl-else")
        };

        let (keep, total_end) = if has_else {
            let else_tag_start = rest_trimmed_offset;
            let else_tag_name = extract_tag_name(&result, else_tag_start);
            let else_element_end =
                find_element_end(&result, else_tag_start, &else_tag_name).unwrap_or(else_tag_start);

            if is_truthy {
                let full_element = result[tag_start..element_end].replace(&attr_full, "");
                (full_element, else_element_end)
            } else {
                let full_else = result[else_tag_start..else_element_end].replace(" cl-else", "");
                (full_else, else_element_end)
            }
        } else if is_truthy {
            let full_element = result[tag_start..element_end].replace(&attr_full, "");
            (full_element, element_end)
        } else {
            (String::new(), element_end)
        };

        result = format!("{}{}{}", &result[..tag_start], keep, &result[total_end..]);
    }

    result
}

/// Process `cl-show="condition"` directives (adds `display:none` when falsy).
fn process_show_directive(html: &str, data: &serde_json::Value) -> String {
    const MARKER: &str = " cl-show=\"";
    let mut result = html.to_string();

    while let Some(attr_pos) = result.find(MARKER) {
        let val_start = attr_pos + MARKER.len();
        let val_end = match result[val_start..].find('"') {
            Some(p) => val_start + p,
            None => break,
        };
        let condition = result[val_start..val_end].to_string();
        let is_truthy = evaluate_condition(&condition, data);

        // Remove the cl-show="..." attribute regardless
        let attr_full = format!(" cl-show=\"{}\"", condition);
        if is_truthy {
            result = result.replacen(&attr_full, "", 1);
        } else {
            // Find the tag start and add display:none style
            let tag_start = match find_tag_start(&result, attr_pos) {
                Some(p) => p,
                None => break,
            };
            let tag_end = match result[tag_start..].find('>') {
                Some(p) => tag_start + p,
                None => break,
            };
            let opening_tag = result[tag_start..tag_end + 1].to_string();
            let new_tag = if opening_tag.contains("style=\"") {
                opening_tag.replacen(&attr_full, "", 1).replacen(
                    "style=\"",
                    "style=\"display:none;",
                    1,
                )
            } else {
                opening_tag.replacen(&attr_full, "", 1).replacen(
                    '>',
                    " style=\"display:none;\">",
                    1,
                )
            };
            result = format!(
                "{}{}{}",
                &result[..tag_start],
                new_tag,
                &result[tag_end + 1..]
            );
        }
    }

    result
}

/// Register async bridge functions (_async_fire, _async_await, _server_sleep)
fn register_async_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _async_fire — fire-and-forget: lower `background expr`
    // Signature: (fn_name_ptr: i32, fn_name_len: i32, args_ptr: i32, args_len: i32) -> void
    register_bridge_fn!(
        linker,
        "_async_fire",
        |mut caller: Caller<'_, WasmState>,
         fn_name_ptr: i32,
         fn_name_len: i32,
         _args_ptr: i32,
         _args_len: i32| {
            let fn_name = read_raw_string(&mut caller, fn_name_ptr, fn_name_len)
                .unwrap_or_else(|| "<unknown>".to_string());
            debug!("_async_fire: scheduling background call to '{}'", fn_name);
        }
    );

    // _async_await — blocking async: lower `later x = expr`
    // Signature: (fn_name_ptr: i32, fn_name_len: i32, args_ptr: i32, args_len: i32) -> i32 (ptr)
    register_bridge_fn!(linker, "_async_await", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                 fn_name_ptr: i32,
                                                 fn_name_len: i32,
                                                 _args_ptr: i32,
                                                 _args_len: i32|
     -> i32 {
        let fn_name = read_raw_string(&mut caller, fn_name_ptr, fn_name_len)
            .unwrap_or_else(|| "<unknown>".to_string());
        debug!("_async_await: blocking call to '{}'", fn_name);
        // Write an empty string result into WASM memory and return its pointer
        write_string_to_caller(&mut caller, "")
    });

    // _server_sleep — suspend for N milliseconds
    // Signature: (ms: i64) -> void
    register_bridge_fn!(
        linker,
        "_server_sleep",
        |_caller: Caller<'_, WasmState>, ms: i64| {
            let duration = std::time::Duration::from_millis(ms.max(0) as u64);
            debug!("_server_sleep: sleeping for {}ms", ms);
            std::thread::sleep(duration);
        }
    );

    Ok(())
}

/// Register dev-mode capture bridge functions.
///
/// Currently one function: `_dev_snapshot()` — returns a length-prefixed JSON
/// string with the runtime capture payload, gated on `CLEAN_DEV=1`. In
/// production (CLEAN_DEV unset or any other value) it returns a
/// length-prefixed empty string, which the framework's `/_debug/capture`
/// handler interprets as "not in dev mode" and responds 404.
///
/// The security boundary is the env var check inside
/// `crate::dev_capture::snapshot_json()` — even if this function is
/// mistakenly called in production, no capture data leaks.
///
/// See `foundation/spec/platform/SERVER_EXTENSIONS.md` §Dev-mode
/// Capture for the payload contract.
fn register_dev_capture_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    register_bridge_fn!(linker, "_dev_snapshot", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        if !check_bridge_permission(&caller, "_dev_snapshot") {
            return write_string_to_caller(&mut caller, "");
        }
        let payload = crate::dev_capture::snapshot_json();
        write_string_to_caller(&mut caller, &payload)
    });
    Ok(())
}

/// Register MCP bridge functions (_mcp_stdio_read, _mcp_stdio_write, _mcp_http_serve,
/// _mcp_http_accept, _mcp_http_respond, _mcp_sse_send, _mcp_log)
fn register_mcp_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _mcp_stdio_read — blocks reading one newline-terminated JSON-RPC message from stdin
    register_bridge_fn!(linker, "_mcp_stdio_read", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        use std::io::BufRead;
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) => write_string_to_caller(&mut caller, ""),
            Ok(_) => {
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                debug!("_mcp_stdio_read: {} bytes", line.len());
                write_string_to_caller(&mut caller, &line)
            }
            Err(e) => {
                error!("_mcp_stdio_read: {}", e);
                write_string_to_caller(&mut caller, "")
            }
        }
    });

    // _mcp_stdio_write — write to stdout (stdio mode) or pending HTTP response (HTTP mode)
    register_bridge_fn!(linker, "_mcp_stdio_write", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                     msg_ptr: i32,
                                                     msg_len: i32|
     -> i32 {
        let msg = match read_raw_string(&mut caller, msg_ptr, msg_len) {
            Some(s) => s,
            None => {
                error!("_mcp_stdio_write: failed to read message");
                return 0;
            }
        };
        let mcp = caller.data().mcp.clone();
        let transport = mcp.transport.lock().expect("mcp transport lock").clone();
        match transport {
            McpTransport::Stdio => {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                if stdout.write_all(format!("{}\n", msg).as_bytes()).is_ok() {
                    let _ = stdout.flush();
                    debug!("_mcp_stdio_write: stdio {} bytes", msg.len());
                    1
                } else {
                    0
                }
            }
            McpTransport::Http => {
                let tx = mcp
                    .current_http_response
                    .lock()
                    .expect("mcp response lock")
                    .take();
                match tx {
                    Some(sender) => {
                        debug!("_mcp_stdio_write: http {} bytes", msg.len());
                        if sender.send(msg).is_ok() { 1 } else { 0 }
                    }
                    None => {
                        error!("_mcp_stdio_write: no pending HTTP response context");
                        0
                    }
                }
            }
        }
    });

    // _mcp_http_serve — start background MCP HTTP+SSE server
    register_bridge_fn!(linker, "_mcp_http_serve", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    port: i32,
                                                    host_ptr: i32,
                                                    host_len: i32|
     -> i32 {
        let host = read_raw_string(&mut caller, host_ptr, host_len)
            .unwrap_or_else(|| "0.0.0.0".to_string());
        let mcp = caller.data().mcp.clone();
        *mcp.transport.lock().expect("mcp transport lock") = McpTransport::Http;
        let addr = format!("{}:{}", host, port);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(run_mcp_http_server(addr.clone(), mcp));
                info!("_mcp_http_serve: MCP HTTP server starting on {}", addr);
                1
            }
            Err(e) => {
                error!("_mcp_http_serve: no tokio runtime: {}", e);
                0
            }
        }
    });

    // _mcp_http_accept — block until next POST /mcp request, return body
    register_bridge_fn!(linker, "_mcp_http_accept", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let mcp = caller.data().mcp.clone();
        let request = {
            let (ref queue_mutex, ref condvar) = mcp.request_queue;
            let mut q = queue_mutex.lock().expect("mcp queue lock");
            loop {
                if let Some(req) = q.pop_front() {
                    break req;
                }
                q = condvar.wait(q).expect("mcp condvar wait");
            }
        };
        *mcp.current_http_response.lock().expect("mcp response lock") = Some(request.response_tx);
        debug!(
            "_mcp_http_accept: received request {} bytes",
            request.body.len()
        );
        write_string_to_caller(&mut caller, &request.body)
    });

    // _mcp_http_respond — deliver response to the client of the last _mcp_http_accept.
    // Pairs 1:1 with _mcp_http_accept; clearer semantics than reusing _mcp_stdio_write
    // in HTTP mode (which conflates stdio and http transports).
    register_bridge_fn!(linker, "_mcp_http_respond", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                      body_ptr: i32,
                                                      body_len: i32|
     -> i32 {
        let body = match read_raw_string(&mut caller, body_ptr, body_len) {
            Some(s) => s,
            None => {
                error!("_mcp_http_respond: failed to read body");
                return 0;
            }
        };
        let tx = caller
            .data()
            .mcp
            .current_http_response
            .lock()
            .expect("mcp response lock")
            .take();
        match tx {
            Some(sender) => {
                debug!("_mcp_http_respond: sending {} bytes", body.len());
                if sender.send(body).is_ok() { 1 } else { 0 }
            }
            None => {
                error!("_mcp_http_respond: no in-flight HTTP request to respond to");
                0
            }
        }
    });

    // _mcp_sse_send — send raw SSE-formatted event to a specific connected client
    register_bridge_fn!(linker, "_mcp_sse_send", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                  client_id_ptr: i32,
                                                  client_id_len: i32,
                                                  event_ptr: i32,
                                                  event_len: i32|
     -> i32 {
        let client_id = match read_raw_string(&mut caller, client_id_ptr, client_id_len) {
            Some(s) => s,
            None => return 0,
        };
        let event = match read_raw_string(&mut caller, event_ptr, event_len) {
            Some(s) => s,
            None => return 0,
        };
        let clients = caller
            .data()
            .mcp
            .sse_clients
            .lock()
            .expect("sse clients lock");
        match clients.get(&client_id) {
            Some(tx) => {
                if tx.send(event).is_ok() {
                    debug!("_mcp_sse_send: sent to client {}", client_id);
                    1
                } else {
                    debug!("_mcp_sse_send: client {} disconnected", client_id);
                    0
                }
            }
            None => {
                debug!("_mcp_sse_send: unknown client {}", client_id);
                0
            }
        }
    });

    // _mcp_log — write structured log to stderr (never stdout, which would corrupt stdio transport)
    register_bridge_fn!(linker, "_mcp_log", |mut caller: Caller<'_, WasmState>,
                                             level_ptr: i32,
                                             level_len: i32,
                                             msg_ptr: i32,
                                             msg_len: i32|
     -> i32 {
        let level = read_raw_string(&mut caller, level_ptr, level_len)
            .unwrap_or_else(|| "info".to_string());
        let msg = read_raw_string(&mut caller, msg_ptr, msg_len).unwrap_or_default();
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "[frame.mcp] {}: {}", level, msg);
        1
    });

    Ok(())
}

// ── MCP HTTP server helpers ──────────────────────────────────────────────────

/// Axum router state for the MCP HTTP server
type McpAxumState = std::sync::Arc<McpBridgeState>;

/// Run an MCP HTTP+SSE server on the given address.
///
/// Called by `_mcp_http_serve` on the current tokio runtime.
async fn run_mcp_http_server(addr: String, mcp: McpAxumState) {
    use axum::{
        Router,
        routing::{get, post},
    };
    use tower_http::cors::{Any, CorsLayer};

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers(Any);

    let app = Router::new()
        .route("/mcp", post(handle_mcp_post))
        .route("/sse", get(handle_mcp_sse))
        .layer(cors)
        .with_state(mcp);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("MCP HTTP server: failed to bind {}: {}", addr, e);
            return;
        }
    };

    tracing::info!("MCP HTTP server listening on {}", addr);
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("MCP HTTP server error: {}", e);
    }
}

/// Handle POST /mcp — queue the request body for the WASM module, await the response.
async fn handle_mcp_post(
    axum::extract::State(mcp): axum::extract::State<McpAxumState>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let body_str = String::from_utf8_lossy(&body).into_owned();
    let session_id = uuid::Uuid::new_v4().to_string();

    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
    {
        let (ref queue_mutex, ref condvar) = mcp.request_queue;
        let mut queue = queue_mutex.lock().expect("mcp queue lock");
        queue.push_back(McpPendingRequest {
            body: body_str,
            response_tx: tx,
        });
        condvar.notify_one();
    }

    let response_body = tokio::task::spawn_blocking(move || rx.recv().unwrap_or_default())
        .await
        .unwrap_or_default();

    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .header("Mcp-Session-Id", session_id)
        .body(axum::body::Body::from(response_body))
        .expect("valid MCP POST response")
}

/// Handle GET /sse — register a new SSE client and stream events to it.
async fn handle_mcp_sse(
    axum::extract::State(mcp): axum::extract::State<McpAxumState>,
) -> axum::response::Response {
    let client_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    mcp.sse_clients
        .lock()
        .expect("sse clients lock")
        .insert(client_id.clone(), tx);

    tracing::info!("MCP SSE client connected: {}", client_id);

    let stream = McpSseStream { rx };

    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Access-Control-Allow-Origin", "*")
        .header("X-Accel-Buffering", "no")
        .header("Mcp-Session-Id", client_id)
        .body(axum::body::Body::from_stream(stream))
        .expect("valid SSE response")
}

/// Stream adapter that converts a tokio unbounded channel receiver into an
/// `http_body` `Stream` usable by `axum::body::Body::from_stream`.
struct McpSseStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl futures::Stream for McpSseStream {
    type Item = Result<bytes::Bytes, std::convert::Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(s)) => {
                std::task::Poll::Ready(Some(Ok(bytes::Bytes::from(s))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Register test bridge functions (_test_http_request, _test_response_status, _test_response_body).
///
/// These support `test: endpoint:` blocks in Clean Language. `_test_http_request` dispatches
/// an in-process HTTP request by calling the registered route handler WASM export directly,
/// capturing the response into a handle map. The companion functions read back status/body.
fn register_test_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _test_http_request - Dispatch an in-process request and return a response handle
    // Signature: (method_ptr, method_len, path_ptr, path_len, body_ptr, body_len,
    //             hkey_ptr, hkey_len, hval_ptr, hval_len) -> i32
    // Returns: handle >= 0 on success, -1 if route not found or dispatch fails
    register_bridge_fn!(linker, "_test_http_request", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                       method_ptr: i32,
                                                       method_len: i32,
                                                       path_ptr: i32,
                                                       path_len: i32,
                                                       body_ptr: i32,
                                                       body_len: i32,
                                                       hkey_ptr: i32,
                                                       hkey_len: i32,
                                                       hval_ptr: i32,
                                                       hval_len: i32|
     -> i32 {
        let method = read_raw_string(&mut caller, method_ptr, method_len)
            .unwrap_or_else(|| "GET".to_string());
        let full_path =
            read_raw_string(&mut caller, path_ptr, path_len).unwrap_or_else(|| "/".to_string());
        let body = read_raw_string(&mut caller, body_ptr, body_len).unwrap_or_default();
        let hkey = read_raw_string(&mut caller, hkey_ptr, hkey_len).unwrap_or_default();
        let hval = read_raw_string(&mut caller, hval_ptr, hval_len).unwrap_or_default();

        // Strip query string from path for route lookup
        let (clean_path, query_str) = match full_path.find('?') {
            Some(qi) => (full_path[..qi].to_string(), full_path[qi + 1..].to_string()),
            None => (full_path.clone(), String::new()),
        };

        let http_method = match HttpMethod::parse(&method) {
            Ok(m) => m,
            Err(_) => {
                error!("_test_http_request: invalid method '{}'", method);
                return -1;
            }
        };

        // Look up route and extract path params
        let (handler_name, path_params) = {
            let state = caller.data();
            match state.router.find(http_method, &clean_path) {
                Some((handler, params)) => (handler.handler_name.clone(), params),
                None => {
                    debug!("_test_http_request: no route for {} {}", method, clean_path);
                    return -1;
                }
            }
        };

        // Get the handler export before mutably borrowing caller
        let handler_func = caller.get_export(&handler_name).and_then(|e| e.into_func());
        let handler_func = match handler_func {
            Some(f) => f,
            None => {
                error!(
                    "_test_http_request: handler export '{}' not found",
                    handler_name
                );
                return -1;
            }
        };

        // Parse query string into a map
        let query: std::collections::HashMap<String, String> = if query_str.is_empty() {
            std::collections::HashMap::new()
        } else {
            use url::form_urlencoded;
            form_urlencoded::parse(query_str.as_bytes())
                .into_owned()
                .collect()
        };

        // Build request headers
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if !hkey.is_empty() {
            headers.push((hkey, hval));
        }

        // Set test request context and clear pending response state
        {
            let state = caller.data_mut();
            state.request_context = Some(crate::wasm::RequestContext {
                method: method.clone(),
                path: clean_path.clone(),
                headers,
                // The test dispatcher accepts a UTF-8 string body; the raw
                // byte view is the same content encoded as UTF-8.
                body_bytes: Some(body.as_bytes().to_vec()),
                body,
                params: path_params,
                query,
            });
            state.pending_status = None;
            state.pending_body = None;
            state.pending_headers.clear();
            state.pending_redirect = None;
            state.auth_context = None;
        }

        // Determine result buffer size and call the handler
        let result_count = handler_func.ty(&caller).results().len();
        let mut results = vec![wasmtime::Val::I32(0); result_count];
        let call_ok = handler_func.call(&mut caller, &[], &mut results).is_ok();

        // Capture and store the response
        let handle = {
            let state = caller.data_mut();
            let status = state.pending_status.unwrap_or(200) as i32;
            let body = state.pending_body.clone().unwrap_or_default();
            // Clear request state
            state.request_context = None;
            state.pending_status = None;
            state.pending_body = None;
            state.pending_headers.clear();
            state.auth_context = None;

            if !call_ok {
                debug!(
                    "_test_http_request: handler '{}' returned an error",
                    handler_name
                );
                return -1;
            }

            let h = state.next_test_handle;
            state.next_test_handle += 1;
            state
                .test_responses
                .insert(h, TestResponse { status, body });
            h
        };

        debug!(
            "_test_http_request: {} {} -> handle {}",
            method, clean_path, handle
        );
        handle
    });

    // _test_response_status - Get HTTP status from a test response handle
    // Signature: (handle: i32) -> i32
    // Returns: status code (e.g. 200), or -1 if handle unknown
    register_bridge_fn!(linker, "_test_response_status", |caller: Caller<
        '_,
        WasmState,
    >,
                                                          handle: i32|
     -> i32 {
        let state = caller.data();
        match state.test_responses.get(&handle) {
            Some(r) => r.status,
            None => {
                debug!("_test_response_status: unknown handle {}", handle);
                -1
            }
        }
    });

    // _test_response_body - Get response body JSON from a test response handle
    // Signature: (handle: i32) -> i32 (length-prefixed pointer)
    // Returns: empty string if handle unknown
    register_bridge_fn!(linker, "_test_response_body", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                        handle: i32|
     -> i32 {
        let body = {
            let state = caller.data();
            match state.test_responses.get(&handle) {
                Some(r) => r.body.clone(),
                None => {
                    debug!("_test_response_body: unknown handle {}", handle);
                    String::new()
                }
            }
        };
        write_string_to_caller(&mut caller, &body)
    });

    Ok(())
}

/// Register SSE bridge functions for STREAM endpoint handlers.
///
/// These implement the SSE wire protocol on top of a per-request tokio channel.
/// The server sets `WasmState::sse_sender` before calling the handler so these
/// functions can write formatted SSE frames directly to the live HTTP response.
fn register_sse_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // _sse_emit — write `data: {payload}\n\n`
    register_bridge_fn!(linker, "_sse_emit", |mut caller: Caller<'_, WasmState>,
                                              data_ptr: i32,
                                              data_len: i32|
     -> i32 {
        let data = match read_raw_string(&mut caller, data_ptr, data_len) {
            Some(s) => s,
            None => return -1,
        };
        let frame = format!("data: {}\n\n", data);
        let tx = caller.data().sse_sender.clone();
        match tx {
            Some(tx) if tx.send(frame).is_ok() => 0,
            _ => -1,
        }
    });

    // _sse_emit_event — write `event: {name}\ndata: {payload}\n\n`
    register_bridge_fn!(linker, "_sse_emit_event", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    name_ptr: i32,
                                                    name_len: i32,
                                                    data_ptr: i32,
                                                    data_len: i32|
     -> i32 {
        let name = match read_raw_string(&mut caller, name_ptr, name_len) {
            Some(s) => s,
            None => return -1,
        };
        let data = match read_raw_string(&mut caller, data_ptr, data_len) {
            Some(s) => s,
            None => return -1,
        };
        let frame = format!("event: {}\ndata: {}\n\n", name, data);
        let tx = caller.data().sse_sender.clone();
        match tx {
            Some(tx) if tx.send(frame).is_ok() => 0,
            _ => -1,
        }
    });

    // _sse_close — drop the sender so the stream EOF is delivered to the client
    register_bridge_fn!(
        linker,
        "_sse_close",
        |mut caller: Caller<'_, WasmState>| -> i32 {
            caller.data_mut().sse_sender = None;
            0
        }
    );

    // _sse_retry — write `retry: {ms}\n\n`
    register_bridge_fn!(linker, "_sse_retry", |caller: Caller<'_, WasmState>,
                                               ms: i32|
     -> i32 {
        let frame = format!("retry: {}\n\n", ms);
        let tx = caller.data().sse_sender.clone();
        match tx {
            Some(tx) if tx.send(frame).is_ok() => 0,
            _ => -1,
        }
    });

    // _sse_is_connected — returns 1 if the client is still connected, 0 if not
    register_bridge_fn!(linker, "_sse_is_connected", |caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        match &caller.data().sse_sender {
            Some(tx) if !tx.is_closed() => 1,
            _ => 0,
        }
    });

    Ok(())
}

/// Register browser-only no-op stubs.
///
/// These functions are implemented by the frame.ui browser runtime. The server
/// registers no-op stubs so WASM modules that import them can instantiate on the
/// server without trapping. String-returning stubs return LP empty string or the
/// fixed JSON literals `"[]"` / `"{}"` as specified in FEXT-1/2/3/5.
fn register_browser_stub_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    // ── DOM Query stubs (FEXT-2) ────────────────────────────────────────────

    register_bridge_fn!(linker, "_ui_get_bounds", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                   _sel_ptr: i32,
                                                   _sel_len: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    register_bridge_fn!(linker, "_ui_get_offset_bounds", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                          _sel_ptr: i32,
                                                          _sel_len: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    register_bridge_fn!(linker, "_ui_get_scroll", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                   _sel_ptr: i32,
                                                   _sel_len: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    register_bridge_fn!(linker, "_ui_set_scroll", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                   _sel_ptr: i32,
                                                   _sel_len: i32,
                                                   _x: f64,
                                                   _y: f64|
     -> i32 { 0 });

    register_bridge_fn!(linker, "_ui_query_all", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                  _sel_ptr: i32,
                                                  _sel_len: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "[]")
    });

    register_bridge_fn!(linker, "_ui_get_computed_style", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                           _sel_ptr: i32,
                                                           _sel_len: i32,
                                                           _prop_ptr: i32,
                                                           _prop_len: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    // ── DOM Patching stub (FEXT-5) ──────────────────────────────────────────

    register_bridge_fn!(linker, "_ui_patch", |_caller: Caller<'_, WasmState>,
                                              _sel_ptr: i32,
                                              _sel_len: i32,
                                              _html_ptr: i32,
                                              _html_len: i32|
     -> i32 { 0 });

    // ── iframe Communication stubs (FEXT-3) ────────────────────────────────

    register_bridge_fn!(linker, "_ui_iframe_send", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                    _sel_ptr: i32,
                                                    _sel_len: i32,
                                                    _msg_ptr: i32,
                                                    _msg_len: i32|
     -> i32 { 0 });

    register_bridge_fn!(linker, "_ui_iframe_on_message", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                          _handler_ptr: i32,
                                                          _handler_len: i32|
     -> i32 { 0 });

    register_bridge_fn!(linker, "_ui_iframe_get_bounds", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                          _iframe_sel_ptr: i32,
                                                          _iframe_sel_len: i32,
                                                          _inner_sel_ptr: i32,
                                                          _inner_sel_len: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    register_bridge_fn!(linker, "_ui_iframe_inject", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                      _sel_ptr: i32,
                                                      _sel_len: i32,
                                                      _script_ptr: i32,
                                                      _script_len: i32|
     -> i32 { 0 });

    // ── Drag Data stubs (FEXT-1) ────────────────────────────────────────────

    register_bridge_fn!(linker, "_ui_set_drag_data", |_caller: Caller<
        '_,
        WasmState,
    >,
                                                      _key_ptr: i32,
                                                      _key_len: i32,
                                                      _val_ptr: i32,
                                                      _val_len: i32|
     -> i32 { 0 });

    register_bridge_fn!(linker, "_ui_get_drag_data", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                      _key_ptr: i32,
                                                      _key_len: i32|
     -> i32 {
        write_string_to_caller(&mut caller, "")
    });

    register_bridge_fn!(linker, "_ui_event_data_json", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        write_string_to_caller(&mut caller, "{}")
    });

    Ok(())
}

/// Register email bridge functions: _email_configure, _email_send, _email_last_error
fn register_email_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    use crate::wasm::SmtpConfig;

    // _email_configure — store SMTP settings during startup
    // Args: host(ptr,len), port(i64), secure(i32), username(ptr,len), password(ptr,len), from(ptr,len)
    register_bridge_fn!(
        linker,
        "_email_configure",
        |mut caller: Caller<'_, WasmState>,
         host_ptr: i32,
         host_len: i32,
         port: i64,
         secure: i32,
         user_ptr: i32,
         user_len: i32,
         pass_ptr: i32,
         pass_len: i32,
         from_ptr: i32,
         from_len: i32| {
            let host = read_raw_string(&mut caller, host_ptr, host_len).unwrap_or_default();
            let username = read_raw_string(&mut caller, user_ptr, user_len).unwrap_or_default();
            let password = read_raw_string(&mut caller, pass_ptr, pass_len).unwrap_or_default();
            let from_address = read_raw_string(&mut caller, from_ptr, from_len).unwrap_or_default();
            let port = port.clamp(1, 65535) as u16;
            debug!(
                "_email_configure: host={} port={} secure={}",
                host,
                port,
                secure != 0
            );
            let config = SmtpConfig {
                host,
                port,
                secure: secure != 0,
                username,
                password,
                from_address,
            };
            caller.data().smtp_state.lock().config = Some(config);
        }
    );

    // _email_send — send an email via SMTP
    // Args: to(ptr,len), subject(ptr,len), html(ptr,len), text(ptr,len), from_override(ptr,len)
    // Returns: 1 on success, 0 on failure
    register_bridge_fn!(linker, "_email_send", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                to_ptr: i32,
                                                to_len: i32,
                                                subject_ptr: i32,
                                                subject_len: i32,
                                                html_ptr: i32,
                                                html_len: i32,
                                                text_ptr: i32,
                                                text_len: i32,
                                                from_ptr: i32,
                                                from_len: i32|
     -> i32 {
        let to = read_raw_string(&mut caller, to_ptr, to_len).unwrap_or_default();
        let subject = read_raw_string(&mut caller, subject_ptr, subject_len).unwrap_or_default();
        let html = read_raw_string(&mut caller, html_ptr, html_len).unwrap_or_default();
        let text = read_raw_string(&mut caller, text_ptr, text_len).unwrap_or_default();
        let from_override = read_raw_string(&mut caller, from_ptr, from_len).unwrap_or_default();

        let config = {
            let guard = caller.data().smtp_state.lock();
            match guard.config.clone() {
                Some(c) => c,
                None => {
                    error!("_email_send: SMTP not configured — call email.configure first");
                    caller.data().smtp_state.lock().last_error = "SMTP not configured".to_string();
                    return 0;
                }
            }
        };

        let from_addr = if from_override.is_empty() {
            &config.from_address
        } else {
            &from_override
        };

        let result = tokio::task::block_in_place(|| {
            use lettre::message::{MultiPart, SinglePart, header::ContentType};
            use lettre::transport::smtp::authentication::Credentials;
            use lettre::{Message, SmtpTransport, Transport};

            let message = Message::builder()
                .from(match from_addr.parse() {
                    Ok(m) => m,
                    Err(e) => return Err(format!("Invalid from address '{}': {}", from_addr, e)),
                })
                .to(match to.parse() {
                    Ok(m) => m,
                    Err(e) => return Err(format!("Invalid to address '{}': {}", to, e)),
                })
                .subject(subject.clone())
                .multipart(
                    MultiPart::alternative()
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_PLAIN)
                                .body(text.clone()),
                        )
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_HTML)
                                .body(html.clone()),
                        ),
                )
                .map_err(|e| format!("Failed to build email: {}", e))?;

            let creds = Credentials::new(config.username.clone(), config.password.clone());

            let transport = if config.secure {
                SmtpTransport::relay(&config.host)
                    .map_err(|e| format!("SMTP relay error: {}", e))?
                    .port(config.port)
                    .credentials(creds)
                    .build()
            } else {
                SmtpTransport::builder_dangerous(&config.host)
                    .port(config.port)
                    .credentials(creds)
                    .build()
            };

            transport
                .send(&message)
                .map_err(|e| format!("SMTP send error: {}", e))?;
            Ok(())
        });

        match result {
            Ok(()) => {
                debug!("_email_send: sent to {}", to);
                caller.data().smtp_state.lock().last_error = String::new();
                1
            }
            Err(e) => {
                error!("_email_send: {}", e);
                caller.data().smtp_state.lock().last_error = e;
                0
            }
        }
    });

    // _email_last_error — return the last SMTP error string
    // Returns: LP-encoded error string (empty if last send succeeded)
    register_bridge_fn!(linker, "_email_last_error", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let err = caller.data().smtp_state.lock().last_error.clone();
        write_string_to_caller(&mut caller, &err)
    });

    Ok(())
}

/// Register WebSocket bridge functions for LIVE endpoint handlers.
///
/// These 9 functions implement the WebSocket server contract defined in
/// `foundation/management/cross-component-prompts/websocket-server-runtime-implementation.md`.
///
/// ## Parameter conventions
///
/// - String parameters use raw `(ptr: i32, len: i32)` pairs — consistent with all
///   other Layer 3 bridge functions.
/// - `clientId` is `i32` (the public WASM surface) even though the internal
///   type is `i64`, because the spec table says `integer` maps to `i32` in this
///   context (WebSocket client IDs fit well within i32 range).
/// - Return strings use the length-prefixed format via `write_string_to_caller`.
fn register_websocket_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    use crate::websocket;

    // -----------------------------------------------------------------------
    // _http_ws_route — register a WebSocket route with its three WASM handlers
    // Signature: (method_ptr, method_len, path_ptr, path_len,
    //             onConnect_ptr, onConnect_len,
    //             onMessage_ptr, onMessage_len,
    //             onClose_ptr, onClose_len) -> void
    // The method parameter is always "LIVE" and is accepted for API symmetry.
    // -----------------------------------------------------------------------
    linker
        .func_wrap(
            "env",
            "_http_ws_route",
            |mut caller: Caller<'_, WasmState>,
             _method_ptr: i32,
             _method_len: i32,
             path_ptr: i32,
             path_len: i32,
             on_connect_ptr: i32,
             on_connect_len: i32,
             on_message_ptr: i32,
             on_message_len: i32,
             on_close_ptr: i32,
             on_close_len: i32| {
                let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                    Some(s) => s,
                    None => {
                        error!("_http_ws_route: Failed to read path");
                        return;
                    }
                };
                let on_connect = match read_raw_string(&mut caller, on_connect_ptr, on_connect_len)
                {
                    Some(s) => s,
                    None => {
                        error!("_http_ws_route: Failed to read onConnect");
                        return;
                    }
                };
                let on_message = match read_raw_string(&mut caller, on_message_ptr, on_message_len)
                {
                    Some(s) => s,
                    None => {
                        error!("_http_ws_route: Failed to read onMessage");
                        return;
                    }
                };
                let on_close = match read_raw_string(&mut caller, on_close_ptr, on_close_len) {
                    Some(s) => s,
                    None => {
                        error!("_http_ws_route: Failed to read onClose");
                        return;
                    }
                };

                info!(
                    "_http_ws_route: path={}, onConnect={}, onMessage={}, onClose={}",
                    path, on_connect, on_message, on_close
                );

                // Register the path in the HTTP router so the server's fallback
                // handler can detect it as a WebSocket route.
                let router = caller.data().router.clone();
                if let Err(e) = router.register_ws(path.clone(), on_connect.clone()) {
                    error!(
                        "_http_ws_route: Failed to register WS route {}: {}",
                        path, e
                    );
                    return;
                }

                // Register handler names in the shared WebSocket state so the
                // server can look them up when a connection arrives.
                let ws_state = caller.data().ws_state.clone();
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(websocket::register_ws_route(
                        &ws_state, path, on_connect, on_message, on_close,
                    ))
                });
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _http_ws_route: {}", e)))?;

    // -----------------------------------------------------------------------
    // _ws_send — send a text message to a specific client
    // Signature: (clientId: i64, msg_ptr: i32, msg_len: i32) -> void
    // (clientId is i64 per function-registry.toml.)
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_ws_send",
        |mut caller: Caller<'_, WasmState>, client_id: i64, msg_ptr: i32, msg_len: i32| {
            let message = match read_raw_string(&mut caller, msg_ptr, msg_len) {
                Some(s) => s,
                None => {
                    error!("_ws_send: Failed to read message");
                    return;
                }
            };
            let ws_state = caller.data().ws_state.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(websocket::ws_send(&ws_state, client_id, message))
            });
        }
    );

    // -----------------------------------------------------------------------
    // _ws_broadcast — broadcast a message to all clients in a room
    // Signature: (room_ptr: i32, room_len: i32, msg_ptr: i32, msg_len: i32) -> void
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_ws_broadcast",
        |mut caller: Caller<'_, WasmState>,
         room_ptr: i32,
         room_len: i32,
         msg_ptr: i32,
         msg_len: i32| {
            let room = match read_raw_string(&mut caller, room_ptr, room_len) {
                Some(s) => s,
                None => {
                    error!("_ws_broadcast: Failed to read room");
                    return;
                }
            };
            let message = match read_raw_string(&mut caller, msg_ptr, msg_len) {
                Some(s) => s,
                None => {
                    error!("_ws_broadcast: Failed to read message");
                    return;
                }
            };
            let ws_state = caller.data().ws_state.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(websocket::ws_room_broadcast(&ws_state, &room, message))
            });
        }
    );

    // -----------------------------------------------------------------------
    // _ws_close — close a specific client connection (code 1000)
    // Signature: (clientId: i64) -> void (per function-registry.toml)
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_ws_close",
        |caller: Caller<'_, WasmState>, client_id: i64| {
            let ws_state = caller.data().ws_state.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(websocket::ws_close(&ws_state, client_id))
            });
        }
    );

    // -----------------------------------------------------------------------
    // _ws_client_id — get the current WebSocket client ID from task-local context
    // Signature: () -> i64 (per function-registry.toml)
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_ws_client_id",
        |_caller: Caller<'_, WasmState>| -> i64 { websocket::current_client_id() }
    );

    // -----------------------------------------------------------------------
    // _ws_message — get the incoming message payload from task-local context
    // Signature: () -> i32 (LP string pointer)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_ws_message", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let msg = websocket::current_message();
        write_string_to_caller(&mut caller, &msg)
    });

    // -----------------------------------------------------------------------
    // _ws_room_join — add a client to a room
    // Signature: (clientId: i64, room_ptr: i32, room_len: i32) -> void
    // (clientId is i64 per function-registry.toml.)
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_ws_room_join",
        |mut caller: Caller<'_, WasmState>, client_id: i64, room_ptr: i32, room_len: i32| {
            let room = match read_raw_string(&mut caller, room_ptr, room_len) {
                Some(s) => s,
                None => {
                    error!("_ws_room_join: Failed to read room name");
                    return;
                }
            };
            let ws_state = caller.data().ws_state.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(websocket::ws_room_join(&ws_state, client_id, room))
            });
        }
    );

    // -----------------------------------------------------------------------
    // _ws_room_leave — remove a client from a room
    // Signature: (clientId: i64, room_ptr: i32, room_len: i32) -> void
    // (clientId is i64 per function-registry.toml.)
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_ws_room_leave",
        |mut caller: Caller<'_, WasmState>, client_id: i64, room_ptr: i32, room_len: i32| {
            let room = match read_raw_string(&mut caller, room_ptr, room_len) {
                Some(s) => s,
                None => {
                    error!("_ws_room_leave: Failed to read room name");
                    return;
                }
            };
            let ws_state = caller.data().ws_state.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(websocket::ws_room_leave(&ws_state, client_id, &room))
            });
        }
    );

    // -----------------------------------------------------------------------
    // _ws_room_broadcast — broadcast to all clients in a room (alias for _ws_broadcast)
    // Registered separately so both `ws.broadcast` and `ws.roomBroadcast` resolve.
    // Signature: (room_ptr: i32, room_len: i32, msg_ptr: i32, msg_len: i32) -> void
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_ws_room_broadcast",
        |mut caller: Caller<'_, WasmState>,
         room_ptr: i32,
         room_len: i32,
         msg_ptr: i32,
         msg_len: i32| {
            let room = match read_raw_string(&mut caller, room_ptr, room_len) {
                Some(s) => s,
                None => {
                    error!("_ws_room_broadcast: Failed to read room");
                    return;
                }
            };
            let message = match read_raw_string(&mut caller, msg_ptr, msg_len) {
                Some(s) => s,
                None => {
                    error!("_ws_room_broadcast: Failed to read message");
                    return;
                }
            };
            let ws_state = caller.data().ws_state.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(websocket::ws_room_broadcast(&ws_state, &room, message))
            });
        }
    );

    Ok(())
}

/// Register background job queue bridge functions (14 functions, frame.jobs contract).
///
/// ## Dual naming
///
/// These functions follow the standard dual-name convention (underscore + dot).
/// The `register_bridge_fn!` macro registers both forms automatically for
/// functions whose names follow the `_namespace_fn` pattern.
///
/// Functions with no dot alias per spec:
/// - `_job_register` (setup-only)
/// - `_schedule_cron` (setup-only)
///
/// ## Parameter conventions
///
/// - String inputs use raw `(ptr: i32, len: i32)` pairs.
/// - Return strings use the length-prefixed format via `write_string_to_caller`.
/// - Integer parameters are `i32` (matching Clean Language `integer` which maps
///   to `i32` in this context — job IDs, attempt counts, and delay values all
///   fit within i32 range).
fn register_jobs_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    use crate::jobs;

    // -----------------------------------------------------------------------
    // _job_register — register a job handler with retry policy (no dot alias)
    // Signature: (name_ptr, name_len, handler_ptr, handler_len,
    //             maxAttempts: i32, backoff_ptr, backoff_len,
    //             delay: i32, timeout: i32, queue_ptr, queue_len) -> void
    // -----------------------------------------------------------------------
    linker
        .func_wrap(
            "env",
            "_job_register",
            |mut caller: Caller<'_, WasmState>,
             name_ptr: i32,
             name_len: i32,
             handler_ptr: i32,
             handler_len: i32,
             max_attempts: i32,
             backoff_ptr: i32,
             backoff_len: i32,
             delay: i32,
             timeout: i32,
             queue_ptr: i32,
             queue_len: i32| {
                let name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s,
                    None => {
                        error!("_job_register: failed to read name");
                        return;
                    }
                };
                let handler = match read_raw_string(&mut caller, handler_ptr, handler_len) {
                    Some(s) => s,
                    None => {
                        error!("_job_register: failed to read handler");
                        return;
                    }
                };
                let backoff_str = read_raw_string(&mut caller, backoff_ptr, backoff_len)
                    .unwrap_or_else(|| "fixed".to_string());
                let queue = read_raw_string(&mut caller, queue_ptr, queue_len)
                    .unwrap_or_else(|| "default".to_string());

                let backoff = jobs::BackoffStrategy::parse(&backoff_str);
                let jobs_state = caller.data().jobs_state.clone();

                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(jobs::register_job(
                        &jobs_state,
                        name,
                        handler,
                        max_attempts.max(1) as u32,
                        backoff,
                        delay.max(0) as u64,
                        timeout.max(0) as u64,
                        queue,
                    ))
                });
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _job_register: {}", e)))?;

    // -----------------------------------------------------------------------
    // _job_enqueue — enqueue a job for immediate execution
    // Signature: (name_ptr, name_len, argsJson_ptr, argsJson_len) -> i32 (LP string job ID)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_job_enqueue", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                 name_ptr: i32,
                                                 name_len: i32,
                                                 args_ptr: i32,
                                                 args_len: i32|
     -> i32 {
        let name = match read_raw_string(&mut caller, name_ptr, name_len) {
            Some(s) => s,
            None => {
                error!("_job_enqueue: failed to read name");
                return 0;
            }
        };
        let args =
            read_raw_string(&mut caller, args_ptr, args_len).unwrap_or_else(|| "{}".to_string());

        let jobs_state = caller.data().jobs_state.clone();
        let job_id = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(jobs::enqueue_job(&jobs_state, name, args))
        });

        write_string_to_caller(&mut caller, &job_id)
    });

    // -----------------------------------------------------------------------
    // _job_enqueue_at — schedule a job for a specific future time
    // Signature: (name_ptr, name_len, argsJson_ptr, argsJson_len, runAtUnixMs: f64) -> i32 (LP string)
    // plugin.toml declares the 3rd param as "number" which expands to WASM f64.
    // f64 can represent Unix epoch milliseconds with 53-bit integer precision —
    // sufficient for any realistic scheduling horizon.
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_job_enqueue_at", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                    name_ptr: i32,
                                                    name_len: i32,
                                                    args_ptr: i32,
                                                    args_len: i32,
                                                    run_at_ms: f64|
     -> i32 {
        let name = match read_raw_string(&mut caller, name_ptr, name_len) {
            Some(s) => s,
            None => {
                error!("_job_enqueue_at: failed to read name");
                return 0;
            }
        };
        let args =
            read_raw_string(&mut caller, args_ptr, args_len).unwrap_or_else(|| "{}".to_string());

        // Cast f64 Unix-epoch milliseconds to u64 for internal storage.
        // Negative or NaN values are clamped to 0 (enqueue immediately).
        let run_at_u64 = if run_at_ms.is_nan() || run_at_ms < 0.0 {
            0u64
        } else {
            run_at_ms as u64
        };

        let jobs_state = caller.data().jobs_state.clone();
        let job_id = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(jobs::enqueue_job_at(
                &jobs_state,
                name,
                args,
                run_at_u64,
            ))
        });

        write_string_to_caller(&mut caller, &job_id)
    });

    // -----------------------------------------------------------------------
    // _job_cancel — cancel a pending job
    // Signature: (id_ptr, id_len) -> i32  (0 = ok / cancelled, -1 = not found / not pending)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_job_cancel", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                id_ptr: i32,
                                                id_len: i32|
     -> i32 {
        let job_id = match read_raw_string(&mut caller, id_ptr, id_len) {
            Some(s) => s,
            None => {
                error!("_job_cancel: failed to read job id");
                return -1;
            }
        };

        let jobs_state = caller.data().jobs_state.clone();
        let cancelled = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(jobs::cancel_job(&jobs_state, &job_id))
        });

        if cancelled { 0 } else { -1 }
    });

    // -----------------------------------------------------------------------
    // _job_status — get the current status string for a job ID
    // Signature: (id_ptr, id_len) -> i32 (LP string: "pending"|"running"|
    //            "succeeded"|"failed"|"cancelled")
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_job_status", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                id_ptr: i32,
                                                id_len: i32|
     -> i32 {
        let job_id = match read_raw_string(&mut caller, id_ptr, id_len) {
            Some(s) => s,
            None => {
                error!("_job_status: failed to read job id");
                return 0;
            }
        };

        let jobs_state = caller.data().jobs_state.clone();
        let status = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(jobs::job_status(&jobs_state, &job_id))
        });

        write_string_to_caller(&mut caller, &status)
    });

    // -----------------------------------------------------------------------
    // _job_result — get the result or error string for a job
    // Signature: (id_ptr, id_len) -> i32 (LP string)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_job_result", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                id_ptr: i32,
                                                id_len: i32|
     -> i32 {
        let job_id = match read_raw_string(&mut caller, id_ptr, id_len) {
            Some(s) => s,
            None => {
                error!("_job_result: failed to read job id");
                return 0;
            }
        };

        let jobs_state = caller.data().jobs_state.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(jobs::job_result(&jobs_state, &job_id))
        });

        write_string_to_caller(&mut caller, &result)
    });

    // -----------------------------------------------------------------------
    // _job_current_id — get the job ID of the currently executing job
    // Signature: () -> i32 (LP string)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_job_current_id", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let id = jobs::current_job_id();
        write_string_to_caller(&mut caller, &id)
    });

    // -----------------------------------------------------------------------
    // _job_current_args — get the args JSON of the currently executing job
    // Signature: () -> i32 (LP string)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_job_current_args", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        let args = jobs::current_job_args();
        write_string_to_caller(&mut caller, &args)
    });

    // -----------------------------------------------------------------------
    // _job_current_attempt — get the 1-based attempt number of the current job
    // Signature: () -> i32
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_job_current_attempt", |_caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        jobs::current_job_attempt()
    });

    // -----------------------------------------------------------------------
    // _job_retry_after — request retry after a custom delay (overrides backoff)
    // Signature: (delayMs: i32) -> void
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_job_retry_after",
        |_caller: Caller<'_, WasmState>, delay_ms: i32| {
            jobs::request_retry_after_ms(delay_ms as i64);
        }
    );

    // -----------------------------------------------------------------------
    // _job_fail — explicitly fail this job (no more retries)
    // Signature: (reason_ptr, reason_len) -> void
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_job_fail",
        |mut caller: Caller<'_, WasmState>, reason_ptr: i32, reason_len: i32| {
            let reason = read_raw_string(&mut caller, reason_ptr, reason_len)
                .unwrap_or_else(|| "unknown".to_string());
            jobs::mark_explicit_fail(reason);
        }
    );

    // -----------------------------------------------------------------------
    // _job_succeed — explicitly mark the job as succeeded with a result
    // Signature: (result_ptr, result_len) -> void
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_job_succeed",
        |mut caller: Caller<'_, WasmState>, result_ptr: i32, result_len: i32| {
            let result = read_raw_string(&mut caller, result_ptr, result_len).unwrap_or_default();
            jobs::mark_explicit_succeed(result);
        }
    );

    // -----------------------------------------------------------------------
    // _schedule_cron — register a cron-scheduled handler (no dot alias)
    // Signature: (name_ptr, name_len, cron_ptr, cron_len, handler_ptr, handler_len) -> ptr
    // Returns a length-prefixed registration ID string, or an empty string when
    // an argument cannot be read or the cron expression is invalid
    // (function-registry.toml: returns = "ptr").
    // -----------------------------------------------------------------------
    linker
        .func_wrap(
            "env",
            "_schedule_cron",
            |mut caller: Caller<'_, WasmState>,
             name_ptr: i32,
             name_len: i32,
             cron_ptr: i32,
             cron_len: i32,
             handler_ptr: i32,
             handler_len: i32|
             -> i32 {
                let name = match read_raw_string(&mut caller, name_ptr, name_len) {
                    Some(s) => s,
                    None => {
                        error!("_schedule_cron: failed to read name");
                        return write_string_to_caller(&mut caller, "");
                    }
                };
                let cron_expr = match read_raw_string(&mut caller, cron_ptr, cron_len) {
                    Some(s) => s,
                    None => {
                        error!("_schedule_cron: failed to read cron expr");
                        return write_string_to_caller(&mut caller, "");
                    }
                };
                let handler = match read_raw_string(&mut caller, handler_ptr, handler_len) {
                    Some(s) => s,
                    None => {
                        error!("_schedule_cron: failed to read handler");
                        return write_string_to_caller(&mut caller, "");
                    }
                };

                debug!(
                    "_schedule_cron: name={}, expr={}, handler={}",
                    name, cron_expr, handler
                );

                let jobs_state = caller.data().jobs_state.clone();
                let id = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(jobs::schedule_cron(
                        &jobs_state,
                        name,
                        cron_expr,
                        handler,
                    ))
                });

                write_string_to_caller(&mut caller, id.as_deref().unwrap_or(""))
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _schedule_cron: {}", e)))?;

    // -----------------------------------------------------------------------
    // _schedule_cancel — cancel a cron schedule by name or by the registration
    // ID returned from _schedule_cron
    // Signature: (key_ptr, key_len) -> i32  (1 = cancelled, 0 = not found / already inactive)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_schedule_cancel", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                     name_ptr: i32,
                                                     name_len: i32|
     -> i32 {
        let name = match read_raw_string(&mut caller, name_ptr, name_len) {
            Some(s) => s,
            None => {
                error!("_schedule_cancel: failed to read name");
                return 0;
            }
        };

        let jobs_state = caller.data().jobs_state.clone();
        let cancelled = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(jobs::schedule_cancel(&jobs_state, &name))
        });

        if cancelled { 1 } else { 0 }
    });

    Ok(())
}

/// Register i18n / locale bridge functions (8 functions, frame.locale contract).
///
/// All 7 "all-host" functions are registered plus `_i18n_load` (server-only).
/// `register_bridge_fn!` automatically creates the `i18n.*` dot-notation aliases.
/// Additional spec-defined aliases (`t`, `tc`, `locale.*`) are added in `register_dot_aliases`.
///
/// Translation data is stored in `WasmState.locale_state` (a `SharedLocaleState`).
/// The active locale for each request is stored in `crate::locale::LOCALE` task-local;
/// `_i18n_set_locale` writes it and all lookup functions read it.
fn register_locale_functions(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    use crate::locale;

    // -----------------------------------------------------------------------
    // _i18n_load — load translation JSON for a locale from a file path
    // Signature: (locale_ptr, locale_len, path_ptr, path_len) -> void
    // Server-only: reads the path from disk relative to the working directory.
    // -----------------------------------------------------------------------
    linker
        .func_wrap(
            "env",
            "_i18n_load",
            |mut caller: Caller<'_, WasmState>,
             locale_ptr: i32,
             locale_len: i32,
             path_ptr: i32,
             path_len: i32| {
                let locale_tag = match read_raw_string(&mut caller, locale_ptr, locale_len) {
                    Some(s) if !s.is_empty() => s,
                    _ => {
                        error!("_i18n_load: failed to read locale tag");
                        return;
                    }
                };
                let file_path = match read_raw_string(&mut caller, path_ptr, path_len) {
                    Some(s) if !s.is_empty() => s,
                    _ => {
                        error!("_i18n_load: failed to read file path");
                        return;
                    }
                };

                let cwd = std::env::current_dir().unwrap_or_default();
                let abs_path = cwd.join(&file_path);
                let json_str = match std::fs::read_to_string(&abs_path) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("_i18n_load: cannot read '{}': {}", abs_path.display(), e);
                        return;
                    }
                };

                let locale_state = caller.data().locale_state.clone();
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        let mut state = locale_state.write().await;
                        if let Err(e) = state.load_json(&locale_tag, &json_str) {
                            error!("{}", e);
                        } else {
                            info!(
                                "_i18n_load: loaded {} translations for locale '{}'",
                                state.translations.get(&locale_tag).map_or(0, |m| m.len()),
                                locale_tag
                            );
                        }
                    })
                });
            },
        )
        .map_err(|e| RuntimeError::wasm(format!("Failed to define _i18n_load: {}", e)))?;
    // _i18n_load has no dot alias (internal function per spec).

    // -----------------------------------------------------------------------
    // _i18n_set_locale — set the active locale for the current request
    // Signature: (locale_ptr, locale_len) -> void
    // -----------------------------------------------------------------------
    register_bridge_fn!(
        linker,
        "_i18n_set_locale",
        |mut caller: Caller<'_, WasmState>, locale_ptr: i32, locale_len: i32| {
            let locale_tag = match read_raw_string(&mut caller, locale_ptr, locale_len) {
                Some(s) if !s.is_empty() => s,
                _ => {
                    error!("_i18n_set_locale: failed to read locale");
                    return;
                }
            };
            if !locale::set_current_locale(locale_tag.clone()) {
                // Called outside a LOCALE task-local scope — fall back to storing
                // the locale in a request header slot so middleware can retrieve it.
                warn!("_i18n_set_locale: called outside LOCALE scope; storing in state");
                // Store as a synthetic pending header so the router middleware can
                // access it on the next lookup. The store field is used as a scratchpad.
                caller
                    .data_mut()
                    .pending_headers
                    .push(("x-cl-locale-requested".to_string(), locale_tag.clone()));
            }
            let is_rtl = locale::is_rtl(&locale_tag);
            if is_rtl {
                // Server-side RTL: schedule data-locale-dir="rtl" header for the renderer.
                caller
                    .data_mut()
                    .pending_headers
                    .push(("x-cl-locale-dir".to_string(), "rtl".to_string()));
            }
            debug!("_i18n_set_locale: locale='{}' rtl={}", locale_tag, is_rtl);
        }
    );

    // -----------------------------------------------------------------------
    // _i18n_locale — get the currently active locale
    // Signature: () -> i32 (LP string)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_i18n_locale", |mut caller: Caller<
        '_,
        WasmState,
    >|
     -> i32 {
        // Read from task-local first.
        let locale_tag = locale::current_locale();
        if !locale_tag.is_empty() {
            return write_string_to_caller(&mut caller, &locale_tag);
        }
        // Fall back to the configured default locale.
        let default = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                caller
                    .data()
                    .locale_state
                    .read()
                    .await
                    .default_locale
                    .clone()
            })
        });
        write_string_to_caller(&mut caller, &default)
    });

    // -----------------------------------------------------------------------
    // _i18n_t — translate a key with optional {placeholder} substitution
    // Signature: (key_ptr, key_len, params_ptr, params_len) -> i32 (LP string)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_i18n_t", |mut caller: Caller<'_, WasmState>,
                                            key_ptr: i32,
                                            key_len: i32,
                                            params_ptr: i32,
                                            params_len: i32|
     -> i32 {
        let key = match read_raw_string(&mut caller, key_ptr, key_len) {
            Some(s) => s,
            None => {
                error!("_i18n_t: failed to read key");
                return write_string_to_caller(&mut caller, "");
            }
        };
        let params = read_raw_string(&mut caller, params_ptr, params_len)
            .unwrap_or_else(|| "{}".to_string());

        let active_locale = locale::current_locale();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let state = caller.data().locale_state.read().await;
                let locale = if active_locale.is_empty() {
                    state.default_locale.clone()
                } else {
                    active_locale.clone()
                };
                state.translate(&key, &locale, &params)
            })
        });
        write_string_to_caller(&mut caller, &result)
    });

    // -----------------------------------------------------------------------
    // _i18n_t_count — translate a key with plural form selection
    // Signature: (key_ptr: i32, key_len: i32, count: i64, params_ptr: i32, params_len: i32) -> i32
    // (count is i64 per function-registry.toml.)
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_i18n_t_count", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                  key_ptr: i32,
                                                  key_len: i32,
                                                  count: i64,
                                                  params_ptr: i32,
                                                  params_len: i32|
     -> i32 {
        let key = match read_raw_string(&mut caller, key_ptr, key_len) {
            Some(s) => s,
            None => {
                error!("_i18n_t_count: failed to read key");
                return write_string_to_caller(&mut caller, "");
            }
        };
        let params = read_raw_string(&mut caller, params_ptr, params_len)
            .unwrap_or_else(|| "{}".to_string());

        let active_locale = locale::current_locale();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let state = caller.data().locale_state.read().await;
                let locale = if active_locale.is_empty() {
                    state.default_locale.clone()
                } else {
                    active_locale.clone()
                };
                state.translate_count(&key, count as i32, &locale, &params)
            })
        });
        write_string_to_caller(&mut caller, &result)
    });

    // -----------------------------------------------------------------------
    // _i18n_format_number — format a number with locale-specific separators
    // Signature: (value: f64, locale_ptr, locale_len, options_ptr, options_len) -> i32
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_i18n_format_number", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                        value: f64,
                                                        locale_ptr: i32,
                                                        locale_len: i32,
                                                        options_ptr: i32,
                                                        options_len: i32|
     -> i32 {
        let locale_arg = read_raw_string(&mut caller, locale_ptr, locale_len).unwrap_or_default();
        let options = read_raw_string(&mut caller, options_ptr, options_len)
            .unwrap_or_else(|| "{}".to_string());

        let effective_locale = if locale_arg.is_empty() {
            let active = locale::current_locale();
            if active.is_empty() {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        caller
                            .data()
                            .locale_state
                            .read()
                            .await
                            .default_locale
                            .clone()
                    })
                })
            } else {
                active
            }
        } else {
            locale_arg
        };

        let (decimals, use_grouping) = locale::parse_number_options(&options);
        let result = locale::format_number(value, &effective_locale, decimals, use_grouping);
        write_string_to_caller(&mut caller, &result)
    });

    // -----------------------------------------------------------------------
    // _i18n_format_currency — format a monetary amount
    // Signature: (value: f64, currency_ptr, currency_len, locale_ptr, locale_len) -> i32
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_i18n_format_currency", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                          value: f64,
                                                          currency_ptr: i32,
                                                          currency_len: i32,
                                                          locale_ptr: i32,
                                                          locale_len: i32|
     -> i32 {
        let currency = match read_raw_string(&mut caller, currency_ptr, currency_len) {
            Some(s) if !s.is_empty() => s,
            _ => {
                error!("_i18n_format_currency: empty currency code");
                return write_string_to_caller(&mut caller, "");
            }
        };
        let locale_arg = read_raw_string(&mut caller, locale_ptr, locale_len).unwrap_or_default();

        let effective_locale = if locale_arg.is_empty() {
            let active = locale::current_locale();
            if active.is_empty() {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        caller
                            .data()
                            .locale_state
                            .read()
                            .await
                            .default_locale
                            .clone()
                    })
                })
            } else {
                active
            }
        } else {
            locale_arg
        };

        let result = locale::format_currency(value, &currency, &effective_locale);
        write_string_to_caller(&mut caller, &result)
    });

    // -----------------------------------------------------------------------
    // _i18n_format_date — format a Unix timestamp as a locale-aware date string
    // Signature: (timestamp: f64, style_ptr, style_len, locale_ptr, locale_len) -> i32
    // `timestamp` is Unix seconds (f64). HOST_BRIDGE.md § i18n states seconds;
    // the spec cross-reference in the task says milliseconds for Intl.DateTimeFormat —
    // server-side we use seconds (chrono) which is what the spec says for Rust.
    // -----------------------------------------------------------------------
    register_bridge_fn!(linker, "_i18n_format_date", |mut caller: Caller<
        '_,
        WasmState,
    >,
                                                      timestamp: f64,
                                                      style_ptr: i32,
                                                      style_len: i32,
                                                      locale_ptr: i32,
                                                      locale_len: i32|
     -> i32 {
        let style = read_raw_string(&mut caller, style_ptr, style_len)
            .unwrap_or_else(|| "medium".to_string());
        let locale_arg = read_raw_string(&mut caller, locale_ptr, locale_len).unwrap_or_default();

        let effective_locale = if locale_arg.is_empty() {
            let active = locale::current_locale();
            if active.is_empty() {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        caller
                            .data()
                            .locale_state
                            .read()
                            .await
                            .default_locale
                            .clone()
                    })
                })
            } else {
                active
            }
        } else {
            locale_arg
        };

        let result = locale::format_date(timestamp, &style, &effective_locale);
        write_string_to_caller(&mut caller, &result)
    });

    Ok(())
}

/// Register dot-notation aliases for all Layer 3 `_namespace_fn` bridge functions.
///
/// The Clean Language compiler (0.30.120+) generates WASM imports in both
/// `_namespace_fn` and `namespace.fn` styles. This registers the dot form as
/// an alias of the already-registered underscore form so both resolve at link time.
///
/// Derived from function-registry.toml `aliases` field (Layer 3 entries).
fn register_dot_aliases(linker: &mut Linker<WasmState>) -> RuntimeResult<()> {
    const ALIASES: &[(&str, &str)] = &[
        // HTTP server (register_http_server_functions)
        ("_http_listen", "http.listen"),
        ("_http_route", "http.route"),
        ("_http_route_protected", "http.route_protected"),
        ("_http_serve_static", "http.serve_static"),
        // Islands (register_islands_functions)
        ("_island_register", "island.register"),
        // Request context (register_request_context_functions)
        ("_req_param", "req.param"),
        ("_req_param_int", "req.param_int"),
        ("_req_query", "req.query"),
        ("_req_body", "req.body"),
        ("_req_body_bytes", "req.body_bytes"),
        ("_req_body_sha256_hex", "req.body_sha256_hex"),
        ("_req_body_field", "req.body_field"),
        ("_req_header", "req.header"),
        ("_req_headers", "req.headers"),
        ("_req_method", "req.method"),
        ("_req_path", "req.path"),
        ("_req_cookie", "req.cookie"),
        ("_req_form", "req.form"),
        ("_req_ip", "req.ip"),
        // Session management (register_session_management_functions)
        ("_session_store", "session.store"),
        ("_session_get", "session.get"),
        ("_session_delete", "session.delete"),
        ("_session_exists", "session.exists"),
        ("_session_set_csrf", "session.set_csrf"),
        ("_session_get_csrf", "session.get_csrf"),
        ("_http_set_cookie", "http.set_cookie"),
        // Auth (register_session_auth_functions)
        ("_auth_get_session", "auth.get_session"),
        ("_auth_require_auth", "auth.require_auth"),
        ("_auth_require_role", "auth.require_role"),
        ("_auth_can", "auth.can"),
        ("_auth_has_any_role", "auth.has_any_role"),
        ("_auth_set_session", "auth.set_session"),
        ("_auth_clear_session", "auth.clear_session"),
        ("_auth_user_id", "auth.user_id"),
        ("_auth_user_role", "auth.user_role"),
        // Roles (register_roles_functions)
        ("_roles_register", "roles.register"),
        ("_role_has_permission", "role.has_permission"),
        ("_role_get_permissions", "role.get_permissions"),
        // Response (register_response_functions)
        ("_http_respond", "http.respond"),
        ("_http_redirect", "http.redirect"),
        ("_http_set_header", "http.set_header"),
        ("_res_set_header", "res.set_header"),
        ("_res_redirect", "res.redirect"),
        ("_res_status", "res.status"),
        ("_res_body", "res.body"),
        ("_res_json", "res.json"),
        ("_http_set_cache", "http.set_cache"),
        ("_http_no_cache", "http.no_cache"),
        // _res_download, _email_configure, _email_send, _email_last_error aliases are
        // derived automatically by the register_bridge_fn! macro.
        // JSON — Delivery-2 wiring per function-registry.toml.
        //   v2 canonicals own the dotted aliases (json.encode, json.encodePretty,
        //   json.decode). Legacy _json_encode / _json_decode remain registered
        //   under their underscored names for frame.server 2.9.5 backward compat
        //   but declare no aliases (dropped in [P5] post-soak).
        ("_json_encode_v2", "json.encode"),
        ("_json_encode_pretty_v2", "json.encodePretty"),
        ("_json_decode_v2", "json.decode"),
        ("_json_get", "json.get"),
        // Async aliases are registered by register_bridge_fn! macro in register_async_functions
        // Test bridge aliases are registered by register_bridge_fn! macro in register_test_functions
        // WebSocket aliases (_ws_*) are registered by register_bridge_fn! macro in
        // register_websocket_functions — they do not need manual entries here.
        // Manual-registration dot aliases the auto-derive macro could not produce
        // because these functions were declared with bare `func_wrap` (not the macro).
        // Each is required by function-registry.toml.
        ("_http_ws_route", "http.ws_route"),
        ("_job_register", "job.register"),
        ("_schedule_cron", "schedule.cron"),
        // Registry declares camelCase aliases for these (auto-derivation produces
        // snake_case which doesn't match). Both forms remain valid via aliases.
        ("_job_enqueue_at", "job.enqueueAt"),
        ("_job_enqueue_at", "queue.enqueueAt"),
        ("_job_current_id", "job.currentId"),
        ("_job_current_args", "job.currentArgs"),
        ("_job_current_attempt", "job.currentAttempt"),
        ("_job_retry_after", "job.retryAfter"),
        // Jobs — spec-defined aliases that differ from the auto-derived job.* forms.
        // register_bridge_fn! already creates: job.enqueue, job.enqueue_at, job.cancel,
        // job.status, job.result, job.current_id, job.current_args, job.current_attempt,
        // job.retry_after, job.fail, job.succeed, schedule.cancel.
        // The entries below add the additional spec aliases with different namespaces.
        ("_job_enqueue", "queue.enqueue"),
        ("_job_enqueue_at", "queue.enqueue_at"),
        ("_job_cancel", "queue.cancel"),
        ("_job_status", "queue.status"),
        ("_job_result", "queue.result"),
        ("_job_current_id", "job.id"),
        ("_job_current_args", "job.args"),
        ("_job_current_attempt", "job.attempt"),
        // _job_retry_after, _job_fail, _job_succeed, _schedule_cancel auto-aliases
        // match the spec (job.retry_after, job.fail, job.succeed, schedule.cancel) —
        // no manual entry needed; register_bridge_fn! handles them.
        // _job_register and _schedule_cron have no dot alias per spec (setup-only).
        // i18n / locale aliases. register_bridge_fn! macro auto-derives the
        // snake-cased dot form (e.g. i18n.t_count, i18n.set_locale). The registry
        // additionally declares camelCase / shortened aliases — registered below.
        // Bare aliases:
        ("_i18n_t", "t"),
        ("_i18n_t_count", "tc"),
        // Registry-declared camelCase forms (not derivable from snake_case):
        ("_i18n_t_count", "i18n.tc"),
        ("_i18n_set_locale", "i18n.setLocale"),
        ("_i18n_format_number", "i18n.formatNumber"),
        ("_i18n_format_currency", "i18n.formatCurrency"),
        ("_i18n_format_date", "i18n.formatDate"),
        // locale.* aliases per registry:
        ("_i18n_locale", "locale.get"),
        ("_i18n_set_locale", "locale.set"),
        ("_i18n_format_number", "locale.formatNumber"),
        ("_i18n_format_currency", "locale.formatCurrency"),
        ("_i18n_format_date", "locale.formatDate"),
        // _i18n_load is registered with bare func_wrap; add its dot alias manually.
        ("_i18n_load", "i18n.load"),
    ];

    for (canonical, dot_alias) in ALIASES {
        linker
            .alias("env", canonical, "env", dot_alias)
            .map_err(|e| {
                RuntimeError::wasm(format!(
                    "Failed to alias {} -> {}: {}",
                    canonical, dot_alias, e
                ))
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_layout(dir: &std::path::Path, rel: &str, body: &str) {
        let full = dir.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }

    #[test]
    fn apply_layout_finds_app_ui_web_layouts() {
        // Regression: SERVER-APPLY-LAYOUT-MISSES-APP-UI-WEB-LAYOUTS — after the
        // PROJECT_STRUCTURE migration apps store layouts under app/ui/web/layouts/.
        let tmp = tempfile::tempdir().unwrap();
        write_layout(
            tmp.path(),
            "app/ui/web/layouts/main.html",
            "<html><head></head><body><slot /></body></html>",
        );
        let out = apply_layout("<h1>hi</h1>", "main", tmp.path());
        assert_eq!(out, "<html><head></head><body><h1>hi</h1></body></html>");
    }

    #[test]
    fn apply_layout_falls_back_to_app_ui_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        write_layout(tmp.path(), "app/ui/layouts/main.html", "<a><slot/></a>");
        let out = apply_layout("X", "main", tmp.path());
        assert_eq!(out, "<a>X</a>");
    }

    #[test]
    fn apply_layout_returns_content_when_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let out = apply_layout("X", "missing", tmp.path());
        assert_eq!(out, "X");
    }

    #[test]
    fn test_create_linker() {
        let engine = Engine::default();
        // This will fail because WasmState requires a router, but the linker creation should work
        let result = create_linker(&engine);
        assert!(result.is_ok());
    }

    #[test]
    fn test_json_get_path_logic() {
        // Tests the path traversal logic used by _json_get, including numeric array indices
        fn json_get_by_path<'a>(
            parsed: &'a serde_json::Value,
            path: &str,
        ) -> Option<&'a serde_json::Value> {
            let mut current = parsed;
            for part in path.split('.') {
                let next = if let Ok(idx) = part.parse::<usize>() {
                    current.get(idx)
                } else {
                    current.get(part)
                };
                current = next?;
            }
            Some(current)
        }

        let parsed = serde_json::json!({
            "ok": true,
            "data": {
                "count": 4,
                "rows": [
                    {"slug": "post-1", "read_time": 5},
                    {"slug": "post-2", "read_time": 8}
                ]
            }
        });

        // Object access
        assert_eq!(
            json_get_by_path(&parsed, "ok"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            json_get_by_path(&parsed, "data.count"),
            Some(&serde_json::json!(4))
        );

        // Array indexing via numeric path segments
        let slug0 = json_get_by_path(&parsed, "data.rows.0.slug").unwrap();
        assert_eq!(slug0, &serde_json::json!("post-1"));

        let slug1 = json_get_by_path(&parsed, "data.rows.1.slug").unwrap();
        assert_eq!(slug1, &serde_json::json!("post-2"));

        let rt0 = json_get_by_path(&parsed, "data.rows.0.read_time").unwrap();
        assert_eq!(rt0, &serde_json::json!(5));

        // Out-of-bounds returns None
        assert!(json_get_by_path(&parsed, "data.rows.5.slug").is_none());

        // Missing key returns None
        assert!(json_get_by_path(&parsed, "data.missing").is_none());
    }

    // Regression: commit-scan-missing-repo-on-body-parse (fp 79ff683e).
    // The hypothesis in the report was that serde_json (used by _json_get)
    // mis-decodes UTF-8 multi-byte sequences (em-dash E2 80 94) when they
    // sit next to backticks, causing the top-level `repo` key to become
    // unreachable. This test uses the exact failing payload from the report
    // and asserts that repo/commit_sha/fingerprints are all still reachable.
    // If this test passes, the JSON bridge is NOT the cause and the bug
    // lives elsewhere (dashboard endpoint, request-body reader, framework).
    #[test]
    fn json_decode_survives_backtick_em_dash_utf8() {
        let payload = "{\n  \"repo\": \"clean-language/clean-errors\",\n  \"commit_sha\": \"abcdef1234567890abcdef1234567890abcdef12\",\n  \"commit_message\": \"fix(commit-scan): handle `backtick-quoted` fingerprints \u{2014} parse both forms\\n\\nPreviously the matcher only recognized `Bug: (fp)` \u{2014} this commit adds the\\n`Bug: fp` trailer form as well. See #1f887f527998 \u{2014} closes it.\",\n  \"author\": \"Ivan <ipasco@grafilab.com>\",\n  \"authored_at\": \"2026-07-16T12:00:00Z\",\n  \"fingerprints\": [\"1f887f527998\"]\n}";

        // Sanity: the payload really does contain both backticks and em-dashes.
        assert!(payload.contains('`'), "payload must contain backtick");
        assert!(payload.contains('\u{2014}'), "payload must contain em-dash");

        let parsed: serde_json::Value =
            serde_json::from_str(payload).expect("serde_json must parse the reported payload");

        assert_eq!(
            parsed.get("repo").and_then(|v| v.as_str()),
            Some("clean-language/clean-errors"),
            "repo key must be reachable"
        );
        assert_eq!(
            parsed.get("commit_sha").and_then(|v| v.as_str()),
            Some("abcdef1234567890abcdef1234567890abcdef12")
        );
        assert_eq!(
            parsed
                .get("fingerprints")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str()),
            Some("1f887f527998")
        );

        let commit_message = parsed
            .get("commit_message")
            .and_then(|v| v.as_str())
            .expect("commit_message must decode as a string");
        assert!(commit_message.contains('`'));
        assert!(commit_message.contains('\u{2014}'));
    }

    #[test]
    fn test_inject_loader_script_for_islands() {
        // SRV003: page with a data-island wrapper must get the loader script tag
        let html = r#"<html><body><div data-island="my-toolbar" data-client="on"><button>x</button></div></body></html>"#;
        let result = inject_loader_script(html);
        assert!(
            result
                .contains(r#"<script src="/loader.js" defer data-wasm="/frontend.wasm"></script>"#),
            "loader script must be injected when islands are present, got: {}",
            result
        );
        assert!(
            result.contains(r#"</script></body>"#),
            "loader script must be inserted before </body>, got: {}",
            result
        );

        // No islands → no injection
        let plain = "<html><body><p>hello</p></body></html>";
        assert_eq!(inject_loader_script(plain), plain);

        // Idempotent: page that already references /loader.js is left untouched
        let already = r#"<html><body><div data-island="x" data-client="on"></div><script src="/loader.js"></script></body></html>"#;
        assert_eq!(inject_loader_script(already), already);

        // No </body> tag: script is appended at the end
        let fragment = r#"<div data-island="x" data-client="on"></div>"#;
        let result = inject_loader_script(fragment);
        assert!(
            result.ends_with(
                r#"<script src="/loader.js" defer data-wasm="/frontend.wasm"></script>"#
            )
        );

        // Case-insensitive </body> matching
        let upper = r#"<HTML><BODY><div data-island="x" data-client="on"></div></BODY></HTML>"#;
        let result = inject_loader_script(upper);
        assert!(result.contains(
            r#"<script src="/loader.js" defer data-wasm="/frontend.wasm"></script></BODY>"#
        ));
    }

    #[test]
    fn test_process_if_directive_preserves_element_structure() {
        let data = serde_json::json!({"show": true, "hide": false});

        // Truthy: full element (with tag, attributes, content) must be preserved; cl-if attr removed
        let html = r#"<a href="/home" class="nav-link" cl-if="show">Home</a>"#;
        let result = process_if_directive(html, &data);
        assert_eq!(
            result, r#"<a href="/home" class="nav-link">Home</a>"#,
            "truthy cl-if must keep full element, not just inner text"
        );

        // Falsy: entire element removed
        let html = r#"<a href="/home" cl-if="hide">Home</a>"#;
        let result = process_if_directive(html, &data);
        assert_eq!(result, "", "falsy cl-if must remove the entire element");

        // cl-if + cl-else truthy: keep if-element, remove else-element
        let html = r#"<span cl-if="show">Yes</span> <span cl-else>No</span>"#;
        let result = process_if_directive(html, &data);
        assert_eq!(
            result, "<span>Yes</span>",
            "truthy: keep if-element, strip cl-else sibling"
        );

        // cl-if + cl-else falsy: remove if-element, keep else-element (cl-else attr stripped)
        let html = r#"<span cl-if="hide">Yes</span> <span cl-else>No</span>"#;
        let result = process_if_directive(html, &data);
        assert_eq!(
            result, "<span>No</span>",
            "falsy: keep else-element without cl-else attr"
        );
    }

    // --- Registry TOML types ---

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    struct Registry {
        meta: RegistryMeta,
        functions: Vec<FunctionEntry>,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    struct RegistryMeta {
        version: String,
        generated_from: Vec<String>,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    struct FunctionEntry {
        name: String,
        layer: u32,
        category: String,
        module: String,
        params: Vec<String>,
        returns: String,
        #[serde(default)]
        aliases: Vec<String>,
        #[serde(default = "default_expand_strings")]
        expand_strings: bool,
        description: String,
    }

    /// Default for the `expand_strings` field on registry entries.
    ///
    /// Historical default: every `"string"` param expands to a pointer+length
    /// pair `(i32, i32)`. Only plugin-declared bridges that opt out
    /// (`expand_strings = false`, e.g. `_json_get`) collapse to a bare `i32`.
    fn default_expand_strings() -> bool {
        true
    }

    /// Expand a registry high-level param type to WASM types.
    ///
    /// `expand_strings` controls how `"string"` is materialized in the WASM
    /// signature. When `true` (the default), a `"string"` param is emitted as
    /// the traditional pointer+length pair `(i32, i32)`. When `false` (used by
    /// plugin-declared bridges that ship `expand_strings = false` such as
    /// `_json_get`), the compiler passes a single length-prefixed pointer, so
    /// the WASM param is a bare `i32`.
    fn expand_param_type(t: &str, expand_strings: bool) -> Vec<&str> {
        match t {
            "string" => {
                if expand_strings {
                    vec!["i32", "i32"]
                } else {
                    vec!["i32"]
                }
            }
            "integer" => vec!["i64"],
            "number" => vec!["f64"],
            "boolean" => vec!["i32"],
            "i32" => vec!["i32"],
            "i64" => vec!["i64"],
            "any" => vec!["i32"],
            other => panic!("Unknown param type in registry: '{}'", other),
        }
    }

    fn expand_return_type(t: &str) -> Option<&str> {
        match t {
            "void" => None,
            "ptr" => Some("i32"),
            "string" => Some("i32"), // string return = ptr to length-prefixed string
            "i32" => Some("i32"),
            "i64" => Some("i64"),
            "boolean" => Some("i32"),
            "integer" => Some("i64"),
            "number" => Some("f64"),
            "any" => Some("i32"),
            other => panic!("Unknown return type in registry: '{}'", other),
        }
    }

    fn generate_wat_import(
        module: &str,
        name: &str,
        params: &[String],
        returns: &str,
        expand_strings: bool,
    ) -> String {
        let mut import = format!("  (import \"{}\" \"{}\" (func", module, name);

        let wasm_params: Vec<&str> = params
            .iter()
            .flat_map(|t| expand_param_type(t, expand_strings))
            .collect();

        if !wasm_params.is_empty() {
            import.push_str(" (param");
            for p in &wasm_params {
                import.push_str(&format!(" {}", p));
            }
            import.push(')');
        }

        if let Some(ret) = expand_return_type(returns) {
            import.push_str(&format!(" (result {})", ret));
        }

        import.push_str("))\n");
        import
    }

    /// Layer 3 spec compliance test: validates that ALL server-specific host
    /// function signatures match the shared function registry.
    ///
    /// This is the Layer 3 counterpart to host-bridge's Layer 2 test.
    /// It generates WAT imports for Layer 3 functions and instantiates
    /// against the full server linker (which includes both L2 and L3 functions).
    #[test]
    fn test_layer3_spec_compliance() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let registry_path = std::path::Path::new(manifest_dir)
            .join("../foundation/spec/platform/function-registry.toml");
        let toml_str = std::fs::read_to_string(&registry_path).unwrap_or_else(|e| {
            panic!(
                "Failed to read function-registry.toml at {:?}: {}",
                registry_path, e
            )
        });

        let registry: Registry =
            toml::from_str(&toml_str).expect("Failed to parse function-registry.toml");

        // Filter for Layer 3 functions only (server-specific scope)
        let layer3_funcs: Vec<&FunctionEntry> =
            registry.functions.iter().filter(|f| f.layer == 3).collect();

        assert!(
            layer3_funcs.len() >= 30,
            "Expected at least 30 Layer 3 canonical functions in registry, found {}",
            layer3_funcs.len()
        );

        // Generate WAT module with all Layer 3 imports
        let mut wat = String::from("(module\n");
        let mut import_count = 0;

        for func in &layer3_funcs {
            wat.push_str(&generate_wat_import(
                &func.module,
                &func.name,
                &func.params,
                &func.returns,
                func.expand_strings,
            ));
            import_count += 1;

            for alias in &func.aliases {
                wat.push_str(&generate_wat_import(
                    &func.module,
                    alias,
                    &func.params,
                    &func.returns,
                    func.expand_strings,
                ));
                import_count += 1;
            }
        }

        wat.push_str(")\n");

        // Create full server linker (includes L2 + L3) and validate
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");
        let module = wasmtime::Module::new(&engine, &wat).unwrap_or_else(|e| {
            panic!(
                "Failed to parse generated WAT ({} imports): {}\n\nGenerated WAT:\n{}",
                import_count, e, wat
            )
        });

        let router = std::sync::Arc::new(crate::router::Router::new());
        let state = WasmState::new(router);
        let mut store = wasmtime::Store::new(&engine, state);

        linker.instantiate(&mut store, &module).unwrap_or_else(|e| {
            panic!(
                "LAYER 3 SPEC COMPLIANCE FAILURE ({} imports):\n{}\n\n\
             Fix the implementation to match function-registry.toml, not the other way around.",
                import_count, e
            )
        });

        eprintln!(
            "Layer 3 spec compliance PASSED: {} canonical + aliases = {} total imports",
            layer3_funcs.len(),
            import_count
        );
    }

    // -----------------------------------------------------------------------
    // SRV001 — component_tag_render dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_render_export_name_strips_hyphens() {
        // Default pattern from frame.ui plugin.toml.
        assert_eq!(
            resolve_render_export_name("my-component", "{tagname}_render"),
            "mycomponent_render"
        );
        assert_eq!(
            resolve_render_export_name("user-badge", "{tagname}_render"),
            "userbadge_render"
        );
        // No hyphen ⇒ no change to the base name.
        assert_eq!(
            resolve_render_export_name("counter", "{tagname}_render"),
            "counter_render"
        );
        // Multiple hyphens collapsed.
        assert_eq!(
            resolve_render_export_name("very-long-tag-name", "{tagname}_render"),
            "verylongtagname_render"
        );
        // Alternate pattern still substitutes.
        assert_eq!(
            resolve_render_export_name("my-thing", "render_{tagname}"),
            "render_mything"
        );
    }

    #[test]
    fn marshal_attrs_as_json_handles_no_attrs() {
        assert_eq!(marshal_attrs_as_json("<my-tag>", "my-tag"), "{}");
        assert_eq!(marshal_attrs_as_json("<my-tag/>", "my-tag"), "{}");
        assert_eq!(marshal_attrs_as_json("<my-tag />", "my-tag"), "{}");
    }

    // With `serde_json`'s `preserve_order` feature enabled (required for D3
    // insertion-order compliance in JSON v2 bridges), `serde_json::Map`
    // outputs keys in insertion order — i.e. HTML source order for
    // marshal_attrs_as_json. The export consumes the JSON by key lookup, so
    // the on-the-wire order is functionally irrelevant — these tests pin
    // the deterministic output for regression-detection.

    #[test]
    fn marshal_attrs_as_json_double_quoted() {
        let json = marshal_attrs_as_json(r#"<my-tag name="bob" count="3">"#, "my-tag");
        assert_eq!(json, r#"{"name":"bob","count":"3"}"#);
    }

    #[test]
    fn marshal_attrs_as_json_single_quoted_and_boolean() {
        let json = marshal_attrs_as_json("<my-tag name='alice' disabled count='7'>", "my-tag");
        assert_eq!(json, r#"{"name":"alice","disabled":"","count":"7"}"#);
    }

    #[test]
    fn marshal_attrs_as_json_self_closing() {
        let json = marshal_attrs_as_json(r#"<my-tag label="x" />"#, "my-tag");
        assert_eq!(json, r#"{"label":"x"}"#);
    }

    #[test]
    fn marshal_attrs_as_json_unquoted_value() {
        let json = marshal_attrs_as_json("<my-tag count=3 name=bob>", "my-tag");
        assert_eq!(json, r#"{"count":"3","name":"bob"}"#);
    }

    #[test]
    fn marshal_attrs_as_json_with_extra_whitespace() {
        let json = marshal_attrs_as_json("<my-tag   name=\"bob\"   count=\"3\"  >", "my-tag");
        assert_eq!(json, r#"{"name":"bob","count":"3"}"#);
    }

    // Regression: SERVER-UI-RENDER-PAGE-INTERP-STRICT-WHITESPACE
    // Spec (HOST_BRIDGE.md §UI templates): `{key}` single-brace syntax; dotted
    // paths supported; missing keys produce empty string; `{{` and `}}` escape
    // literal braces.

    #[test]
    fn substitute_template_single_brace() {
        let data = serde_json::json!({"greeting": "hello"});
        assert_eq!(
            substitute_template("<p>{greeting}</p>", &data),
            "<p>hello</p>"
        );
    }

    #[test]
    fn substitute_template_dotted_path() {
        let data = serde_json::json!({"user": {"name": "Ada"}});
        assert_eq!(substitute_template("Hi {user.name}!", &data), "Hi Ada!");
    }

    #[test]
    fn substitute_template_missing_key_becomes_empty_string() {
        let data = serde_json::json!({"present": "x"});
        assert_eq!(substitute_template("a{missing}b", &data), "ab");
    }

    #[test]
    fn substitute_template_missing_nested_path_becomes_empty() {
        let data = serde_json::json!({"user": {}});
        assert_eq!(substitute_template("[{user.name}]", &data), "[]");
    }

    #[test]
    fn substitute_template_multiple_substitutions() {
        let data = serde_json::json!({"a": "1", "b": "2"});
        assert_eq!(
            substitute_template("{a} and {b} and {a}", &data),
            "1 and 2 and 1"
        );
    }

    #[test]
    fn substitute_template_non_string_values_stringify() {
        let data = serde_json::json!({"n": 42, "f": 3.5, "t": true, "z": null});
        assert_eq!(
            substitute_template("{n}|{f}|{t}|{z}|", &data),
            "42|3.5|true||"
        );
    }

    #[test]
    fn substitute_template_unclosed_brace_left_literal() {
        let data = serde_json::json!({"x": "y"});
        assert_eq!(
            substitute_template("before { unclosed text", &data),
            "before { unclosed text"
        );
    }

    #[test]
    fn substitute_template_escape_double_brace() {
        let data = serde_json::json!({"x": "y"});
        // `{{` → literal `{`, `}}` → literal `}`. Both pairs in `{{x}}`
        // are escapes, so the input renders as literal `{x}` (no
        // substitution). To wrap a substitution in literal braces, the
        // user writes `{{{x}}}` (escape + placeholder + escape).
        assert_eq!(substitute_template("{{x}}", &data), "{x}");
        assert_eq!(substitute_template("{{{x}}}", &data), "{y}");
        assert_eq!(
            substitute_template("price: {{USD}} {x}", &data),
            "price: {USD} y"
        );
    }

    #[test]
    fn substitute_template_preserves_utf8() {
        let data = serde_json::json!({"name": "世界"});
        assert_eq!(
            substitute_template("¡Hola, {name}! 🎉", &data),
            "¡Hola, 世界! 🎉"
        );
    }

    #[test]
    fn substitute_template_empty_key_left_literal() {
        let data = serde_json::json!({});
        assert_eq!(substitute_template("a{}b{   }c", &data), "a{}b{   }c");
    }

    #[test]
    fn substitute_template_newline_inside_braces_left_literal() {
        let data = serde_json::json!({"x": "y"});
        assert_eq!(
            substitute_template("before {\nbroken} after", &data),
            "before {\nbroken} after"
        );
    }

    // --- cl-iterate wrapper preservation (RUNTIME-SSR-NEW-DIRECTIVES-NOT-IMPLEMENTED) ---

    #[test]
    fn iterate_preserves_wrapper_for_object_items() {
        let data = serde_json::json!({"products": [
            {"name": "Apple", "price": "1.00"},
            {"name": "Banana", "price": "0.50"}
        ]});
        let html = r#"<ul><li cl-iterate="product in products" class="card">{product.name} - {product.price}</li></ul>"#;
        let result = process_iterate_directive(html, &data);
        assert_eq!(
            result,
            r#"<ul><li class="card">Apple - 1.00</li><li class="card">Banana - 0.50</li></ul>"#,
            "cl-iterate must repeat the wrapper element with attributes (minus cl-iterate) preserved"
        );
    }

    #[test]
    fn iterate_preserves_wrapper_for_scalar_items() {
        let data = serde_json::json!({"fruits": ["apple", "banana"]});
        let html = r#"<ul><li cl-iterate="fruit in fruits">{fruit}</li></ul>"#;
        let result = process_iterate_directive(html, &data);
        assert_eq!(
            result, "<ul><li>apple</li><li>banana</li></ul>",
            "scalar iteration must keep the wrapper per item"
        );
    }

    #[test]
    fn iterate_can_reference_outer_scope() {
        let data = serde_json::json!({
            "prefix": "Item",
            "items": ["a", "b"]
        });
        let html = r#"<ul><li cl-iterate="x in items">{prefix}: {x}</li></ul>"#;
        let result = process_iterate_directive(html, &data);
        assert_eq!(result, "<ul><li>Item: a</li><li>Item: b</li></ul>");
    }

    #[test]
    fn iterate_with_empty_array_removes_element() {
        let data = serde_json::json!({"items": []});
        let html = r#"<ul><li cl-iterate="x in items">{x}</li></ul>"#;
        let result = process_iterate_directive(html, &data);
        assert_eq!(
            result, "<ul></ul>",
            "empty array must produce zero iterations"
        );
    }

    #[test]
    fn iterate_missing_array_path_renders_zero_iterations() {
        let data = serde_json::json!({});
        let html = r#"<div><span cl-iterate="x in missing">{x}</span></div>"#;
        let result = process_iterate_directive(html, &data);
        assert_eq!(result, "<div></div>");
    }

    // --- End-to-end: render_page pipeline (directives → substitution) ---

    #[test]
    fn render_pipeline_handles_spec_canonical_pattern() {
        // Mirrors the failing repro from RUNTIME-SSR-NEW-DIRECTIVES-NOT-IMPLEMENTED.
        let data = serde_json::json!({
            "title": "hi",
            "fruits": ["apple", "banana"]
        });
        let template =
            "<h1>{title}</h1>\n<ul>\n  <li cl-iterate=\"fruit in fruits\">{fruit}</li>\n</ul>";
        let with_directives = process_directives(template, &data);
        let result = substitute_template(&with_directives, &data);
        assert_eq!(
            result,
            "<h1>hi</h1>\n<ul>\n  <li>apple</li><li>banana</li>\n</ul>"
        );
    }
}
