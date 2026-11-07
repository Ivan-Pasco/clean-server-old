//! WASM Module Loading and Execution
//!
//! Handles loading compiled Clean Language WASM modules and executing route handlers.

use crate::bridge::create_linker;
use crate::error::{RuntimeError, RuntimeResult};
use crate::error_reporting::{self, WasmParseReport};
use crate::permissions::{PermissionGate, parse_permissions};
use crate::router::SharedRouter;
use crate::session::{SessionConfig, SharedSessionStore, create_session_store};
use host_bridge::{DbBridge, WasmMemory, WasmStateCore};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use tokio::sync::RwLock as TokioRwLock;
use tracing::{debug, info, warn};
use wasmtime::{Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder, Trap};

/// Shared database bridge type
pub type SharedDbBridge = Arc<TokioRwLock<DbBridge>>;

/// Roles and permissions store
#[derive(Default)]
pub struct RolesStore {
    /// Map of role name -> list of permissions
    pub roles: HashMap<String, Vec<String>>,
}

impl RolesStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register roles from JSON config
    /// Expects: {"admin": ["*"], "user": ["read", "write"], "guest": ["read"]}
    pub fn register(&mut self, config: &str) -> bool {
        match serde_json::from_str::<HashMap<String, Vec<String>>>(config) {
            Ok(parsed) => {
                self.roles = parsed;
                true
            }
            Err(_) => false,
        }
    }

    /// Check if a role has a specific permission
    pub fn has_permission(&self, role: &str, permission: &str) -> bool {
        self.roles
            .get(role)
            .map(|perms| {
                perms.contains(&"*".to_string()) || perms.contains(&permission.to_string())
            })
            .unwrap_or(false)
    }

    /// Get all permissions for a role
    pub fn get_permissions(&self, role: &str) -> Vec<String> {
        self.roles.get(role).cloned().unwrap_or_default()
    }
}

/// Shared roles store type
pub type SharedRolesStore = Arc<RwLock<RolesStore>>;

/// Shared static file directories: list of (url_prefix, filesystem_dir) pairs
pub type SharedStaticDirs = Arc<RwLock<Vec<(String, String)>>>;

/// Create a new empty shared static dirs list
pub fn create_shared_static_dirs() -> SharedStaticDirs {
    Arc::new(RwLock::new(Vec::new()))
}

/// A single registered island component for client-side hydration
#[derive(Debug, Clone, serde::Serialize)]
pub struct IslandEntry {
    /// The component name used as identifier (e.g. "Counter", "SearchBar")
    pub component: String,
    /// URL path to the compiled WASM module for this island (e.g. "/islands/counter.wasm")
    pub module: String,
    /// Hydration strategy: "on" | "visible" | "idle" | "only"
    pub hydration: String,
}

/// Store of all island components registered during WASM module initialization
#[derive(Default, Clone)]
pub struct IslandsStore {
    pub islands: Vec<IslandEntry>,
}

impl IslandsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Shared islands store type — Arc<RwLock<...>> so it is accessible after initialization
pub type SharedIslandsStore = Arc<RwLock<IslandsStore>>;

/// Map of component tag name → server-side HTML template registered by _ui_register_component_html
pub type ComponentRegistry = std::collections::HashMap<String, String>;
pub type SharedComponentRegistry = Arc<RwLock<ComponentRegistry>>;

pub fn create_shared_component_registry() -> SharedComponentRegistry {
    Arc::new(RwLock::new(ComponentRegistry::new()))
}

/// Create a new empty shared islands store
pub fn create_shared_islands_store() -> SharedIslandsStore {
    Arc::new(RwLock::new(IslandsStore::new()))
}

/// MCP transport mode — toggles when `_mcp_http_serve` is called
#[derive(Debug, Clone, PartialEq, Default)]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

/// A pending HTTP MCP request waiting for a WASM-side response
pub struct McpPendingRequest {
    pub body: String,
    /// Request headers (lower-cased names), readable via `_mcp_http_header`
    pub headers: Vec<(String, String)>,
    /// Sync channel sender used to deliver the WASM response back to the HTTP handler
    pub response_tx: std::sync::mpsc::SyncSender<String>,
}

/// Shared MCP bridge state — one instance per WASM module
///
/// Bridges synchronous WASM bridge calls to the async HTTP+SSE server when in
/// HTTP transport mode. In stdio mode the queue and SSE map are unused.
pub struct McpBridgeState {
    pub transport: Mutex<McpTransport>,
    pub request_queue: (
        Mutex<std::collections::VecDeque<McpPendingRequest>>,
        Condvar,
    ),
    pub current_http_response: Mutex<Option<std::sync::mpsc::SyncSender<String>>>,
    /// Headers of the request returned by the most recent `_mcp_http_accept`
    pub current_http_headers: Mutex<Vec<(String, String)>>,
    /// API key required as `Authorization: Bearer <key>` on every HTTP request
    /// (frame.mcp `apiKey:`), set via `_mcp_http_require_key`
    pub api_key: Mutex<Option<String>>,
    pub sse_clients:
        Mutex<std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>>,
}

