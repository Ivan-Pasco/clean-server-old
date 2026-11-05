//! WASM Linker Host Functions
//!
//! Provides all host functions required by Clean Language WASM modules.
//! This module creates wasmtime-compatible host function bindings.
//!
//! ## Generic Architecture
//!
//! All registration functions are generic over `WasmStateCore`, allowing any
//! runtime (CLI, server, embedded) to use them with their own state type.
//!
//! ## Host Function Categories
//!
//! ### Platform I/O (requires host access)
//! - Console: print, printl, input, input_*
//! - File: file_read, file_write, file_exists, file_delete, file_append
//! - HTTP Client: http_get, http_post, etc.
//! - Database: _db_query, _db_execute, etc.
//!
//! ### Advanced Math (no native WASM instructions)
//! - Trigonometric: sin, cos, tan, asin, acos, atan, atan2
//! - Hyperbolic: sinh, cosh, tanh
//! - Logarithmic: ln, log10, log2, exp, exp2, pow, sqrt
//!
//! ### String Allocation (requires memory management)
//! - concat, substring, trim, toUpper, toLower, replace, split
//!
//! ### Memory Runtime
//! - mem_alloc, mem_retain, mem_release, mem_scope_push, mem_scope_pop

mod array_funcs;
mod console;
mod crypto_funcs;
mod database;
mod env_time;
mod file_io;
mod helpers;
mod http_client;
mod list_funcs;
mod math;
mod memory;
mod state;
mod string_ops;

pub use array_funcs::reset_array_store;
pub use list_funcs::reset_list_store;
// NOTE: HTTP Server functions (Layer 3) are NOT in host-bridge.
// They are server-specific and implemented in clean-server/src/bridge.rs.
// See foundation/spec/platform/EXECUTION_LAYERS.md for layer definitions.

// Re-export core types
pub use helpers::{
    read_length_prefixed_bytes, read_raw_bytes, read_raw_string, read_string_from_caller,
    write_bytes_to_caller, write_string_to_caller, STRING_LENGTH_PREFIX_SIZE,
};
pub use state::{
    AuthContext, HttpResponseBuilder, RequestContext, SharedDbBridge, WasmMemory, WasmState,
    WasmStateCore,
};

use crate::error::BridgeResult;
use wasmtime::{Engine, Linker};

/// Register a bridge function under its canonical name and, when it follows the
/// `_namespace_fn` convention, also under its `namespace.fn` dot-notation alias.
///
/// This is the preferred way to add new `_*`-prefix bridge functions.  Using
/// the macro instead of a bare `linker.func_wrap` call ensures the dot alias is
/// never accidentally omitted (the compiler >= 0.30.120 emits both forms).
///
/// # Usage
///
/// ```ignore
/// register_bridge_fn!(linker, "env", "_db_query", |mut caller, ptr, len, p2, l2| { … })?;
/// ```
///
/// The macro calls `func_wrap` once (consuming the closure), then derives the
/// dot alias from the canonical name via `linker.alias()` — which does not
/// require the closure a second time.
///
/// Existing functions registered before this macro was introduced are covered
/// by the `register_dot_aliases` post-registration loop.
///
/// See `foundation/spec/platform/HOST_BRIDGE.md § Dual Naming`.
#[macro_export]
macro_rules! register_bridge_fn {
    ($linker:expr, $env:expr, $name:expr, $func:expr) => {{
        $linker.func_wrap($env, $name, $func)?;
        // Derive dot-notation alias: _namespace_fn → namespace.fn
        // Skip names that don't start with '_' or have no second '_' to pivot on.
        let _stripped: &str = $name.trim_start_matches('_');
        if $name.starts_with('_') && !$name.starts_with("__") {
            if let Some(_dot_idx) = _stripped.find('_') {
                let _dot_name =
                    format!("{}.{}", &_stripped[.._dot_idx], &_stripped[_dot_idx + 1..]);
                $linker.alias($env, $name, $env, &_dot_name)?;
            }
        }
    }};
}

