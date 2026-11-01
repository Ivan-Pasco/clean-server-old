//! Bridge Contract Test
//!
//! Verifies that every function and alias defined in `function-registry.toml`
//! is resolvable in the server linker.  Unlike `bridge_compliance_test.rs`
//! (which stops at the first failure), this test collects **all** missing
//! registrations and reports them together, making it easier to spot
//! systematic omissions (e.g. an entire alias group missing).
//!
//! This is the load-bearing test for dual-name registration: a new bridge
//! function added without its dot-notation alias will be caught here before
//! it causes a production `LinkError`.
//!
//! See `foundation/spec/platform/HOST_BRIDGE.md § Dual Naming`.

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::WasmState;
use std::path::PathBuf;
use std::sync::Arc;
use wasmtime::{Engine, Module, Store};

// ---------------------------------------------------------------------------
// Registry types
// ---------------------------------------------------------------------------

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
    #[allow(dead_code)]
    description: String,
    // Empty = unrestricted (backward compat with pre-2026-06 entries).
    // See registry header "HOST SCOPING" section.
    #[serde(default)]
    hosts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Type-expansion convention selector.
///
/// The registry uses the same word "integer" for two different WASM types
/// depending on which layer the function belongs to:
///
/// - **HostBridge (Layer 2 host functions like `math_sin`, `db_query`)** —
///   `integer` expands to `i64`. This is the wide native register width used
///   for portable host bridge signatures.
/// - **PluginBridge (frame.ui / frame.canvas helpers with `hosts = ["browser", ...]`
///   or plugin-owned server stubs)** — `integer` expands to `i32`. This
///   matches the compiler's plugin-bridge convention: plugin-side function
///   ABIs are compiled to `i32` so they fit the browser JS bridge cleanly,
///   and the server no-op stubs must match the compiler-emitted import.
///
/// Both conventions coexist in the same registry file; the `hosts` field
/// tells them apart. Anything with `browser` in `hosts` is a plugin-side
/// function → PluginBridge convention. Everything else (pure server layer 2
/// or 3) → HostBridge convention.
#[derive(Copy, Clone)]
enum TypeConvention {
    HostBridge,
    PluginBridge,
}

fn convention_for(hosts: &[String]) -> TypeConvention {
    if hosts.iter().any(|h| h == "browser") {
        TypeConvention::PluginBridge
    } else {
        TypeConvention::HostBridge
    }
}

fn expand_param_type(t: &str, conv: TypeConvention) -> Vec<&str> {
    match t {
        "string" => vec!["i32", "i32"],
        "integer" => match conv {
            TypeConvention::HostBridge => vec!["i64"],
            TypeConvention::PluginBridge => vec!["i32"],
        },
        "number" => vec!["f64"],
        "boolean" => vec!["i32"],
        "i32" => vec!["i32"],
        "i64" => vec!["i64"],
        "any" => vec!["i32"],
        // "ptr" param is a single i32 pointing into linear memory — the
        // callee reads a length prefix (or other framing) from that address.
        "ptr" => vec!["i32"],
        other => panic!("Unknown param type in registry: '{}'", other),
    }
}

fn expand_return_type(t: &str, conv: TypeConvention) -> Option<&str> {
    match t {
        "void" => None,
        "ptr" => Some("i32"),
        "string" => Some("i32"), // string return = ptr to length-prefixed string
        "i32" => Some("i32"),
        "i64" => Some("i64"),
        "boolean" => Some("i32"),
        "integer" => match conv {
            TypeConvention::HostBridge => Some("i64"),
            TypeConvention::PluginBridge => Some("i32"),
        },
        "number" => Some("f64"),
        "any" => Some("i32"),
        other => panic!("Unknown return type in registry: '{}'", other),
    }
}

fn single_import_wat(
    module: &str,
    name: &str,
    params: &[String],
    returns: &str,
    conv: TypeConvention,
) -> String {
    let mut wat = format!("(module\n  (import \"{}\" \"{}\" (func", module, name);
    let wasm_params: Vec<&str> = params
        .iter()
        .flat_map(|t| expand_param_type(t, conv))
        .collect();
    if !wasm_params.is_empty() {
        wat.push_str(" (param");
        for p in &wasm_params {
            wat.push_str(&format!(" {}", p));
        }
        wat.push(')');
    }
    if let Some(ret) = expand_return_type(returns, conv) {
        wat.push_str(&format!(" (result {})", ret));
    }
    wat.push_str("))\n)\n");
    wat
}

fn make_store(engine: &Engine) -> Store<WasmState> {
    let router = Arc::new(Router::new());
    Store::new(engine, WasmState::new(router))
}