impl McpBridgeState {
    pub fn new() -> Self {
        Self {
            transport: Mutex::new(McpTransport::Stdio),
            request_queue: (
                Mutex::new(std::collections::VecDeque::new()),
                Condvar::new(),
            ),
            current_http_response: Mutex::new(None),
            current_http_headers: Mutex::new(Vec::new()),
            api_key: Mutex::new(None),
            sse_clients: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for McpBridgeState {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedMcpBridgeState = Arc<McpBridgeState>;

pub fn create_shared_mcp_bridge_state() -> SharedMcpBridgeState {
    Arc::new(McpBridgeState::new())
}

/// SMTP configuration stored by _email_configure during startup
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub secure: bool,
    pub username: String,
    pub password: String,
    pub from_address: String,
}

/// Per-module email state: current config and last send error
pub struct SmtpState {
    pub config: Option<SmtpConfig>,
    pub last_error: String,
}

impl SmtpState {
    pub fn new() -> Self {
        Self {
            config: None,
            last_error: String::new(),
        }
    }
}

impl Default for SmtpState {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared SMTP state — shared across all request handlers for this module
pub type SharedSmtpState = Arc<parking_lot::Mutex<SmtpState>>;

pub fn create_shared_smtp_state() -> SharedSmtpState {
    Arc::new(parking_lot::Mutex::new(SmtpState::new()))
}

/// State held by each WASM store instance
pub struct WasmState {
    /// Memory allocator
    pub memory: WasmMemory,
    /// Router for registering routes
    pub router: SharedRouter,
    /// Server port (for _http_listen)
    pub port: u16,
    /// Current request context (if handling a request)
    pub request_context: Option<RequestContext>,
    /// Auth context (if authenticated)
    pub auth_context: Option<AuthContext>,
    /// Session stored during this request (`_session_store` blob). It is the
    /// caller's session for the rest of the request, before the browser has
    /// echoed its cookie back.
    pub request_session_id: Option<String>,
    /// Locale selected with `_i18n_set_locale` during this request. `None`
    /// means the module's default locale (`LocaleState::default_locale`).
    pub active_locale: Option<String>,
    /// `true` while the module's entry point runs at startup. Bridges that
    /// configure module-wide defaults (e.g. `_i18n_set_locale`) use it.
    pub initializing: bool,
    /// Last error (for error reporting)
    pub last_error: Option<String>,
    /// Database bridge for database operations
    pub db_bridge: SharedDbBridge,
    /// Session store for session-based auth
    pub session_store: SharedSessionStore,
    /// Pending Set-Cookie header (for session creation/destruction)
    pub pending_set_cookie: Option<String>,
    /// Pending custom response headers
    pub pending_headers: Vec<(String, String)>,
    /// Pending redirect (status_code, url)
    pub pending_redirect: Option<(u16, String)>,
    /// Pending response status (for _http_respond)
    pub pending_status: Option<u16>,
    /// Pending response body (for _http_respond)
    pub pending_body: Option<String>,
    /// Transaction opened by `_db_begin` (see `WasmStateCore::transaction_slot`)
    pub open_transaction: Option<host_bridge::OpenTransaction>,
    /// Cached last_insert_id from the most recent INSERT in this request.
    /// The DB driver acquires a fresh pooled connection per query, so
    /// MySQL's `LAST_INSERT_ID()` and SQLite's `LAST_INSERT_ROWID()` are
    /// invisible to a follow-up call. The bridge caches the value here
    /// and serves it back when the caller asks for it.
    pub last_insert_id: Option<i64>,
    /// Roles and permissions store
    pub roles_store: SharedRolesStore,
    /// Static file directories registered via _http_serve_static
    pub static_dirs: SharedStaticDirs,
    /// Island components registered for client-side hydration
    pub islands_store: SharedIslandsStore,
    /// Component HTML templates registered via _ui_register_component_html for SSR expansion
    pub component_registry: SharedComponentRegistry,
    /// Resolved `[bridge.functions.callback]` contracts from the build manifest.
    /// Populated by `WasmInstance::set_callbacks` at startup and copied into
    /// each fresh state in `create_instance`. Empty when the manifest didn't
    /// declare any (e.g. older compiler / no v2 plugins loaded). See
    /// `foundation/spec/plugins/contracts/bridge-host-classes.md` §4.
    pub callbacks: Arc<Vec<crate::build_manifest::CallbackContract>>,
    /// Bridge function permission gate derived from the `clean:permissions`
    /// custom section of the loaded WASM module.
    pub permission_gate: PermissionGate,
    /// Resource limits for this Store (memory, tables, instances)
    pub limits: StoreLimits,
    /// Accumulated stylesheet hrefs for injection into the response <head> (deduplicated)
    pub pending_head_links: Vec<String>,
    /// MCP bridge state (transport mode, request queue, SSE clients)
    pub mcp: SharedMcpBridgeState,
    /// Responses from in-process test requests, keyed by handle
    pub test_responses: std::collections::HashMap<i32, TestResponse>,
    /// Counter for the next test response handle
    pub next_test_handle: i32,
    /// SSE sender for STREAM route handlers — set by the server before calling the handler
    pub sse_sender: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// SMTP configuration and last-error state for email bridge functions
    pub smtp_state: SharedSmtpState,
    /// Shared WebSocket server state (connections, rooms, route registry).
    /// Populated once at server startup and shared across all WASM instances.
    pub ws_state: crate::websocket::SharedWsState,
    /// Shared background job queue state (configs, records, cron schedules).
    /// Populated once at server startup and shared across all WASM instances.
    pub jobs_state: crate::jobs::SharedJobsState,
    /// Shared locale/i18n state (translation maps, default locale).
    /// Populated once at server startup and shared across all WASM instances.
    pub locale_state: crate::locale::SharedLocaleState,
    /// Shared runtime configuration populated by `server:` block bridges
    /// (`_http_listen_on`, `_cors_configure`, `_rate_limit_configure`,
    /// `_http_set_global_error_handler`) during WASM init. Read by the server
    /// when building the axum router so WASM-declared host/port/CORS/rate-limit
    /// values are honored.
    pub runtime_config: crate::runtime_config::SharedRuntimeConfig,
}

/// Request context passed to handlers
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Raw request body bytes, preserved byte-for-byte from the wire.
    ///
    /// Populated by the HTTP entry point (see `handle_request` in `server.rs`)
    /// and by any test/job dispatcher that ingests a request body.
    /// `_req_body_bytes` returns this verbatim when set; when `None`, it falls
    /// back to the UTF-8 bytes of `body` so text-only handlers still work.
    ///
    /// Additive to `body` (the UTF-8 string surface, consumed by `_req_body`).
    /// Both fields describe the same request payload — `body` is the lossy
    /// UTF-8 view, `body_bytes` is the lossless view.
    pub body_bytes: Option<Vec<u8>>,
    pub params: std::collections::HashMap<String, String>,
    pub query: std::collections::HashMap<String, String>,
}

/// Authentication context
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: i32,
    pub role: String,
    pub session_id: Option<String>,
}

/// Response captured from a `_test_http_request` in-process dispatch
#[derive(Debug, Clone)]
pub struct TestResponse {
    pub status: i32,
    pub body: String,
}

/// Response data from a handler call
#[derive(Debug, Clone)]
pub struct HandlerResponse {
    /// Response body
    pub body: String,
    /// Pending Set-Cookie header
    pub set_cookie: Option<String>,
    /// Pending custom headers
    pub headers: Vec<(String, String)>,
    /// Pending redirect (status_code, url)
    pub redirect: Option<(u16, String)>,
    /// Response status code (from _http_respond)
    pub status: Option<u16>,
    /// Stylesheet hrefs to inject into the response <head> as <link> tags
    pub head_links: Vec<String>,
}

/// Build StoreLimits from a memory limit in bytes
fn build_store_limits(memory_limit: usize) -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(memory_limit)
        .trap_on_grow_failure(true)
        .instances(1)
        .tables(10)
        .memories(1)
        .build()
}

/// Format a wasmtime error including its underlying `Trap` kind.
///
/// `wasmtime::Error` is an `anyhow::Error`. Its `Display` impl prints only the
/// top-level message ("error while executing at wasm backtrace: …") and the
/// frame table — the actual trap code (`memory out of bounds`, `wasm stack
/// overflow`, `unreachable`, `integer overflow`, …) lives on the `Trap`
/// downcast and gets stripped by `format!("{}", err)`. Callers that log or
/// return the error to a client MUST use this helper so operators can tell
/// which trap fired without re-running under a debugger.
fn format_wasm_error(err: &wasmtime::Error) -> String {
    match err.downcast_ref::<Trap>() {
        Some(trap) => format!("wasm trap: {} — {}", trap, err),
        None => format!("{}", err),
    }
}

/// Detect whether a wasmtime trap is the memory-size limit being hit.
/// We can't pattern-match on a structured error code because wasmtime collapses
/// the OOM into a generic trap; the trap message is the only signal.
fn is_memory_limit_trap(err: &wasmtime::Error) -> bool {
    if let Some(trap) = err.downcast_ref::<Trap>()
        && matches!(
            trap,
            Trap::MemoryOutOfBounds | Trap::HeapMisaligned | Trap::AllocationTooLarge
        )
    {
        return true;
    }
    let msg = format_wasm_error(err).to_lowercase();
    msg.contains("memory")
        && (msg.contains("grow") || msg.contains("out of bounds") || msg.contains("limit"))
}

/// Wrap a wasmtime handler error with friendlier context when the trap is
/// caused by the per-instance memory limit. Lets operators see "raise
/// CLEAN_SERVER_MEMORY_LIMIT_MB" instead of the raw wasmtime backtrace.
fn classify_handler_error(handler_name: &str, err: wasmtime::Error) -> RuntimeError {
    let formatted = format_wasm_error(&err);
    if is_memory_limit_trap(&err) {
        let limit_mb = std::env::var("CLEAN_SERVER_MEMORY_LIMIT_MB")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_MEMORY_LIMIT / (1024 * 1024));
        RuntimeError::wasm(format!(
            "Handler {} hit the {} MB WASM memory limit. \
             Raise it with CLEAN_SERVER_MEMORY_LIMIT_MB=<MB> if the request is \
             genuinely allocation-heavy, or investigate the handler for an \
             iterate/string accumulation loop that doesn't release intermediate \
             buffers (see compiler bug SERVER-NO-WASM-HEAP-RESET-PER-REQUEST). \
             Original error: {}",
            handler_name, limit_mb, formatted
        ))
    } else {
        RuntimeError::wasm(format!("Handler {} failed: {}", handler_name, formatted))
    }
}

