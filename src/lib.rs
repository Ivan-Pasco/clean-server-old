//! Clean Server
//!
//! HTTP Server Runtime for executing compiled Clean Language WASM applications.
//!
//! # Overview
//!
//! Clean Server is the execution environment for Clean Language applications.
//! It provides:
//!
//! - **HTTP Server**: Axum-based server that handles incoming requests
//! - **WASM Execution**: Wasmtime-powered WASM module loading and execution
//! - **Route Registration**: Dynamic route registration from WASM modules
//! - **Host Bridge**: System capabilities exposed to WASM (I/O, HTTP, Database, Auth)
//!
//! # Architecture
//!
//! ```text
//! HTTP Request
//!      │
//!      ▼
//! ┌─────────────┐
//! │ Axum Server │
//! └──────┬──────┘
//!        │
//!        ▼
//! ┌─────────────┐
//! │   Router    │──► Match route by method + path
//! └──────┬──────┘
//!        │
//!        ▼
//! ┌─────────────┐
//! │WASM Instance│──► Call handler function
//! └──────┬──────┘
//!        │
//!        ▼
//! ┌─────────────┐
//! │ Host Bridge │──► System I/O, HTTP, DB, Auth
//! └─────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,no_run
//! use clean_server::{start_server, ServerConfig};
//! use std::path::PathBuf;
//!
//! #[tokio::main]
//! async fn main() {
//!     let wasm_path = PathBuf::from("app.wasm");
//!     let config = ServerConfig::default().with_port(3000);
//!
//!     start_server(wasm_path, config).await.unwrap();
//! }
//! ```
//!
//! # WASM Module Requirements
//!
//! WASM modules must:
//!
//! 1. Export a `main`, `_start`, `start`, or `init` function for initialization
//! 2. Call `_http_route(method, path, handler_index)` to register routes
//! 3. Export handler functions that return string pointers
//! 4. Use the standard Clean Language memory format (length-prefixed strings)
//!
//! # Host Functions
//!
//! The runtime provides these host function namespaces:
//!
//! - **env**: Core I/O (print, input), type conversions
//! - **memory_runtime**: Memory allocation (mem_alloc, mem_retain, mem_release)
//! - **http**: HTTP client functions (http_get, http_post, etc.)
//! - **file**: File I/O (file_read, file_write, etc.)
//! - **db**: Database operations (_db_query, _db_execute)
//! - **auth**: Authentication (_auth_verify, _auth_create_session)

pub mod bridge;
pub mod bridge_browser_stubs;
pub mod bridge_canvas_stubs;
pub mod bridge_ui_stubs;
pub mod build_manifest;
pub mod dev_capture;
pub mod error;
pub mod error_reporting;
pub mod jobs;
pub mod locale;
pub mod memory;
pub mod permissions;
pub mod rate_limit;
pub mod router;
pub mod runtime_config;
pub mod server;
pub mod session;
pub mod startup_snapshot;
pub mod wasm;
pub mod websocket;

// Re-exports for convenience
pub use error::{HttpError, RuntimeError, RuntimeResult};
pub use jobs::{SharedJobsState, create_shared_jobs_state};
pub use router::{HttpMethod, RouteHandler, Router, SharedRouter};
pub use server::{MemoryTier, ServerConfig, start_server};
pub use session::{
    SessionConfig, SessionData, SessionStore, SharedSessionStore, create_session_store,
    parse_cookies,
};
pub use wasm::{
    AuthContext, RequestContext, SharedDbBridge, SharedWasmInstance, WasmInstance, WasmState,
    create_shared_instance_with_db,
};

// Re-export host-bridge types for database configuration
pub use host_bridge::{DbBridge, DbConfig};

/// Runtime version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runtime name
pub const NAME: &str = "Clean Server";

/// Print version information
pub fn print_version() {
    println!("{} v{}", NAME, VERSION);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn test_name() {
        assert_eq!(NAME, "Clean Server");
    }
}
