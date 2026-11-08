//! UI Bridge Stub Coverage Test
//!
//! Verifies that the server linker registers a working stub for every
//! browser-side frame.ui bridge function, so that WASM modules compiled with
//! `component: client="on"` / `events:` blocks link and run on the server.
//!
//! Without these stubs, any WASM module compiled with a client-hydration
//! component fails `Linker::instantiate` with
//! `unknown import: env::_ui_on_event`. This test catches missing stubs, wrong
//! signatures and trapping stubs before they cause production failures.
//!
//! # Source of truth
//!
//! The set of browser-side UI bridges comes from the function registry
//! (`foundation/spec/platform/function-registry.toml`): every entry in the
//! frame.ui categories (`ui`, `ui_browser`, `storage`) whose `hosts` field
//! includes `"browser"`. That covers both the SSR-stubbed entries
//! (`hosts = ["browser", "server"]`) and the browser-only entries
//! (`hosts = ["browser"]`) that the `events:` DSL may still emit into
//! server-mode WASM (`_ui_navigate`, `_ui_shortcut_*`, ...).
//!
//! # How It Works
//!
//! 1. Reads the registry and selects the browser-side UI entries.
//! 2. Generates one synthetic WAT module importing every function (with the
//!    plugin-bridge type convention, `integer` → `i32`, which is what the
//!    compiler emits for browser-hosted bridges) plus one exported probe per
//!    import that calls it with zero arguments.
//! 3. Instantiates it against the full server linker — fails with the precise
//!    missing import if any stub is absent or has the wrong signature.
//! 4. Calls every probe — fails listing every stub that traps.

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::WasmState;
use std::path::PathBuf;
use std::sync::Arc;
use wasmtime::{Engine, Module, Store};

/// Registry categories that make up the frame.ui bridge surface.
const UI_CATEGORIES: &[&str] = &["ui", "ui_browser", "storage"];

#[derive(serde::Deserialize)]
struct Registry {
    functions: Vec<FunctionEntry>,
}

#[derive(serde::Deserialize)]
struct FunctionEntry {
    name: String,
    category: String,
    module: String,
    params: Vec<String>,
    returns: String,
    #[serde(default)]
    hosts: Vec<String>,
}

fn load_browser_ui_entries() -> Vec<FunctionEntry> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let registry_path = manifest_dir.join("../foundation/spec/platform/function-registry.toml");
    let text = std::fs::read_to_string(&registry_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", registry_path.display(), e));
    let registry: Registry = toml::from_str(&text).expect("Failed to parse function-registry.toml");
    registry
        .functions
        .into_iter()
        .filter(|f| {
            UI_CATEGORIES.contains(&f.category.as_str()) && f.hosts.iter().any(|h| h == "browser")
        })
        .collect()
}

/// Browser-hosted bridges follow the plugin-bridge convention: `integer` is
/// an `i32` (see `bridge_contract_test.rs::TypeConvention`).
fn expand_param_type(t: &str) -> Vec<&'static str> {
    match t {
        "string" => vec!["i32", "i32"],
        "integer" | "boolean" | "i32" | "ptr" | "any" => vec!["i32"],
        "i64" => vec!["i64"],
        "number" => vec!["f64"],
        other => panic!("Unknown param type in function-registry.toml: '{}'", other),
    }
}

fn expand_return_type(t: &str) -> Option<&'static str> {
    match t {
        "" | "void" => None,
        "string" | "ptr" | "integer" | "boolean" | "i32" | "any" => Some("i32"),
        "i64" => Some("i64"),
        "number" => Some("f64"),
        other => panic!("Unknown return type in function-registry.toml: '{}'", other),
    }
}

fn zero_const(wasm_type: &str) -> String {
    format!("({}.const 0)", wasm_type)
}

/// Builds a module that imports every entry and exports `probe_<i>` calling
/// import `i` with zero arguments. It also exports `memory`, `malloc` and
/// `__heap_ptr` so string-returning stubs can write their (empty) result the
/// same way they do for compiler-generated modules.
fn generate_probe_module(entries: &[FunctionEntry]) -> String {
    let mut wat = String::from("(module\n");
    for (i, entry) in entries.iter().enumerate() {
        let params: Vec<&str> = entry
            .params
            .iter()
            .flat_map(|t| expand_param_type(t))
            .collect();
        let result = expand_return_type(&entry.returns);
        wat.push_str(&format!(
            "  (import \"{}\" \"{}\" (func $f{}",
            entry.module, entry.name, i
        ));
        if !params.is_empty() {
            wat.push_str(&format!(" (param {})", params.join(" ")));
        }
        if let Some(r) = result {
            wat.push_str(&format!(" (result {})", r));
        }
        wat.push_str("))\n");
    }
    wat.push_str("  (memory (export \"memory\") 1)\n");
    wat.push_str("  (global $heap (export \"__heap_ptr\") (mut i32) (i32.const 1024))\n");
    wat.push_str(
        "  (func (export \"malloc\") (param $n i32) (result i32)\n\
         \x20   (local $p i32)\n\
         \x20   (local.set $p (global.get $heap))\n\
         \x20   (global.set $heap (i32.add (global.get $heap) (local.get $n)))\n\
         \x20   (local.get $p))\n",
    );
    for (i, entry) in entries.iter().enumerate() {
        let args: Vec<String> = entry
            .params
            .iter()
            .flat_map(|t| expand_param_type(t))
            .map(zero_const)
            .collect();
        let call = format!("(call $f{} {})", i, args.join(" "));
        let body = if expand_return_type(&entry.returns).is_some() {
            format!("(drop {})", call)
        } else {
            call
        };
        wat.push_str(&format!("  (func (export \"probe_{}\") {})\n", i, body));
    }
    wat.push(')');
    wat
}

#[test]
fn ui_stubs_cover_browser_side_bridge() {
    let entries = load_browser_ui_entries();
    assert!(
        entries.len() >= 50,
        "Expected at least 50 browser-side UI bridge entries in function-registry.toml \
         (categories {:?}, hosts containing \"browser\") — found {}. \
         If the registry changed, review load_browser_ui_entries().",
        UI_CATEGORIES,
        entries.len()
    );

    let wat = generate_probe_module(&entries);
    let engine = Engine::default();
    let module = Module::new(&engine, &wat).unwrap_or_else(|e| {
        panic!(
            "Failed to parse synthetic UI WAT module ({} imports): {}\nWAT:\n{}",
            entries.len(),
            e,
            &wat[..wat.len().min(800)]
        )
    });

    let linker = create_linker(&engine).expect("create_linker should succeed");
    let router = Arc::new(Router::new());
    let state = WasmState::new(router);
    let mut store = Store::new(&engine, state);

    let instance = linker.instantiate(&mut store, &module).unwrap_or_else(|e| {
        panic!(
            "Failed to instantiate UI WAT module with {} imports: {}\n\
             A browser-side `_ui_*` or `_storage_*` host stub is missing or \
             has the wrong signature. Add it to src/bridge_ui_stubs.rs.",
            entries.len(),
            e
        )
    });

    let mut trapped: Vec<String> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let probe = instance
            .get_typed_func::<(), ()>(&mut store, &format!("probe_{}", i))
            .expect("probe export must exist");
        if let Err(e) = probe.call(&mut store, ()) {
            trapped.push(format!("{} — {}", entry.name, e));
        }
    }
    assert!(
        trapped.is_empty(),
        "{} browser-side UI stub(s) trapped when called on the server:\n{}",
        trapped.len(),
        trapped.join("\n")
    );
}