/// Default memory limit: 128 MB. Raised from 32 MB because realistic
/// SSR handlers (page rendering with iterate + string.split + string.trim
/// chains, design-doc validators, etc.) exhaust 32 MB inside a single
/// request — see SERVER-NO-WASM-HEAP-RESET-PER-REQUEST. The intra-request
/// allocation chain has no per-iteration scope_pop, so peak heap usage in
/// one handler invocation can be several MB even for moderately sized
/// inputs. 128 MB matches what wasmtime defaults to in most embedder
/// projects and leaves room for compiler/framework-level intra-request
/// scope work without re-tuning every deployment.
///
/// Override at runtime with `CLEAN_SERVER_MEMORY_LIMIT_MB`, in megabytes,
/// e.g. `CLEAN_SERVER_MEMORY_LIMIT_MB=512`.
const DEFAULT_MEMORY_LIMIT: usize = 128 * 1024 * 1024;

/// Read `CLEAN_SERVER_MEMORY_LIMIT_MB` from the environment and convert
/// to bytes. Returns `DEFAULT_MEMORY_LIMIT` when the env var is unset or
/// invalid.
pub fn memory_limit_from_env() -> usize {
    match std::env::var("CLEAN_SERVER_MEMORY_LIMIT_MB") {
        Ok(s) => match s.trim().parse::<usize>() {
            Ok(mb) if mb > 0 => mb.saturating_mul(1024 * 1024),
            _ => DEFAULT_MEMORY_LIMIT,
        },
        Err(_) => DEFAULT_MEMORY_LIMIT,
    }
}

impl WasmState {
    pub fn new(router: SharedRouter) -> Self {
        Self {
            memory: WasmMemory::new(),
            router,
            port: 3000,
            request_context: None,
            auth_context: None,
            request_session_id: None,
            active_locale: None,
            initializing: false,
            last_error: None,
            db_bridge: Arc::new(TokioRwLock::new(DbBridge::new())),
            session_store: create_session_store(SessionConfig::default()),
            pending_set_cookie: None,
            pending_headers: Vec::new(),
            pending_redirect: None,
            pending_status: None,
            pending_body: None,
            open_transaction: None,
            last_insert_id: None,
            roles_store: Arc::new(RwLock::new(RolesStore::new())),
            static_dirs: create_shared_static_dirs(),
            islands_store: create_shared_islands_store(),
            component_registry: create_shared_component_registry(),
            callbacks: Arc::new(Vec::new()),
            permission_gate: PermissionGate::allow_all(),
            limits: build_store_limits(DEFAULT_MEMORY_LIMIT),
            pending_head_links: Vec::new(),
            mcp: create_shared_mcp_bridge_state(),
            test_responses: std::collections::HashMap::new(),
            next_test_handle: 0,
            sse_sender: None,
            smtp_state: create_shared_smtp_state(),
            ws_state: crate::websocket::create_shared_ws_state(),
            jobs_state: crate::jobs::create_shared_jobs_state(),
            locale_state: crate::locale::create_shared_locale_state(),
            runtime_config: crate::runtime_config::create_shared_runtime_config(),
        }
    }