/// Register all bridge host functions with a linker
///
/// This is the main entry point for any runtime. It registers:
/// - Console I/O (print, input)
/// - Math functions (sin, cos, pow, etc.)
/// - String operations (concat, substring, etc.)
/// - Memory runtime (mem_alloc, etc.)
/// - Database operations (_db_query, _db_execute, etc.)
/// - File I/O (file_read, file_write, etc.)
/// - HTTP client (http_get, http_post, etc.)
/// - Crypto (password hashing, etc.)
///
/// # Type Parameter
///
/// `S` must implement `WasmStateCore` which provides access to the memory allocator.
///
/// # Example
///
/// ```ignore
/// use host_bridge::wasm_linker::{register_all_functions, WasmState};
/// use wasmtime::{Engine, Linker};
///
/// let engine = Engine::default();
/// let mut linker = Linker::new(&engine);
/// register_all_functions(&mut linker)?;
/// ```
pub fn register_all_functions<S: WasmStateCore>(linker: &mut Linker<S>) -> BridgeResult<()> {
    // Core functions
    console::register_functions(linker)?;
    math::register_functions(linker)?;
    string_ops::register_functions(linker)?;
    memory::register_functions(linker)?;

    // Platform I/O
    database::register_functions(linker)?;
    file_io::register_functions(linker)?;
    http_client::register_functions(linker)?;
    crypto_funcs::register_functions(linker)?;
    env_time::register_functions(linker)?;
    list_funcs::register_functions(linker)?;
    array_funcs::register_functions(linker)?;

    // NOTE: HTTP Server functions (Layer 3) are NOT provided by host-bridge.
    // Server-specific functions like _req_param, _req_body, _http_route, etc.
    // must be implemented by the server runtime (e.g., clean-server/src/bridge.rs).
    // See foundation/spec/platform/EXECUTION_LAYERS.md for layer definitions.

    // Register dot-notation aliases (compiler >= 0.30.120 emits both forms).
    // See foundation/spec/platform/HOST_BRIDGE.md § Dual Naming.
    register_dot_aliases(linker)?;

    Ok(())
}

