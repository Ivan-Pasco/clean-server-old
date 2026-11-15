//! HTTP Route Registry
//!
//! Manages route registration from WASM modules and matches incoming requests.

use crate::error::{RuntimeError, RuntimeResult};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Convert Express-style route parameters to matchit syntax
/// Express uses `:id`, matchit 0.8+ uses `{id}`
/// Example: "/users/:id/posts/:post_id" -> "/users/{id}/posts/{post_id}"
fn convert_express_to_matchit(path: &str) -> String {
    let mut result = String::with_capacity(path.len() + 8);
    let mut chars = path.chars().peekable();

    while let Some(c) = chars.next() {
        if c == ':' {
            // Start of a parameter - collect the parameter name
            result.push('{');
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '_' {
                    result.push(chars.next().unwrap());
                } else {
                    break;
                }
            }
            result.push('}');
        } else {
            result.push(c);
        }
    }

    result
}

/// HTTP methods supported by the router
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    GET,
    POST,
    PUT,
    PATCH,
    DELETE,
    HEAD,
    OPTIONS,
}

impl HttpMethod {
    /// Parse HTTP method from string
    pub fn parse(s: &str) -> RuntimeResult<Self> {
        match s.to_uppercase().as_str() {
            "GET" => Ok(HttpMethod::GET),
            "POST" => Ok(HttpMethod::POST),
            "PUT" => Ok(HttpMethod::PUT),
            "PATCH" => Ok(HttpMethod::PATCH),
            "DELETE" => Ok(HttpMethod::DELETE),
            "HEAD" => Ok(HttpMethod::HEAD),
            "OPTIONS" => Ok(HttpMethod::OPTIONS),
            other => Err(RuntimeError::route(format!(
                "Unknown HTTP method: {}",
                other
            ))),
        }
    }

    /// Convert to string representation
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::GET => "GET",
            HttpMethod::POST => "POST",
            HttpMethod::PUT => "PUT",
            HttpMethod::PATCH => "PATCH",
            HttpMethod::DELETE => "DELETE",
            HttpMethod::HEAD => "HEAD",
            HttpMethod::OPTIONS => "OPTIONS",
        }
    }
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Route handler information
#[derive(Debug, Clone)]
pub struct RouteHandler {
    /// HTTP method
    pub method: HttpMethod,
    /// URL path pattern
    pub path: String,
    /// WASM export name to call (e.g. "__route_handler_0")
    pub handler_name: String,
    /// Whether this route requires authentication
    pub protected: bool,
    /// Required role (if any)
    pub required_role: Option<String>,
    /// Whether this is a Server-Sent Events (STREAM) route
    pub is_sse: bool,
    /// Whether this is a WebSocket (LIVE) route
    pub is_ws: bool,
    /// If set, this route is a static redirect: (destination, status_code).
    /// The server returns the redirect immediately without invoking any WASM handler.
    pub redirect_destination: Option<(String, u16)>,
}

/// Key for route lookup
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    method: HttpMethod,
    path: String,
}

/// Thread-safe route registry
pub struct Router {
    /// Routes indexed by method and path
    routes: RwLock<HashMap<RouteKey, RouteHandler>>,
    /// Static path matcher for fast lookups
    path_matcher: RwLock<matchit::Router<RouteKey>>,
}

impl Router {
    /// Create a new router
    pub fn new() -> Self {
        Self {
            routes: RwLock::new(HashMap::new()),
            path_matcher: RwLock::new(matchit::Router::new()),
        }
    }

    /// Register a route handler
    pub fn register(
        &self,
        method: HttpMethod,
        path: String,
        handler_name: String,
        protected: bool,
        required_role: Option<String>,
        is_sse: bool,
    ) -> RuntimeResult<()> {
        let key = RouteKey {
            method,
            path: path.clone(),
        };

        let handler = RouteHandler {
            method,
            path: path.clone(),
            handler_name,
            protected,
            required_role,
            is_sse,
            is_ws: false,
            redirect_destination: None,
        };

        // Store in routes map
        {
            let mut routes = self.routes.write();
            routes.insert(key.clone(), handler);
        }

        // Add to path matcher
        // Convert Express-style :param to matchit-style {param}
        // matchit 0.8+ uses {id} syntax instead of :id
        let matchit_path = convert_express_to_matchit(&path);

        {
            let mut matcher = self.path_matcher.write();
            // matchit returns an error if the path is already registered,
            // which we ignore since we're replacing the route
            let _ = matcher.insert(matchit_path, key);
        }

        Ok(())
    }