    pub fn with_db_bridge(router: SharedRouter, db_bridge: SharedDbBridge) -> Self {
        Self {
            memory: WasmMemory::new(),
            router,
            port: 3000,
            request_context: None,
            auth_context: None,
            request_session_id: None,
            active_locale: None,
            initializing: false,
            last_error: None,
            db_bridge,
            session_store: create_session_store(SessionConfig::default()),
            pending_set_cookie: None,
            pending_headers: Vec::new(),
            pending_redirect: None,
            pending_status: None,
            pending_body: None,
            open_transaction: None,
            last_insert_id: None,
            roles_store: Arc::new(RwLock::new(RolesStore::new())),
            static_dirs: create_shared_static_dirs(),
            islands_store: create_shared_islands_store(),
            component_registry: create_shared_component_registry(),
            callbacks: Arc::new(Vec::new()),
            permission_gate: PermissionGate::allow_all(),
            limits: build_store_limits(DEFAULT_MEMORY_LIMIT),
            pending_head_links: Vec::new(),
            mcp: create_shared_mcp_bridge_state(),
            test_responses: std::collections::HashMap::new(),
            next_test_handle: 0,
            sse_sender: None,
            smtp_state: create_shared_smtp_state(),
            ws_state: crate::websocket::create_shared_ws_state(),
            jobs_state: crate::jobs::create_shared_jobs_state(),
            locale_state: crate::locale::create_shared_locale_state(),
            runtime_config: crate::runtime_config::create_shared_runtime_config(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_session_store(
        router: SharedRouter,
        db_bridge: SharedDbBridge,
        session_store: SharedSessionStore,
        static_dirs: SharedStaticDirs,
        islands_store: SharedIslandsStore,
        component_registry: SharedComponentRegistry,
        permission_gate: PermissionGate,
        memory_limit: usize,
    ) -> Self {
        Self {
            memory: WasmMemory::new(),
            router,
            port: 3000,
            request_context: None,
            auth_context: None,
            request_session_id: None,
            active_locale: None,
            initializing: false,
            last_error: None,
            db_bridge,
            session_store,
            pending_set_cookie: None,
            pending_headers: Vec::new(),
            pending_redirect: None,
            pending_status: None,
            pending_body: None,
            open_transaction: None,
            last_insert_id: None,
            roles_store: Arc::new(RwLock::new(RolesStore::new())),
            static_dirs,
            islands_store,
            component_registry,
            callbacks: Arc::new(Vec::new()),
            permission_gate,
            limits: build_store_limits(memory_limit),
            pending_head_links: Vec::new(),
            mcp: create_shared_mcp_bridge_state(),
            test_responses: std::collections::HashMap::new(),
            next_test_handle: 0,
            sse_sender: None,
            smtp_state: create_shared_smtp_state(),
            ws_state: crate::websocket::create_shared_ws_state(),
            jobs_state: crate::jobs::create_shared_jobs_state(),
            locale_state: crate::locale::create_shared_locale_state(),
            runtime_config: crate::runtime_config::create_shared_runtime_config(),
        }
    }

    /// Set request context for the current request
    pub fn set_request(&mut self, ctx: RequestContext) {
        tracing::debug!(
            "WasmState::set_request: Setting context with params: {:?}",
            ctx.params
        );
        tracing::debug!("WasmState::set_request: Path: {}", ctx.path);
        self.request_context = Some(ctx);
        // Reset memory allocator for new request
        self.memory.reset();
    }

    /// Clear request context
    pub fn clear_request(&mut self) {
        self.request_context = None;
        self.auth_context = None;
        self.request_session_id = None;
        self.active_locale = None;
        self.pending_set_cookie = None;
        self.pending_headers.clear();
        self.pending_redirect = None;
        self.pending_head_links.clear();
    }

    /// Set auth context from session
    pub fn set_auth_from_session(&mut self, user_id: i32, role: String, session_id: String) {
        self.auth_context = Some(AuthContext {
            user_id,
            role,
            session_id: Some(session_id),
        });
    }

    /// Take pending Set-Cookie header (consumes it)
    pub fn take_pending_cookie(&mut self) -> Option<String> {
        self.pending_set_cookie.take()
    }

    /// Take pending custom headers (consumes them)
    pub fn take_pending_headers(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.pending_headers)
    }

    /// Take pending redirect (consumes it)
    pub fn take_pending_redirect(&mut self) -> Option<(u16, String)> {
        self.pending_redirect.take()
    }

    /// Take pending status (consumes it)
    pub fn take_pending_status(&mut self) -> Option<u16> {
        self.pending_status.take()
    }

    /// Set response status
    pub fn set_status(&mut self, status: u16) {
        self.pending_status = Some(status);
    }

    /// Set response body
    pub fn set_body(&mut self, body: String) {
        self.pending_body = Some(body);
    }

    /// Add a custom response header
    pub fn add_header(&mut self, name: String, value: String) {
        self.pending_headers.push((name, value));
    }

    /// Set a redirect response
    pub fn set_redirect(&mut self, status_code: u16, url: String) {
        self.pending_redirect = Some((status_code, url));
    }

    /// Take accumulated head link hrefs (consumes them)
    pub fn take_pending_head_links(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_head_links)
    }
}

// Implement WasmStateCore trait from host-bridge
// This allows clean-server to use all shared host functions from host-bridge
impl WasmStateCore for WasmState {
    fn memory(&self) -> &WasmMemory {
        &self.memory
    }

    fn memory_mut(&mut self) -> &mut WasmMemory {
        &mut self.memory
    }

    fn db_bridge(&self) -> Option<host_bridge::SharedDbBridge> {
        Some(self.db_bridge.clone())
    }

    fn set_error(&mut self, error: String) {
        self.last_error = Some(error);
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn transaction_slot(&mut self) -> Option<&mut Option<host_bridge::OpenTransaction>> {
        Some(&mut self.open_transaction)
    }

    fn last_insert_id(&self) -> Option<i64> {
        self.last_insert_id
    }

    fn set_last_insert_id(&mut self, id: Option<i64>) {
        self.last_insert_id = id;
    }
}

/// WASM module instance ready for execution
pub struct WasmInstance {
    /// Wasmtime engine
    engine: Engine,
    /// Compiled module (thread-safe, can create instances from this)
    module: Module,
    /// Shared router
    router: SharedRouter,
    /// Linker for creating instances
    linker: wasmtime::Linker<WasmState>,
    /// Database bridge (shared with state for external configuration)
    db_bridge: SharedDbBridge,
    /// Session store (shared across all instances)
    session_store: SharedSessionStore,
    /// Static file directories registered during initialization
    static_dirs: SharedStaticDirs,
    /// Island components registered for client-side hydration
    islands_store: SharedIslandsStore,
    /// Component HTML templates registered via _ui_register_component_html
    component_registry: SharedComponentRegistry,
    /// Resolved callback contracts from `build-manifest.json`. Set once at
    /// startup via `set_callbacks` then copied into every fresh `WasmState`
    /// in `create_instance`. Interior mutability lets the server install
    /// callbacks after the `WasmInstance` is constructed (build manifest is
    /// loaded after the WASM module). See contracts/bridge-host-classes.md §4.
    callbacks: parking_lot::Mutex<Arc<Vec<crate::build_manifest::CallbackContract>>>,
    /// Bridge function permission gate parsed from the loaded WASM binary
    permission_gate: PermissionGate,
    /// Memory limit in bytes for each Store
    memory_limit: usize,
    /// Snapshot exports and data-segment extent of the instrumented module.
    snapshot_layout: crate::startup_snapshot::InstrumentedModule,
    /// Instance state captured after the start function ran in `initialize`.
    /// Restored into every per-request instance so startup state (module
    /// globals, heap data) is visible to handlers. `None` until initialized.
    startup_snapshot: parking_lot::RwLock<Option<Arc<crate::startup_snapshot::StartupSnapshot>>>,
    /// Roles registered via `_roles_register` (shared across all instances so
    /// a `roles:` block applied at startup is visible to every request).
    roles_store: SharedRolesStore,
    /// SMTP configuration set via `_email_configure` (shared across all instances).
    smtp_state: SharedSmtpState,
    /// Shared WebSocket state (connections, rooms, route registry).
    /// A single instance is shared with the server so bridge functions can
    /// find and update live WebSocket connections.
    pub ws_state: crate::websocket::SharedWsState,
    /// Shared background job queue state (configs, records, cron schedules).
    /// Shared with the server so the worker loop and bridge functions see the
    /// same job registry.
    pub jobs_state: crate::jobs::SharedJobsState,
    /// Shared locale/i18n state (translation maps, default locale).
    /// Shared with the server so WASM init (locale: blocks) and request handlers see
    /// the same translation maps.
    pub locale_state: crate::locale::SharedLocaleState,
    /// Shared runtime configuration populated by `server:` block bridges during
    /// WASM init. Read by `start_server` after init to apply WASM-declared
    /// host/port/CORS/rate-limit/global-error-handler values.
    pub runtime_config: crate::runtime_config::SharedRuntimeConfig,
}

impl WasmInstance {
    /// Load a WASM module from a file
    pub fn load(wasm_path: &Path, router: SharedRouter) -> RuntimeResult<Self> {
        info!("Loading WASM module from {:?}", wasm_path);

        // Read WASM bytes
        let wasm_bytes = std::fs::read(wasm_path).map_err(|e| {
            RuntimeError::wasm(format!("Failed to read WASM file {:?}: {}", wasm_path, e))
        })?;

        let db_bridge = Arc::new(TokioRwLock::new(DbBridge::new()));
        let session_store = create_session_store(SessionConfig::default());
        Self::from_bytes_inner(
            &wasm_bytes,
            router,
            db_bridge,
            session_store,
            Some(wasm_path),
            memory_limit_from_env(),
        )
    }