/// Register dot-notation aliases for all `_namespace_fn` bridge functions.
///
/// The Clean Language compiler (0.30.120+) generates WASM imports in both
/// `_namespace_fn` and `namespace.fn` styles. This loop registers the dot form
/// as an alias of the already-registered underscore form so both work.
///
/// Derived from function-registry.toml `aliases` field (Layer 2 entries).
/// Math and string dot aliases are registered inline in their own modules
/// because they predate this loop; only the newer `_*`-prefix groups are here.
fn register_dot_aliases<S: WasmStateCore>(linker: &mut Linker<S>) -> BridgeResult<()> {
    const ALIASES: &[(&str, &str)] = &[
        // HTML interpolation helpers (string_ops module)
        ("_html_escape", "html.escape"),
        ("_html_raw", "html.raw"),
        // Database (database module)
        ("_db_query", "db.query"),
        ("_db_execute", "db.execute"),
        ("_db_begin", "db.begin"),
        ("_db_commit", "db.commit"),
        ("_db_rollback", "db.rollback"),
        ("_db_register_migration", "db.register_migration"),
        ("_db_configure", "db.configure"),
        ("_db_paginate", "db.paginate"),
        ("_db_cursor_page", "db.cursorPage"),
        ("_db_migration_diff", "db.migration_diff"),
        ("_db_migration_status", "db.migration_status"),
        ("_db_rollback_migration", "db.rollback_migration"),
        ("_db_run_migrations", "db.run_migrations"),
        ("_db_valid_field", "db.valid_field"),
        // Crypto (crypto_funcs module)
        ("_crypto_hash_password", "crypto.hash_password"),
        ("_crypto_verify_password", "crypto.verify_password"),
        ("_crypto_random_bytes", "crypto.random_bytes"),
        ("_crypto_random_hex", "crypto.random_hex"),
        ("_crypto_hash_sha256", "crypto.hash_sha256"),
        ("_crypto_sha256_bytes", "crypto.sha256_bytes"),
        ("_crypto_hash_sha512", "crypto.hash_sha512"),
        ("_crypto_hmac", "crypto.hmac"),
        // Crypto extras (Phase 2)
        ("_crypto_uuid", "crypto.uuid"),
        ("_crypto_hash_md5", "crypto.hash_md5"),
        ("_crypto_hmac_sha256", "crypto.hmac_sha256"),
        ("_crypto_random_base64", "crypto.random_base64"),
        ("_crypto_base64_encode", "crypto.base64_encode"),
        ("_crypto_base64_decode", "crypto.base64_decode"),
        ("_crypto_encrypt_aes", "crypto.encrypt_aes"),
        ("_crypto_decrypt_aes", "crypto.decrypt_aes"),
        // JWT (crypto_funcs module)
        ("_jwt_sign", "jwt.sign"),
        ("_jwt_verify", "jwt.verify"),
        ("_jwt_decode", "jwt.decode"),
        // Environment and time (env_time module)
        ("_env_get", "env.get"),
        ("_time_now", "time.now"),
        // Env extras (Phase 2)
        ("_env_has", "env.has"),
        ("_env_all", "env.all"),
        ("_env_node_env", "env.node_env"),
        ("_env_is_production", "env.is_production"),
        ("_env_is_development", "env.is_development"),
        // Time extras (Phase 2)
        ("_time_epoch_ms", "time.epoch_ms"),
        ("_time_epoch_sec", "time.epoch_sec"),
        ("_time_iso", "time.iso"),
        ("_time_format_iso", "time.format_iso"),
        ("_time_parse_iso", "time.parse_iso"),
        ("_time_components", "time.components"),
        ("_time_from_components", "time.from_components"),
        ("_time_add", "time.add"),
        ("_time_format", "time.format"),
        ("_time_relative", "time.relative"),
        ("_time_parse", "time.parse"),
        ("_time_parts", "time.parts"),
        ("_time_diff", "time.diff"),
        ("_time_format_locale", "time.format_locale"),
        ("_time_timezone_offset", "time.timezone_offset"),
        ("_time_is_past", "time.is_past"),
        ("_time_is_future", "time.is_future"),
        ("_time_sleep", "time.sleep"),
        // DB async extras (Phase 2)
        ("_db_connected", "db.connected"),
        ("_db_query_async", "db.query_async"),
        ("_db_query_result", "db.query_result"),
        ("_db_execute_async", "db.execute_async"),
        ("_db_execute_result", "db.execute_result"),
        // HTTP _-prefixed aliases (registry declares both forms)
        ("http_put_with_headers", "_http_put_with_headers"),
        ("http_patch_with_headers", "_http_patch_with_headers"),
        ("http_delete_with_headers", "_http_delete_with_headers"),
    ];

    for (canonical, dot_alias) in ALIASES {
        linker.alias("env", canonical, "env", dot_alias)?;
    }

    Ok(())
}