    /// Register a static redirect route.
    ///
    /// When matched, the server immediately returns an HTTP redirect to
    /// `to_path` with `status` (301/302/…) without invoking any WASM handler.
    pub fn register_redirect(
        &self,
        method: HttpMethod,
        from_path: String,
        to_path: String,
        status: u16,
    ) -> RuntimeResult<()> {
        let key = RouteKey {
            method,
            path: from_path.clone(),
        };

        let handler = RouteHandler {
            method,
            path: from_path.clone(),
            handler_name: String::new(),
            protected: false,
            required_role: None,
            is_sse: false,
            is_ws: false,
            redirect_destination: Some((to_path, status)),
        };

        {
            let mut routes = self.routes.write();
            routes.insert(key.clone(), handler);
        }

        let matchit_path = convert_express_to_matchit(&from_path);
        {
            let mut matcher = self.path_matcher.write();
            let _ = matcher.insert(matchit_path, key);
        }

        Ok(())
    }

    /// Register a WebSocket (LIVE) route handler.
    ///
    /// The path is stored in the route registry with `is_ws = true`.  The
    /// server's fallback handler recognises this flag and performs the WebSocket
    /// upgrade instead of calling a regular WASM handler.
    pub fn register_ws(&self, path: String, handler_name: String) -> RuntimeResult<()> {
        let key = RouteKey {
            method: HttpMethod::GET,
            path: path.clone(),
        };

        let handler = RouteHandler {
            method: HttpMethod::GET,
            path: path.clone(),
            handler_name,
            protected: false,
            required_role: None,
            is_sse: false,
            is_ws: true,
            redirect_destination: None,
        };

        {
            let mut routes = self.routes.write();
            routes.insert(key.clone(), handler);
        }

        let matchit_path = convert_express_to_matchit(&path);
        {
            let mut matcher = self.path_matcher.write();
            let _ = matcher.insert(matchit_path, key);
        }

        Ok(())
    }

    /// Find a handler for the given method and path
    pub fn find(
        &self,
        method: HttpMethod,
        path: &str,
    ) -> Option<(RouteHandler, HashMap<String, String>)> {
        use tracing::debug;

        debug!("Router::find: Looking for {} {}", method.as_str(), path);

        let matcher = self.path_matcher.read();
        let routes = self.routes.read();

        // Try to match the path
        if let Ok(matched) = matcher.at(path) {
            debug!(
                "Router::find: matchit matched path '{}' to route '{}'",
                path, matched.value.path
            );

            // Check if this path has a handler for the requested method
            let key = RouteKey {
                method,
                path: matched.value.path.clone(),
            };

            // HEAD is answered by the GET handler when no explicit HEAD
            // route exists (RFC 9110 §9.3.2); hyper drops the body.
            let fallback_key;
            let handler = match routes.get(&key) {
                Some(h) => Some(h),
                None if method == HttpMethod::HEAD => {
                    fallback_key = RouteKey {
                        method: HttpMethod::GET,
                        path: matched.value.path.clone(),
                    };
                    routes.get(&fallback_key).filter(|h| !h.is_ws)
                }
                None => None,
            };

            if let Some(handler) = handler {
                // Extract path parameters
                let params: HashMap<String, String> = matched
                    .params
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();

                debug!("Router::find: Extracted params: {:?}", params);
                return Some((handler.clone(), params));
            } else {
                debug!(
                    "Router::find: No handler found for method {} on matched route",
                    method.as_str()
                );
            }
        } else {
            debug!("Router::find: matchit did not match path '{}'", path);
        }

        None
    }