    /// Load a WASM module from a file with a custom database bridge
    pub fn load_with_db(
        wasm_path: &Path,
        router: SharedRouter,
        db_bridge: SharedDbBridge,
    ) -> RuntimeResult<Self> {
        Self::load_with_db_and_limit(wasm_path, router, db_bridge, memory_limit_from_env())
    }

    /// Load a WASM module from a file with a custom database bridge and memory limit
    pub fn load_with_db_and_limit(
        wasm_path: &Path,
        router: SharedRouter,
        db_bridge: SharedDbBridge,
        memory_limit: usize,
    ) -> RuntimeResult<Self> {
        info!("Loading WASM module from {:?}", wasm_path);

        let wasm_bytes = std::fs::read(wasm_path).map_err(|e| {
            RuntimeError::wasm(format!("Failed to read WASM file {:?}: {}", wasm_path, e))
        })?;

        let session_store = create_session_store(SessionConfig::default());
        Self::from_bytes_inner(
            &wasm_bytes,
            router,
            db_bridge,
            session_store,
            Some(wasm_path),
            memory_limit,
        )
    }

    /// Load a WASM module from bytes
    pub fn from_bytes(wasm_bytes: &[u8], router: SharedRouter) -> RuntimeResult<Self> {
        let db_bridge = Arc::new(TokioRwLock::new(DbBridge::new()));
        Self::from_bytes_with_db(wasm_bytes, router, db_bridge)
    }

    /// Load a WASM module from bytes with a custom database bridge
    pub fn from_bytes_with_db(
        wasm_bytes: &[u8],
        router: SharedRouter,
        db_bridge: SharedDbBridge,
    ) -> RuntimeResult<Self> {
        let session_store = create_session_store(SessionConfig::default());
        Self::from_bytes_with_all(wasm_bytes, router, db_bridge, session_store)
    }

    /// Load a WASM module from bytes with all shared resources
    pub fn from_bytes_with_all(
        wasm_bytes: &[u8],
        router: SharedRouter,
        db_bridge: SharedDbBridge,
        session_store: SharedSessionStore,
    ) -> RuntimeResult<Self> {
        Self::from_bytes_inner(
            wasm_bytes,
            router,
            db_bridge,
            session_store,
            None,
            memory_limit_from_env(),
        )
    }

    /// Internal loader that carries the optional source path for error reporting.
    ///
    /// Public `from_bytes_*` entry points have no path to report; the
    /// file-based `load*` entry points pass `Some(path)` so that
    /// `RUNTIME_WASM_PARSE` diagnostics include the originating file.
    fn from_bytes_inner(
        wasm_bytes: &[u8],
        router: SharedRouter,
        db_bridge: SharedDbBridge,
        session_store: SharedSessionStore,
        module_path: Option<&Path>,
        memory_limit: usize,
    ) -> RuntimeResult<Self> {
        // Parse the clean:permissions custom section before compiling so we
        // have the gate available before any bridge function can be called.
        let permission_gate = parse_permissions(wasm_bytes, "main");

        if permission_gate.is_enforcing() {
            info!(
                "Module loaded with permission enforcement: {} allowed bridge functions",
                permission_gate.allowed_count().unwrap_or(0)
            );
        } else {
            debug!("Module loaded without permission enforcement (no clean:permissions section)");
        }

        // Create engine
        let engine = Engine::default();

        // Compile module. When wasmtime rejects the bytes, assemble a
        // structured diagnostic bundle (see `error_reporting`) before
        // surfacing the error so the compiler team can reproduce the
        // bug from the on-disk report.
        // Export every module-level mutable global so the post-startup state can
        // be captured and restored into per-request instances. Malformed bytes
        // fall through unchanged so wasmtime produces the diagnostic below.
        let snapshot_layout = crate::startup_snapshot::instrument_module(wasm_bytes)
            .unwrap_or_else(|e| {
                debug!("Startup snapshot instrumentation skipped: {}", e);
                crate::startup_snapshot::InstrumentedModule {
                    bytes: wasm_bytes.to_vec(),
                    global_exports: Vec::new(),
                    data_end: None,
                }
            });

        let module = Module::new(&engine, &snapshot_layout.bytes).map_err(|e| {
            let report = WasmParseReport::new(wasm_bytes, &e, module_path);
            let diag_root = error_reporting::diag_dir();
            match report.emit(wasm_bytes, &diag_root) {
                Ok(path) => warn!(
                    "Wrote RUNTIME_WASM_PARSE diagnostic to {:?} (sha={})",
                    path,
                    report.short_fingerprint()
                ),
                Err(io_err) => warn!(
                    "Failed to persist RUNTIME_WASM_PARSE diagnostic: {}",
                    io_err
                ),
            }
            RuntimeError::wasm(format!(
                "Failed to compile WASM module [diag: {}]: {}. Run `clean-server errors show {}` for details.",
                report.short_fingerprint(),
                e,
                report.short_fingerprint()
            ))
        })?;

        debug!("WASM module compiled successfully");

        // Stash the raw WASM bytes for `_dev_snapshot()`. This is a one-time
        // copy at load; the ring buffer's read side base64-encodes on demand.
        // No-op path when CLEAN_DEV isn't `1` — the setter always writes, but
        // the snapshot function ignores the value in production mode.
        crate::dev_capture::set_current_wasm(
            wasm_bytes.to_vec(),
            module_path.map(|p| p.to_path_buf()),
        );

        // Create linker with host functions
        let linker = create_linker(&engine)?;

        debug!("WASM linker configured");

        Ok(Self {
            engine,
            module,
            router,
            linker,
            db_bridge,
            session_store,
            static_dirs: create_shared_static_dirs(),
            islands_store: create_shared_islands_store(),
            component_registry: create_shared_component_registry(),
            callbacks: parking_lot::Mutex::new(Arc::new(Vec::new())),
            permission_gate,
            memory_limit,
            snapshot_layout,
            startup_snapshot: parking_lot::RwLock::new(None),
            roles_store: Arc::new(RwLock::new(RolesStore::new())),
            smtp_state: create_shared_smtp_state(),
            ws_state: crate::websocket::create_shared_ws_state(),
            jobs_state: crate::jobs::create_shared_jobs_state(),
            locale_state: crate::locale::create_shared_locale_state(),
            runtime_config: crate::runtime_config::create_shared_runtime_config(),
        })
    }

    /// Install the resolved callback contracts after construction. Called
    /// once at startup by `start_server` once the build manifest is loaded.
    /// Subsequent `create_instance` calls hand the same `Arc` to every
    /// fresh `WasmState`.
    pub fn set_callbacks(&self, callbacks: Arc<Vec<crate::build_manifest::CallbackContract>>) {
        *self.callbacks.lock() = callbacks;
    }

    /// Create a fresh WASM instance for request handling.
    ///
    /// The instance is brand new (no state shared with other requests), then
    /// the post-startup snapshot taken by `initialize` is applied so startup
    /// state is visible to the handler. See `crate::startup_snapshot`.
    fn create_instance(&self) -> RuntimeResult<(Store<WasmState>, Instance)> {
        let (mut store, instance) = self.instantiate_fresh()?;
        let snapshot = self.startup_snapshot.read().clone();
        if let Some(snapshot) = snapshot {
            snapshot.restore(&mut store, &instance).map_err(|e| {
                RuntimeError::wasm(format!("Failed to restore startup state: {}", e))
            })?;
        }
        Ok((store, instance))
    }