/// Create a fully configured linker with all host functions
///
/// Convenience function that creates a new linker and registers all functions.
/// Uses the default `WasmState` type from host-bridge.
pub fn create_linker(engine: &Engine) -> BridgeResult<Linker<WasmState>> {
    let mut linker = Linker::new(engine);
    register_all_functions(&mut linker)?;
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Module, Store};

    #[test]
    fn test_create_linker() {
        let engine = Engine::default();
        let linker = create_linker(&engine);
        assert!(linker.is_ok());
    }

    // --- Registry TOML types ---

    #[derive(serde::Deserialize)]
    struct Registry {
        #[allow(dead_code)]
        meta: RegistryMeta,
        functions: Vec<FunctionEntry>,
    }

    #[derive(serde::Deserialize)]
    struct RegistryMeta {
        #[allow(dead_code)]
        version: String,
        #[allow(dead_code)]
        generated_from: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    struct FunctionEntry {
        name: String,
        layer: u32,
        #[allow(dead_code)]
        category: String,
        module: String,
        params: Vec<String>,
        returns: String,
        #[serde(default)]
        aliases: Vec<String>,
        #[serde(default = "default_hosts_all")]
        hosts: Vec<String>,
        #[serde(default = "default_expand_strings")]
        expand_strings: bool,
        #[allow(dead_code)]
        description: String,
    }

    fn default_hosts_all() -> Vec<String> {
        vec!["all".to_string()]
    }

    /// Default for the `expand_strings` field on registry entries.
    /// See the Layer 3 test for the full explanation — mirrored here so the
    /// Layer 2 test doesn't silently break the day a Layer 2 entry opts out.
    fn default_expand_strings() -> bool {
        true
    }

    /// True when this function's `hosts` field includes this host class
    /// ("all" or the specific class, here "server").
    fn applies_to_host(entry: &FunctionEntry, host: &str) -> bool {
        entry.hosts.iter().any(|h| h == "all" || h == host)
    }

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
            // A `ptr` parameter is a single i32 pointing into linear memory —
            // the callee reads a length prefix (or other framing) from that
            // address. This mirrors the return-type expansion.
            "ptr" => vec!["i32"],
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

    /// Spec compliance test: validates that ALL Layer 2 host function signatures
    /// match the shared function registry (foundation/spec/platform/function-registry.toml).
    ///
    /// The registry defines every function with high-level types that expand to
    /// exact WASM signatures. This test dynamically generates a WAT module from
    /// the registry and instantiates it against the linker. If any signature in
    /// the implementation differs from the registry, instantiation fails.
    ///
    /// To update: modify function-registry.toml first, then the implementation.
    /// Never change the registry just to pass tests.
    #[test]
    fn test_spec_compliance() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let registry_path = std::path::Path::new(manifest_dir)
            .join("../../foundation/spec/platform/function-registry.toml");
        let toml_str = std::fs::read_to_string(&registry_path).unwrap_or_else(|e| {
            panic!(
                "Failed to read function-registry.toml at {:?}: {}",
                registry_path, e
            )
        });

        let registry: Registry =
            toml::from_str(&toml_str).expect("Failed to parse function-registry.toml");

        // Filter for Layer 2 functions implemented by host-bridge.
        //
        // host-bridge is the portable subset of Layer 2: console, math, string,
        // memory, database, file I/O, HTTP client, crypto, env, time, list, array, jwt, html.
        // Other Layer 2 categories (canvas, audio, anim, sprite, ui, i18n, locale, asset,
        // camera, input, page, state, build_state, etc.) are framework- or browser-runtime
        // concerns. The full clean-server linker covers those via bridge_ui_stubs.rs /
        // bridge_canvas_stubs.rs; the Layer 3 spec_compliance test in bridge.rs validates
        // them against the full linker. The host filter additionally drops browser-only
        // entries that might otherwise leak in.
        const HOST_BRIDGE_CATEGORIES: &[&str] = &[
            "console",
            "math",
            "string",
            "memory",
            "database",
            "file_io",
            "http_client",
            "crypto",
            "env",
            "time",
            "list",
            "array",
            "jwt",
            "html",
        ];
        let layer2_funcs: Vec<&FunctionEntry> = registry
            .functions
            .iter()
            .filter(|f| f.layer == 2)
            .filter(|f| HOST_BRIDGE_CATEGORIES.contains(&f.category.as_str()))
            .filter(|f| applies_to_host(f, "server"))
            .collect();

        assert!(
            layer2_funcs.len() >= 80,
            "Expected at least 80 Layer 2 canonical functions in registry, found {}",
            layer2_funcs.len()
        );

        // Generate WAT module with all Layer 2 imports
        let mut wat = String::from("(module\n");
        let mut import_count = 0;

        for func in &layer2_funcs {
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

        // Create linker and validate all signatures
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("Failed to create linker");
        let module = Module::new(&engine, &wat).unwrap_or_else(|e| {
            panic!(
                "Failed to parse generated WAT ({} imports): {}\n\nGenerated WAT:\n{}",
                import_count, e, wat
            )
        });

        let mut store = Store::new(&engine, WasmState::default());

        linker.instantiate(&mut store, &module).unwrap_or_else(|e| {
            panic!(
                "SPEC COMPLIANCE FAILURE ({} Layer 2 imports):\n{}\n\n\
             Fix the implementation to match function-registry.toml, not the other way around.",
                import_count, e
            )
        });

        eprintln!(
            "Layer 2 spec compliance PASSED: {} canonical + aliases = {} total imports",
            layer2_funcs.len(),
            import_count
        );
    }
}