// ---------------------------------------------------------------------------
// The contract test
// ---------------------------------------------------------------------------

/// Verify that every canonical function and every alias in `function-registry.toml`
/// is registered in the server linker.
///
/// Each function/alias is probed individually so **all** missing registrations
/// are collected and reported in one failure message, rather than stopping at
/// the first absent import.
///
/// This is the enforcement gate for the dual-name registration convention
/// (`register_bridge_fn!` macro).  It catches:
/// - A new `_namespace_fn` added without its `namespace.fn` alias.
/// - A function removed from the implementation but left in the registry.
/// - A signature mismatch between the registry and the implementation.
#[test]
fn bridge_covers_registry() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let registry_path = manifest_dir.join("../foundation/spec/platform/function-registry.toml");

    let toml_str = std::fs::read_to_string(&registry_path).unwrap_or_else(|e| {
        panic!(
            "bridge_covers_registry: Cannot read function-registry.toml at {:?}: {}",
            registry_path, e
        )
    });

    let registry: Registry = toml::from_str(&toml_str)
        .expect("bridge_covers_registry: Failed to parse function-registry.toml");

    let engine = Engine::default();
    let linker =
        create_linker(&engine).expect("bridge_covers_registry: Failed to create server linker");

    let mut missing: Vec<String> = Vec::new();

    for func in registry.functions.iter().filter(|f| {
        (f.layer == 2 || f.layer == 3)
            && (f.hosts.is_empty() || f.hosts.iter().any(|h| h == "server"))
    }) {
        let conv = convention_for(&func.hosts);
        // Probe canonical name.
        let wat = single_import_wat(&func.module, &func.name, &func.params, &func.returns, conv);
        if let Ok(module) = Module::new(&engine, &wat) {
            let mut store = make_store(&engine);
            if linker.instantiate(&mut store, &module).is_err() {
                missing.push(format!("canonical  {}::{}", func.module, func.name));
            }
        }

        // Probe every alias.
        for alias in &func.aliases {
            let wat = single_import_wat(&func.module, alias, &func.params, &func.returns, conv);
            if let Ok(module) = Module::new(&engine, &wat) {
                let mut store = make_store(&engine);
                if linker.instantiate(&mut store, &module).is_err() {
                    missing.push(format!(
                        "alias      {}::{} (of {})",
                        func.module, alias, func.name
                    ));
                }
            }
        }
    }

    assert!(
        missing.is_empty(),
        "bridge_covers_registry FAILED — {} registration(s) missing from the server linker:\n\n{}\n\n\
         Add each missing function/alias to the appropriate registration function.\n\
         Use the register_bridge_fn! macro for new _namespace_fn functions.\n\
         Never modify function-registry.toml to make this test pass.",
        missing.len(),
        missing.join("\n")
    );

    eprintln!(
        "bridge_covers_registry PASSED — all registry entries resolved by the server linker."
    );
}

/// The server registers no-op stubs for browser-hosted frame.client `_api_*`
/// bridges so modules that also contain client code (form:/send: blocks)
/// instantiate on the server. Those stubs must match the compiler-emitted
/// import exactly, which follows the PluginBridge convention (`integer` →
/// `i32`) for every registry entry with `browser` in `hosts`.
#[test]
fn api_stubs_match_registry_signatures() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let registry_path = manifest_dir.join("../foundation/spec/platform/function-registry.toml");
    let toml_str =
        std::fs::read_to_string(&registry_path).expect("Cannot read function-registry.toml");
    let registry: Registry =
        toml::from_str(&toml_str).expect("Failed to parse function-registry.toml");

    let engine = Engine::default();
    let linker = create_linker(&engine).expect("Failed to create server linker");

    let api_funcs: Vec<&FunctionEntry> = registry
        .functions
        .iter()
        .filter(|f| f.name.starts_with("_api_"))
        .collect();
    assert!(
        !api_funcs.is_empty(),
        "registry must declare the frame.client _api_* bridges"
    );

    let mut mismatched: Vec<String> = Vec::new();
    for func in api_funcs {
        let conv = convention_for(&func.hosts);
        let wat = single_import_wat(&func.module, &func.name, &func.params, &func.returns, conv);
        let module = Module::new(&engine, &wat).expect("generated WAT must be valid");
        let mut store = make_store(&engine);
        if let Err(e) = linker.instantiate(&mut store, &module) {
            mismatched.push(format!("{}::{} — {}", func.module, func.name, e));
        }
    }

    assert!(
        mismatched.is_empty(),
        "{} _api_* stub(s) do not match function-registry.toml:\n{}",
        mismatched.len(),
        mismatched.join("\n")
    );
}