    /// Instantiate the module with only its static initial state.
    fn instantiate_fresh(&self) -> RuntimeResult<(Store<WasmState>, Instance)> {
        let mut state = WasmState::with_session_store(
            self.router.clone(),
            self.db_bridge.clone(),
            self.session_store.clone(),
            self.static_dirs.clone(),
            self.islands_store.clone(),
            self.component_registry.clone(),
            self.permission_gate.clone(),
            self.memory_limit,
        );
        state.ws_state = self.ws_state.clone();
        state.jobs_state = self.jobs_state.clone();
        state.locale_state = self.locale_state.clone();
        state.runtime_config = self.runtime_config.clone();
        state.roles_store = self.roles_store.clone();
        state.smtp_state = self.smtp_state.clone();
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        // Copy the resolved callback contracts into the fresh state so bridge
        // functions like `_ui_render_page` can look up their dispatch rules.
        store.data_mut().callbacks = self.callbacks.lock().clone();

        let instance = self
            .linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| RuntimeError::wasm(format!("Failed to instantiate WASM module: {}", e)))?;

        Ok((store, instance))
    }

    /// Get the session store
    pub fn session_store(&self) -> &SharedSessionStore {
        &self.session_store
    }

    /// Get the shared runtime configuration populated by `server:` block bridges
    pub fn runtime_config(&self) -> &crate::runtime_config::SharedRuntimeConfig {
        &self.runtime_config
    }

    /// Get the database bridge for configuration
    pub fn db_bridge(&self) -> &SharedDbBridge {
        &self.db_bridge
    }

    /// Get the registered static file directories
    pub fn static_dirs(&self) -> &SharedStaticDirs {
        &self.static_dirs
    }

    /// Get the registered island components for client-side hydration
    pub fn islands_store(&self) -> &SharedIslandsStore {
        &self.islands_store
    }

    /// Get the component HTML registry (populated by _ui_register_component_html during init)
    pub fn component_registry(&self) -> &SharedComponentRegistry {
        &self.component_registry
    }