    /// Check if a route exists
    pub fn exists(&self, method: HttpMethod, path: &str) -> bool {
        self.find(method, path).is_some()
    }

    /// Get all registered routes (for debugging)
    pub fn all_routes(&self) -> Vec<RouteHandler> {
        let routes = self.routes.read();
        routes.values().cloned().collect()
    }

    /// Clear all routes
    pub fn clear(&self) {
        let mut routes = self.routes.write();
        let mut matcher = self.path_matcher.write();
        routes.clear();
        *matcher = matchit::Router::new();
    }

    /// Get route count
    pub fn len(&self) -> usize {
        self.routes.read().len()
    }

    /// Check if router is empty
    pub fn is_empty(&self) -> bool {
        self.routes.read().is_empty()
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared router instance wrapped in Arc for thread-safety
pub type SharedRouter = Arc<Router>;

/// Create a new shared router
pub fn create_shared_router() -> SharedRouter {
    Arc::new(Router::new())
}

/// Route match result with extracted parameters
#[derive(Debug, Clone)]
pub struct RouteMatch {
    pub handler: RouteHandler,
    pub params: HashMap<String, String>,
}

impl RouteMatch {
    /// Get a path parameter by name
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(|s| s.as_str())
    }

    /// Get a path parameter or return an error
    pub fn require_param(&self, name: &str) -> RuntimeResult<&str> {
        self.param(name)
            .ok_or_else(|| RuntimeError::route(format!("Missing path parameter: {}", name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_method_parsing() {
        assert_eq!(HttpMethod::parse("GET").unwrap(), HttpMethod::GET);
        assert_eq!(HttpMethod::parse("post").unwrap(), HttpMethod::POST);
        assert_eq!(HttpMethod::parse("Delete").unwrap(), HttpMethod::DELETE);
        assert!(HttpMethod::parse("INVALID").is_err());
    }

    #[test]
    fn test_router_basic() {
        let router = Router::new();

        router
            .register(
                HttpMethod::GET,
                "/".to_string(),
                "__route_handler_0".to_string(),
                false,
                None,
                false,
            )
            .unwrap();
        router
            .register(
                HttpMethod::GET,
                "/api/users".to_string(),
                "__route_handler_1".to_string(),
                false,
                None,
                false,
            )
            .unwrap();
        router
            .register(
                HttpMethod::POST,
                "/api/users".to_string(),
                "__route_handler_2".to_string(),
                false,
                None,
                false,
            )
            .unwrap();

        assert!(router.find(HttpMethod::GET, "/").is_some());
        assert!(router.find(HttpMethod::GET, "/api/users").is_some());
        assert!(router.find(HttpMethod::POST, "/api/users").is_some());
        assert!(router.find(HttpMethod::DELETE, "/api/users").is_none());
        assert!(router.find(HttpMethod::GET, "/not-found").is_none());
    }

    #[test]
    fn test_head_falls_back_to_get_handler() {
        let router = Router::new();
        router
            .register(
                HttpMethod::GET,
                "/api/health".to_string(),
                "__route_handler_health".to_string(),
                false,
                None,
                false,
            )
            .unwrap();
        router
            .register(
                HttpMethod::POST,
                "/api/submit".to_string(),
                "__route_handler_submit".to_string(),
                false,
                None,
                false,
            )
            .unwrap();
        router
            .register_ws("/live".to_string(), "__ws_handler".to_string())
            .unwrap();

        let (handler, _) = router.find(HttpMethod::HEAD, "/api/health").unwrap();
        assert_eq!(handler.handler_name, "__route_handler_health");
        assert!(router.find(HttpMethod::HEAD, "/api/submit").is_none());
        assert!(router.find(HttpMethod::HEAD, "/live").is_none());
        assert!(router.find(HttpMethod::HEAD, "/missing").is_none());
    }

    #[test]
    fn test_explicit_head_route_wins_over_get() {
        let router = Router::new();
        for (method, name) in [(HttpMethod::GET, "__get"), (HttpMethod::HEAD, "__head")] {
            router
                .register(
                    method,
                    "/x".to_string(),
                    name.to_string(),
                    false,
                    None,
                    false,
                )
                .unwrap();
        }
        let (handler, _) = router.find(HttpMethod::HEAD, "/x").unwrap();
        assert_eq!(handler.handler_name, "__head");
    }

    #[test]
    fn test_router_with_params() {
        let router = Router::new();

        router
            .register(
                HttpMethod::GET,
                "/users/:id".to_string(),
                "__route_handler_0".to_string(),
                false,
                None,
                false,
            )
            .unwrap();
        router
            .register(
                HttpMethod::GET,
                "/posts/:post_id/comments/:comment_id".to_string(),
                "__route_handler_1".to_string(),
                false,
                None,
                false,
            )
            .unwrap();

        let (handler, params) = router.find(HttpMethod::GET, "/users/123").unwrap();
        assert_eq!(handler.handler_name, "__route_handler_0");
        assert_eq!(params.get("id"), Some(&"123".to_string()));

        let (handler, params) = router
            .find(HttpMethod::GET, "/posts/42/comments/7")
            .unwrap();
        assert_eq!(handler.handler_name, "__route_handler_1");
        assert_eq!(params.get("post_id"), Some(&"42".to_string()));
        assert_eq!(params.get("comment_id"), Some(&"7".to_string()));
    }

    #[test]
    fn test_router_protected_routes() {
        let router = Router::new();

        router
            .register(
                HttpMethod::GET,
                "/public".to_string(),
                "__route_handler_0".to_string(),
                false,
                None,
                false,
            )
            .unwrap();
        router
            .register(
                HttpMethod::GET,
                "/protected".to_string(),
                "__route_handler_1".to_string(),
                true,
                None,
                false,
            )
            .unwrap();
        router
            .register(
                HttpMethod::GET,
                "/admin".to_string(),
                "__route_handler_2".to_string(),
                true,
                Some("admin".to_string()),
                false,
            )
            .unwrap();

        let (handler, _) = router.find(HttpMethod::GET, "/public").unwrap();
        assert!(!handler.protected);
        assert!(handler.required_role.is_none());

        let (handler, _) = router.find(HttpMethod::GET, "/protected").unwrap();
        assert!(handler.protected);
        assert!(handler.required_role.is_none());

        let (handler, _) = router.find(HttpMethod::GET, "/admin").unwrap();
        assert!(handler.protected);
        assert_eq!(handler.required_role, Some("admin".to_string()));
    }

    #[test]
    fn test_router_clear() {
        let router = Router::new();

        router
            .register(
                HttpMethod::GET,
                "/".to_string(),
                "__route_handler_0".to_string(),
                false,
                None,
                false,
            )
            .unwrap();
        assert_eq!(router.len(), 1);

        router.clear();
        assert_eq!(router.len(), 0);
        assert!(router.is_empty());
    }

    #[test]
    fn test_express_to_matchit_conversion() {
        assert_eq!(convert_express_to_matchit("/users"), "/users");
        assert_eq!(convert_express_to_matchit("/users/:id"), "/users/{id}");
        assert_eq!(
            convert_express_to_matchit("/posts/:post_id/comments/:comment_id"),
            "/posts/{post_id}/comments/{comment_id}"
        );
        assert_eq!(
            convert_express_to_matchit("/api/users/:id/profile"),
            "/api/users/{id}/profile"
        );
    }

    #[test]
    fn test_router_api_users_id() {
        // Specific test for the reported bug scenario
        let router = Router::new();

        router
            .register(
                HttpMethod::GET,
                "/api/users/:id".to_string(),
                "__route_handler_2".to_string(),
                false,
                None,
                false,
            )
            .unwrap();

        // Test that the route matches and params are extracted
        let result = router.find(HttpMethod::GET, "/api/users/1");
        assert!(result.is_some(), "Route should match /api/users/1");

        let (handler, params) = result.unwrap();
        assert_eq!(handler.handler_name, "__route_handler_2");
        assert_eq!(
            params.get("id"),
            Some(&"1".to_string()),
            "Param 'id' should be '1'"
        );
    }
}