    /// Initialize the module (calls main/start function to register routes)
    pub fn initialize(&self) -> RuntimeResult<()> {
        // Create an instance specifically for initialization
        let (mut store, instance) = self.instantiate_fresh()?;

        // Read __heap_ptr from WASM exports and use it as the authoritative heap start.
        // This avoids hardcoding 65536 and respects the compiler's actual data layout.
        let heap_ptr = instance
            .get_global(&mut store, "__heap_ptr")
            .and_then(|g| g.get(&mut store).i32())
            .unwrap_or(65536) as usize; // fallback for old modules

        info!(
            "Initial heap pointer from __heap_ptr: {} (0x{:x})",
            heap_ptr, heap_ptr
        );
        store.data_mut().memory.set_offset(heap_ptr);

        // Try different entry point names
        let entry_names = ["main", "_start", "start", "init"];

        for name in entry_names {
            if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, name) {
                info!("Calling WASM entry point: {}", name);
                store.data_mut().initializing = true;

                func.call(&mut store, ())
                    .map_err(|e| RuntimeError::wasm(format!("Failed to call {}: {}", name, e)))?;

                // Capture the post-startup state for per-request instances.
                let snapshot = crate::startup_snapshot::StartupSnapshot::capture(
                    &mut store,
                    &instance,
                    &self.snapshot_layout,
                );
                if snapshot.is_none() {
                    warn!(
                        "Module exports no memory; startup state cannot be carried into requests"
                    );
                }
                *self.startup_snapshot.write() = snapshot.map(Arc::new);
                store.data_mut().initializing = false;

                self.configure_session_from_module();

                // Run any migrations registered during WASM startup
                let db = self.db_bridge.clone();
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        let bridge = db.read().await;
                        if let Err(e) = bridge.run_pending_migrations().await {
                            tracing::warn!("Migration runner failed: {}", e);
                        }
                    })
                });

                // Check if routes were registered
                let route_count = self.router.len();
                info!("Module initialized with {} routes", route_count);

                return Ok(());
            }
        }

        // No entry point found, but module might still work
        // (routes could be registered via exports or other means)
        warn!("No entry point found in WASM module");
        Ok(())
    }

    /// Call a route handler function
    /// Creates a fresh WASM instance for each request to ensure clean memory state
    pub fn call_handler(
        &self,
        handler_name: &str,
        request: RequestContext,
    ) -> RuntimeResult<String> {
        // Create a fresh instance for this request
        // This ensures each request starts with clean memory (no heap exhaustion)
        let (mut store, instance) = self.create_instance()?;

        // Set request context
        store.data_mut().set_request(request);

        // Get memory
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| RuntimeError::wasm("Module has no memory export"))?;

        debug!("Calling handler: {}", handler_name);

        if let Ok(handler) = instance.get_typed_func::<(), i32>(&mut store, handler_name) {
            let result_ptr = handler
                .call(&mut store, ())
                .map_err(|e| classify_handler_error(handler_name, e))?;

            let result =
                crate::memory::read_string_from_memory(&store, &memory, result_ptr as u32)?;

            return Ok(result);
        }

        Err(RuntimeError::wasm(format!(
            "Could not find or call handler '{}'",
            handler_name
        )))
    }

    /// Call a route handler function with auth context
    /// Returns the full handler response including body, headers, cookies, and redirects
    pub fn call_handler_with_auth(
        &self,
        handler_name: &str,
        request: RequestContext,
        auth_context: Option<AuthContext>,
    ) -> RuntimeResult<HandlerResponse> {
        debug!(
            "call_handler_with_auth: handler={}, path={}, params={:?}",
            handler_name, request.path, request.params
        );

        // Create a fresh instance for this request
        let (mut store, instance) = self.create_instance()?;

        // Set request context
        debug!(
            "call_handler_with_auth: Setting request context with {} params",
            request.params.len()
        );
        store.data_mut().set_request(request);

        // Verify the params were set correctly
        if let Some(ref ctx) = store.data().request_context {
            debug!(
                "call_handler_with_auth: Verified params in store: {:?}",
                ctx.params
            );
        }

        // Set auth context if provided
        if let Some(auth) = auth_context {
            store.data_mut().auth_context = Some(auth);
        }

        // Get memory
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| RuntimeError::wasm("Module has no memory export"))?;

        debug!("Calling handler with auth: {}", handler_name);

        let result =
            if let Ok(handler) = instance.get_typed_func::<(), i32>(&mut store, handler_name) {
                let result_ptr = handler
                    .call(&mut store, ())
                    .map_err(|e| classify_handler_error(handler_name, e))?;

                // When the handler signalled a redirect via `_http_redirect` /
                // `_res_redirect`, its i32 return value is not guaranteed to be a
                // length-prefixed UTF-8 string. The frame.ui page-render export, for
                // instance, forwards the boxed-any value returned from `guard()`
                // unchanged. The redirect path in `handle_request` discards the body
                // anyway, so reading it would only risk a spurious UTF-8 trap that
                // turns the intended 302 into a 500.
                if store.data().pending_redirect.is_some() {
                    String::new()
                } else {
                    crate::memory::read_string_from_memory(&store, &memory, result_ptr as u32)?
                }
            } else {
                return Err(RuntimeError::wasm(format!(
                    "Could not find or call handler '{}'",
                    handler_name
                )));
            };

        // Get any pending response data
        let set_cookie = store.data_mut().take_pending_cookie();
        let headers = store.data_mut().take_pending_headers();
        let redirect = store.data_mut().take_pending_redirect();
        let status = store.data_mut().take_pending_status();
        let head_links = store.data_mut().take_pending_head_links();

        Ok(HandlerResponse {
            body: result,
            set_cookie,
            headers,
            redirect,
            status,
            head_links,
        })
    }

    /// Call a STREAM (SSE) route handler.
    ///
    /// Creates a fresh WASM instance, wires the SSE sender into `WasmState` so
    /// the `_sse_emit*` bridge functions can write to the channel, then runs the
    /// handler synchronously.  The caller is responsible for consuming the
    /// receiver end of the channel and streaming it to the HTTP client.
    ///
    /// The SSE connection is considered closed when this method returns (the
    /// sender is dropped with the store).
    pub fn call_handler_sse(
        &self,
        handler_name: &str,
        request: RequestContext,
        auth_context: Option<AuthContext>,
        sse_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> RuntimeResult<()> {
        let (mut store, instance) = self.create_instance()?;
        store.data_mut().set_request(request);
        if let Some(auth) = auth_context {
            store.data_mut().auth_context = Some(auth);
        }
        store.data_mut().sse_sender = Some(sse_tx);

        if let Ok(handler) = instance.get_typed_func::<(), i32>(&mut store, handler_name) {
            let _ = handler.call(&mut store, ()).map_err(|e| {
                RuntimeError::wasm(format!("SSE handler {} failed: {}", handler_name, e))
            });
        } else {
            return Err(RuntimeError::wasm(format!(
                "Could not find or call SSE handler '{}'",
                handler_name
            )));
        }

        // Dropping the store drops sse_sender, closing the channel and signalling EOF to the stream.
        Ok(())
    }

    /// Call a WebSocket handler (onConnect / onMessage / onClose).
    ///
    /// Creates a fresh WASM instance, sets the request context and auth context,
    /// then calls the named export synchronously.  The WebSocket client ID and
    /// the current message payload are provided via `websocket::WS_CLIENT_ID` and
    /// `websocket::WS_MESSAGE` task-locals that the caller must have set before
    /// invoking this method (see `websocket::call_wasm_ws_handler`).
    ///
    /// Returns `Ok(())` on success.  The handler's return value is ignored
    /// because WebSocket handlers communicate back to clients via bridge functions
    /// (`_ws_send`, `_ws_broadcast`, etc.) rather than returning a response body.
    pub fn call_handler_ws(
        &self,
        handler_name: &str,
        request: RequestContext,
        auth_context: Option<AuthContext>,
        _client_id: i64,
    ) -> RuntimeResult<()> {
        let (mut store, instance) = self.create_instance()?;

        store.data_mut().set_request(request);
        if let Some(auth) = auth_context {
            store.data_mut().auth_context = Some(auth);
        }

        debug!("Calling WebSocket handler: {}", handler_name);

        // WebSocket handlers may export as () -> i32 or () -> ().
        // Try both signatures; the return value is discarded.
        if let Ok(handler) = instance.get_typed_func::<(), i32>(&mut store, handler_name) {
            let _ = handler.call(&mut store, ()).map_err(|e| {
                crate::error::RuntimeError::wasm(format!(
                    "WebSocket handler {} failed: {}",
                    handler_name, e
                ))
            })?;
            return Ok(());
        }

        if let Ok(handler) = instance.get_typed_func::<(), ()>(&mut store, handler_name) {
            handler.call(&mut store, ()).map_err(|e| {
                crate::error::RuntimeError::wasm(format!(
                    "WebSocket handler {} failed: {}",
                    handler_name, e
                ))
            })?;
            return Ok(());
        }

        Err(crate::error::RuntimeError::wasm(format!(
            "Could not find or call WebSocket handler '{}'",
            handler_name
        )))
    }

    /// Get the shared router
    pub fn router(&self) -> &SharedRouter {
        &self.router
    }

    /// Call a job handler function (invoked by the background worker loop).
    ///
    /// Creates a fresh WASM instance, sets the request context to a synthetic
    /// JOB request, and calls the named export.  The handler return value is
    /// discarded because job handlers communicate outcomes through bridge
    /// functions (job_succeed, job_fail, job_retry_after) rather than return values.
    ///
    /// Returns Ok(()) on normal completion or Err(RuntimeError) on WASM trap
    /// or missing export.
    pub fn call_handler_job(
        &self,
        handler_name: &str,
        request: RequestContext,
        auth_context: Option<AuthContext>,
    ) -> crate::error::RuntimeResult<()> {
        let (mut store, instance) = self.create_instance()?;

        store.data_mut().set_request(request);
        if let Some(auth) = auth_context {
            store.data_mut().auth_context = Some(auth);
        }

        tracing::debug!("Calling job handler: {}", handler_name);

        // Job handlers may export as () -> i32 or () -> ().
        // Try both signatures; the return value is discarded.
        if let Ok(handler) = instance.get_typed_func::<(), i32>(&mut store, handler_name) {
            let _ = handler.call(&mut store, ()).map_err(|e| {
                crate::error::RuntimeError::wasm(format!(
                    "Job handler {} failed: {}",
                    handler_name, e
                ))
            })?;
            return Ok(());
        }

        if let Ok(handler) = instance.get_typed_func::<(), ()>(&mut store, handler_name) {
            handler.call(&mut store, ()).map_err(|e| {
                crate::error::RuntimeError::wasm(format!(
                    "Job handler {} failed: {}",
                    handler_name, e
                ))
            })?;
            return Ok(());
        }

        Err(crate::error::RuntimeError::wasm(format!(
            "Could not find or call job handler '{}'",
            handler_name
        )))
    }

    /// Get export names (for debugging)
    pub fn export_names(&self) -> Vec<String> {
        self.module
            .exports()
            .map(|e| e.name())
            .filter(|name| !name.starts_with(crate::startup_snapshot::SNAPSHOT_GLOBAL_PREFIX))
            .map(str::to_string)
            .collect()
    }

    /// Check if an export exists
    pub fn has_export(&self, name: &str) -> bool {
        self.module.exports().any(|e| e.name() == name)
    }

    /// Call a module export of shape `(string) -> string` (or `() -> string`
    /// when `arg` is `None`) on a fresh request-like instance carrying the
    /// startup state. Strings use the Clean ABI: a pointer to a
    /// length-prefixed UTF-8 buffer, allocated with the module's `malloc`.
    ///
    /// Returns `Ok(None)` when the export is absent or has another signature.
    fn call_string_export(&self, name: &str, arg: Option<&str>) -> RuntimeResult<Option<String>> {
        if !self.has_export(name) {
            return Ok(None);
        }
        let (mut store, instance) = self.create_instance()?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| RuntimeError::wasm("Module has no memory export"))?;
        let result_ptr = match arg {
            Some(arg) => {
                let func = match instance.get_typed_func::<i32, i32>(&mut store, name) {
                    Ok(f) => f,
                    Err(_) => return Ok(None),
                };
                let malloc = instance
                    .get_typed_func::<i32, i32>(&mut store, "malloc")
                    .map_err(|e| RuntimeError::wasm(format!("Module has no malloc: {}", e)))?;
                let bytes = arg.as_bytes();
                let ptr = malloc
                    .call(&mut store, (4 + bytes.len()) as i32)
                    .map_err(|e| RuntimeError::wasm(format!("malloc failed: {}", e)))?;
                if ptr <= 0 {
                    return Err(RuntimeError::wasm("malloc returned a null pointer"));
                }
                let mut buf = Vec::with_capacity(4 + bytes.len());
                buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(bytes);
                memory.write(&mut store, ptr as usize, &buf).map_err(|e| {
                    RuntimeError::memory(format!("Failed to write argument: {}", e))
                })?;
                func.call(&mut store, ptr)
                    .map_err(|e| RuntimeError::wasm(format!("{} trapped: {}", name, e)))?
            }
            None => {
                let func = match instance.get_typed_func::<(), i32>(&mut store, name) {
                    Ok(f) => f,
                    Err(_) => return Ok(None),
                };
                func.call(&mut store, ())
                    .map_err(|e| RuntimeError::wasm(format!("{} trapped: {}", name, e)))?
            }
        };
        crate::memory::read_string_from_memory(&store, &memory, result_ptr as u32).map(Some)
    }

    /// Adopt the application's `auth: session:` settings (cookie name,
    /// path, SameSite, Secure, HttpOnly, timeout) for the host session store.
    ///
    /// frame.auth bakes these settings into its generated
    /// `__auth_build_cookie_header(sessionId)` helper, so the host calls it
    /// once after startup and parses the `Set-Cookie` value it returns.
    /// Modules without frame.auth keep `SessionConfig::default()`.
    pub fn configure_session_from_module(&self) {
        const HELPER: &str = "__auth_build_cookie_header";
        match self.call_string_export(HELPER, Some("probe")) {
            Ok(Some(header)) => match SessionConfig::from_set_cookie(&header) {
                Some(config) => self
                    .session_store
                    .write()
                    .expect("session store lock poisoned")
                    .set_config(config),
                None => warn!("{} returned an unusable cookie: {:?}", HELPER, header),
            },
            Ok(None) => {}
            Err(e) => warn!("Could not read the session cookie settings: {}", e),
        }
    }

    /// Verify a JWT with the application's configured secret and algorithm
    /// by calling frame.auth's generated `jwt_verify(token)` helper. Returns
    /// the verified claims JSON, or `None` when the token is invalid, expired,
    /// or the module has no JWT configuration.
    pub fn verify_bearer_token(&self, token: &str) -> Option<String> {
        match self.call_string_export("jwt_verify", Some(token)) {
            Ok(Some(claims)) if !claims.trim().is_empty() => Some(claims),
            Ok(_) => None,
            Err(e) => {
                warn!("Bearer token verification failed: {}", e);
                None
            }
        }
    }
}

/// Shared WASM instance wrapped in Arc
pub type SharedWasmInstance = Arc<WasmInstance>;

/// Create a shared WASM instance
pub fn create_shared_instance(
    wasm_path: &Path,
    router: SharedRouter,
) -> RuntimeResult<SharedWasmInstance> {
    let instance = WasmInstance::load(wasm_path, router)?;
    Ok(Arc::new(instance))
}

/// Create a shared WASM instance with a database bridge
pub fn create_shared_instance_with_db(
    wasm_path: &Path,
    router: SharedRouter,
    db_bridge: SharedDbBridge,
) -> RuntimeResult<SharedWasmInstance> {
    let instance = WasmInstance::load_with_db(wasm_path, router, db_bridge)?;
    Ok(Arc::new(instance))
}

/// Create a shared WASM instance with a database bridge and memory limit
pub fn create_shared_instance_with_config(
    wasm_path: &Path,
    router: SharedRouter,
    db_bridge: SharedDbBridge,
    memory_limit: usize,
) -> RuntimeResult<SharedWasmInstance> {
    let instance =
        WasmInstance::load_with_db_and_limit(wasm_path, router, db_bridge, memory_limit)?;
    Ok(Arc::new(instance))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::create_shared_router;

    #[test]
    fn test_wasm_state() {
        let router = create_shared_router();
        let mut state = WasmState::new(router);

        assert!(state.request_context.is_none());

        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: vec![],
            body: String::new(),
            body_bytes: None,
            params: std::collections::HashMap::new(),
            query: std::collections::HashMap::new(),
        };

        state.set_request(request);
        assert!(state.request_context.is_some());

        state.clear_request();
        assert!(state.request_context.is_none());
    }

    #[test]
    fn test_request_context() {
        let mut params = std::collections::HashMap::new();
        params.insert("id".to_string(), "123".to_string());

        let mut query = std::collections::HashMap::new();
        query.insert("page".to_string(), "1".to_string());

        let request = RequestContext {
            method: "GET".to_string(),
            path: "/users/123".to_string(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: String::new(),
            body_bytes: None,
            params,
            query,
        };

        assert_eq!(request.method, "GET");
        assert_eq!(request.params.get("id"), Some(&"123".to_string()));
        assert_eq!(request.query.get("page"), Some(&"1".to_string()));
    }

    // Regression: SERVER-TRAP-KIND-STRIPPED. wasmtime::Error's Display strips
    // the Trap kind; format_wasm_error must surface it so operators can tell
    // an OOB apart from a stack overflow without a debugger.
    #[test]
    fn format_wasm_error_surfaces_trap_kind() {
        let err: wasmtime::Error = Trap::MemoryOutOfBounds.into();
        let formatted = format_wasm_error(&err);
        assert!(
            formatted.contains("wasm trap:") && formatted.to_lowercase().contains("memory"),
            "expected formatted error to name the trap kind, got: {formatted}"
        );
    }

    #[test]
    fn is_memory_limit_trap_detects_memory_out_of_bounds() {
        let err: wasmtime::Error = Trap::MemoryOutOfBounds.into();
        assert!(
            is_memory_limit_trap(&err),
            "MemoryOutOfBounds must be classified as a memory-limit trap"
        );
    }

    #[test]
    fn is_memory_limit_trap_ignores_stack_overflow() {
        let err: wasmtime::Error = Trap::StackOverflow.into();
        assert!(
            !is_memory_limit_trap(&err),
            "StackOverflow must not be classified as a memory-limit trap"
        );
    }

    #[test]
    fn classify_handler_error_includes_trap_kind_for_generic_traps() {
        let err: wasmtime::Error = Trap::UnreachableCodeReached.into();
        let runtime = classify_handler_error("my_handler", err);
        let msg = format!("{runtime}");
        assert!(
            msg.contains("my_handler") && msg.to_lowercase().contains("unreachable"),
            "expected handler error to name both the handler and the trap kind, got: {msg}"
        );
    }
}
