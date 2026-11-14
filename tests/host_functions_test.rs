//! End-to-end tests for the request host functions and the SSR handler
//! patterns, run against the real `clean-server` binary.
//!
//! Tests `_req_param`, `_req_body`, `_req_query`, `_req_header`, `_req_method`
//! and `_req_path` through programs compiled with the old-toolchain `cln`
//! (`~/.cleen/bin/cln`), plus redirect/HTML handler behaviour through
//! hand-written WAT modules.
//!
//! Every setup step (compiling, starting the server) is part of the test: a
//! failure there fails the test with the compiler or server output.

mod common;

use common::{SERVER_START_TIMEOUT, compile_cln, pick_port, wait_for_server};
use std::path::PathBuf;
use std::process::{Child, Command};
use tempfile::TempDir;

/// Helper struct to manage a test server process
struct TestServer {
    process: Child,
    port: u16,
    temp_dir: TempDir,
}

impl TestServer {
    /// Compile Clean source with `cln` and start a test server on it.
    /// Panics with the compiler or server output when either step fails.
    async fn new(source: &str) -> Self {
        let temp_dir = TempDir::new().expect("create temp dir");
        let source_path = temp_dir.path().join("test_app.cln");
        let wasm_path = temp_dir.path().join("test_app.wasm");
        std::fs::write(&source_path, source).expect("write test_app.cln");
        if let Err(e) = compile_cln(&source_path, &wasm_path, &[]) {
            panic!("test program does not compile:\n{e}\n--- source ---\n{source}");
        }
        Self::spawn(temp_dir, &wasm_path)
    }

    /// Start a test server from pre-compiled WASM bytes (bypasses compiler).
    async fn from_wasm(wasm_bytes: &[u8]) -> Self {
        let temp_dir = TempDir::new().expect("create temp dir");
        let wasm_path = temp_dir.path().join("test_app.wasm");
        std::fs::write(&wasm_path, wasm_bytes).expect("write test_app.wasm");
        Self::spawn(temp_dir, &wasm_path)
    }

    /// Start the server on `wasm_path` and wait until it serves its port.
    fn spawn(temp_dir: TempDir, wasm_path: &std::path::Path) -> Self {
        let server_path = find_clean_server_binary();
        let port = pick_port();
        let log_path = temp_dir.path().join("server.log");
        let err_path = temp_dir.path().join("server.err");
        let mut process = Command::new(&server_path)
            .arg(wasm_path)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .stdout(std::fs::File::create(&log_path).expect("create server.log"))
            .stderr(std::fs::File::create(&err_path).expect("create server.err"))
            .spawn()
            .unwrap_or_else(|e| panic!("failed to start {}: {e}", server_path.display()));

        if let Err(why) = wait_for_server(&mut process, port, &err_path, SERVER_START_TIMEOUT) {
            let _ = process.kill();
            let _ = process.wait();
            panic!(
                "server setup failed: {why}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                std::fs::read_to_string(&log_path).unwrap_or_default(),
                std::fs::read_to_string(&err_path).unwrap_or_default()
            );
        }

        Self {
            process,
            port,
            temp_dir,
        }
    }

    /// Get the base URL for this test server
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Everything the server process has written to stdout so far.
    fn stdout_bytes(&self) -> Vec<u8> {
        std::fs::read(self.temp_dir.path().join("server.log")).unwrap_or_default()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Kill the server process
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// The `clean-server` binary built for this test run.
fn find_clean_server_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_clean-server"))
}

// ============================================================================
// Test: _req_param extracts path parameters
// ============================================================================

#[tokio::test]
async fn test_req_param_extracts_path_params() {
    // Handler functions must be named __route_handler_N where N is the handler index
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/test/:id", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		string id = _req_param("id")
		return id
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/test/hello123", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("hello123"),
                "Expected body to contain 'hello123', got: {}",
                body
            );
        }
        Err(e) => {
            eprintln!("Request failed: {}", e);
            // Test fails if server isn't responding
            panic!("Server request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: _req_body reads request body
// ============================================================================

#[tokio::test]
async fn test_req_body_reads_post_body() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("POST", "/echo", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		return _req_body()
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/echo", server.base_url()))
        .body("hello world")
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("hello world"),
                "Expected body to contain 'hello world', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: _req_query extracts query parameters
// ============================================================================

#[tokio::test]
async fn test_req_query_extracts_query_params() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/search", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		return _req_query("name")
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/search?name=testvalue", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("testvalue"),
                "Expected body to contain 'testvalue', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: _req_header reads headers
// ============================================================================

#[tokio::test]
async fn test_req_header_reads_headers() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/header", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		return _req_header("x-custom-header")
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/header", server.base_url()))
        .header("X-Custom-Header", "my-custom-value")
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("my-custom-value"),
                "Expected body to contain 'my-custom-value', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: _req_method returns HTTP method
// ============================================================================

#[tokio::test]
async fn test_req_method_returns_method() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("POST", "/method", "__route_handler_0")
	status = _http_route("PUT", "/method", "__route_handler_1")
	status = _http_route("DELETE", "/method", "__route_handler_2")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		return _req_method()

	string __route_handler_1()
		return _req_method()

	string __route_handler_2()
		return _req_method()
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();

    // Test POST
    let response = client
        .post(format!("{}/method", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("POST"),
                "Expected body to contain 'POST', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("POST request failed: {}", e);
        }
    }

    // Test PUT
    let response = client
        .put(format!("{}/method", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("PUT"),
                "Expected body to contain 'PUT', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("PUT request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: _req_path returns request path
// ============================================================================

#[tokio::test]
async fn test_req_path_returns_path() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/api/test/path", "__route_handler_0")
	status = _http_route("GET", "/another/route", "__route_handler_1")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		return _req_path()

	string __route_handler_1()
		return _req_path()
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/api/test/path", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("/api/test/path"),
                "Expected body to contain '/api/test/path', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: Multiple path parameters
// ============================================================================

#[tokio::test]
async fn test_multiple_path_params() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/users/:userId/posts/:postId", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		string userId = _req_param("userId")
		string postId = _req_param("postId")
		return userId
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/users/user42/posts/post99", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            // Should return the first param (userId)
            assert!(
                body.contains("user42"),
                "Expected body to contain 'user42', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: Empty request body
// ============================================================================

#[tokio::test]
async fn test_req_body_empty() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("POST", "/check", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		string body = _req_body()
		return body
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/check", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            let status = resp.status();
            assert!(
                status.is_success(),
                "Expected success status, got: {}",
                status
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: Missing query parameter returns empty string
// ============================================================================

#[tokio::test]
async fn test_req_query_missing_param() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/missing", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		return _req_query("nonexistent")
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/missing", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            let status = resp.status();
            assert!(
                status.is_success(),
                "Expected success status, got: {}",
                status
            );
            // Empty string is valid response for missing param
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: Header name is case-insensitive
// ============================================================================

#[tokio::test]
async fn test_req_header_case_insensitive() {
    // Changed: `start:` now precedes `functions:` (SYN007), and `_http_route`
    // takes the handler's export name as a string, as function-registry.toml
    // declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("POST", "/content", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		return _req_header("content-type")
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/content", server.base_url()))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await;

    match response {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("application/json"),
                "Expected body to contain 'application/json', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// SSR Companion File Pattern Tests
// ============================================================================
//
// These tests verify the server correctly handles the companion file SSR
// pattern where route handlers may call _res_redirect() (guard pattern)
// and return HTML content (page rendering pattern).
//
// Tests use hand-crafted WAT modules to directly test the server's behavior
// without depending on the Clean Language compiler. `_http_route` takes the
// handler's export name (ptr+len), stored at offset 200 in each module.
// ============================================================================

// ============================================================================
// Test: Guard redirect - handler calls _res_redirect then returns empty string
// Verifies: HTTP 302, Location header set, empty body not returned as 200
// ============================================================================

#[tokio::test]
async fn test_guard_redirect_returns_302() {
    // WAT module: handler calls _res_redirect("/login", 302) then returns ""
    // Memory layout:
    //   0..2:   "GET" (3 bytes)
    //   16..25: "/dashboard" (10 bytes)
    //   32..37: "/login" (6 bytes)
    //   48..51: length-prefixed empty string (4 zero bytes = len 0)
    //   200..216: "__route_handler_0" (17 bytes), the handler export name
    let wat = r#"
    (module
      (import "env" "_http_route" (func $http_route (param i32 i32 i32 i32 i32 i32) (result i32)))
      (import "env" "_http_listen" (func $http_listen (param i32) (result i32)))
      (import "env" "_res_redirect" (func $res_redirect (param i32 i32 i32) (result i32)))

      (memory (export "memory") 1)

      (data (i32.const 0) "GET")
      (data (i32.const 200) "__route_handler_0")
      (data (i32.const 16) "/dashboard")
      (data (i32.const 32) "/login")
      (data (i32.const 48) "\00\00\00\00")

      (func (export "_start")
        (drop (call $http_route
          (i32.const 0) (i32.const 3)
          (i32.const 16) (i32.const 10)
          (i32.const 200) (i32.const 17)
        ))
        (drop (call $http_listen (i32.const 3000)))
      )

      (func (export "__route_handler_0") (result i32)
        (drop (call $res_redirect
          (i32.const 32) (i32.const 6)
          (i32.const 302)
        ))
        (i32.const 48)
      )
    )
    "#;

    let wasm_bytes = wat::parse_str(wat).expect("Failed to parse WAT");

    let server = TestServer::from_wasm(&wasm_bytes).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let response = client
        .get(format!("{}/dashboard", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            assert_eq!(
                resp.status().as_u16(),
                302,
                "Expected 302 redirect, got: {}",
                resp.status()
            );
            let location = resp
                .headers()
                .get("location")
                .expect("Expected Location header on redirect");
            assert_eq!(
                location.to_str().unwrap(),
                "/login",
                "Expected Location: /login, got: {}",
                location.to_str().unwrap()
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: Guard redirect with 301 permanent
// ============================================================================

#[tokio::test]
async fn test_guard_redirect_301_permanent() {
    // Memory layout:
    //   0..2:   "GET" (3 bytes)
    //   16..24: "/old-page" (9 bytes)
    //   32..44: "/new-location" (13 bytes)
    //   48..51: length-prefixed empty string
    //   200..216: "__route_handler_0" (17 bytes), the handler export name
    let wat = r#"
    (module
      (import "env" "_http_route" (func $http_route (param i32 i32 i32 i32 i32 i32) (result i32)))
      (import "env" "_http_listen" (func $http_listen (param i32) (result i32)))
      (import "env" "_res_redirect" (func $res_redirect (param i32 i32 i32) (result i32)))

      (memory (export "memory") 1)

      (data (i32.const 0) "GET")
      (data (i32.const 200) "__route_handler_0")
      (data (i32.const 16) "/old-page")
      (data (i32.const 32) "/new-location")
      (data (i32.const 48) "\00\00\00\00")

      (func (export "_start")
        (drop (call $http_route
          (i32.const 0) (i32.const 3)
          (i32.const 16) (i32.const 9)
          (i32.const 200) (i32.const 17)
        ))
        (drop (call $http_listen (i32.const 3000)))
      )

      (func (export "__route_handler_0") (result i32)
        (drop (call $res_redirect
          (i32.const 32) (i32.const 13)
          (i32.const 301)
        ))
        (i32.const 48)
      )
    )
    "#;

    let wasm_bytes = wat::parse_str(wat).expect("Failed to parse WAT");

    let server = TestServer::from_wasm(&wasm_bytes).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let response = client
        .get(format!("{}/old-page", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            assert_eq!(
                resp.status().as_u16(),
                301,
                "Expected 301 redirect, got: {}",
                resp.status()
            );
            let location = resp
                .headers()
                .get("location")
                .expect("Expected Location header on redirect");
            assert_eq!(
                location.to_str().unwrap(),
                "/new-location",
                "Expected Location: /new-location, got: {}",
                location.to_str().unwrap()
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: Page returns HTML - handler returns full HTML document
// Verifies: Content-Type is text/html, body contains HTML
// ============================================================================

#[tokio::test]
async fn test_page_returns_html_content_type() {
    // Handler returns "<!DOCTYPE html><html><body><h1>Hello</h1></body></html>"
    // which is 55 bytes = 0x37
    // Memory layout:
    //   0..2:    "GET" (3 bytes)
    //   16..20:  "/page" (5 bytes)
    //   32..35:  LE u32 length = 55 (0x37, 0x00, 0x00, 0x00)
    //   36..90:  HTML string data
    //   200..216: "__route_handler_0" (17 bytes), the handler export name
    let wat = r#"
    (module
      (import "env" "_http_route" (func $http_route (param i32 i32 i32 i32 i32 i32) (result i32)))
      (import "env" "_http_listen" (func $http_listen (param i32) (result i32)))

      (memory (export "memory") 1)

      (data (i32.const 0) "GET")
      (data (i32.const 200) "__route_handler_0")
      (data (i32.const 16) "/page")
      (data (i32.const 32) "\37\00\00\00<!DOCTYPE html><html><body><h1>Hello</h1></body></html>")

      (func (export "_start")
        (drop (call $http_route
          (i32.const 0) (i32.const 3)
          (i32.const 16) (i32.const 5)
          (i32.const 200) (i32.const 17)
        ))
        (drop (call $http_listen (i32.const 3000)))
      )

      (func (export "__route_handler_0") (result i32)
        (i32.const 32)
      )
    )
    "#;

    let wasm_bytes = wat::parse_str(wat).expect("Failed to parse WAT");

    let server = TestServer::from_wasm(&wasm_bytes).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/page", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            assert_eq!(
                resp.status().as_u16(),
                200,
                "Expected 200 OK, got: {}",
                resp.status()
            );
            let content_type = resp
                .headers()
                .get("content-type")
                .expect("Expected Content-Type header")
                .to_str()
                .unwrap()
                .to_string();
            assert!(
                content_type.contains("text/html"),
                "Expected text/html content type, got: {}",
                content_type
            );
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("<h1>Hello</h1>"),
                "Expected HTML body with <h1>Hello</h1>, got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: Static page (no companion) - plain HTML rendering still works
// ============================================================================

#[tokio::test]
async fn test_static_page_no_companion() {
    // Handler returns "<html><head><title>Static</title></head><body>Static content</body></html>"
    // which is 73 bytes = 0x49
    let wat = r#"
    (module
      (import "env" "_http_route" (func $http_route (param i32 i32 i32 i32 i32 i32) (result i32)))
      (import "env" "_http_listen" (func $http_listen (param i32) (result i32)))

      (memory (export "memory") 1)

      (data (i32.const 0) "GET")
      (data (i32.const 200) "__route_handler_0")
      (data (i32.const 16) "/about")
      (data (i32.const 32) "\49\00\00\00<html><head><title>Static</title></head><body>Static content</body></html>")

      (func (export "_start")
        (drop (call $http_route
          (i32.const 0) (i32.const 3)
          (i32.const 16) (i32.const 6)
          (i32.const 200) (i32.const 17)
        ))
        (drop (call $http_listen (i32.const 3000)))
      )

      (func (export "__route_handler_0") (result i32)
        (i32.const 32)
      )
    )
    "#;

    let wasm_bytes = wat::parse_str(wat).expect("Failed to parse WAT");

    let server = TestServer::from_wasm(&wasm_bytes).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/about", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            assert_eq!(
                resp.status().as_u16(),
                200,
                "Expected 200 OK, got: {}",
                resp.status()
            );
            let content_type = resp
                .headers()
                .get("content-type")
                .expect("Expected Content-Type header")
                .to_str()
                .unwrap()
                .to_string();
            assert!(
                content_type.contains("text/html"),
                "Expected text/html content type, got: {}",
                content_type
            );
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("Static content"),
                "Expected body to contain 'Static content', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Test: Page with data from request context (companion load pattern)
// Handler reads path param and embeds it in HTML response
// ============================================================================

#[tokio::test]
async fn test_page_with_request_data() {
    // This test uses the Clean Language compiler because it needs _req_param
    // and string concatenation which are complex to replicate in raw WAT
    // Changed: `_http_route` takes the handler's export name as a string, as
    // function-registry.toml declares (it was a table index).
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/user/:name", "__route_handler_0")
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		string name = _req_param("name")
		return "<!DOCTYPE html><html><body>" + name + "</body></html>"
"#;

    let server = TestServer::new(source).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/user/alice", server.base_url()))
        .send()
        .await;

    match response {
        Ok(resp) => {
            assert_eq!(
                resp.status().as_u16(),
                200,
                "Expected 200 OK, got: {}",
                resp.status()
            );
            let content_type = resp
                .headers()
                .get("content-type")
                .expect("Expected Content-Type header")
                .to_str()
                .unwrap()
                .to_string();
            assert!(
                content_type.contains("text/html"),
                "Expected text/html content type, got: {}",
                content_type
            );
            let body = resp.text().await.unwrap_or_default();
            assert!(
                body.contains("alice"),
                "Expected body to contain 'alice', got: {}",
                body
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }
}

// ============================================================================
// Strings are bytes: `start:` output from a compiled program, read from the
// server's stdout.
// ============================================================================

/// Compile `source`, start it on the server, and return the bytes `start:`
/// wrote to stdout. Panics when the program cannot be compiled or served.
async fn start_output(source: &str) -> Vec<u8> {
    TestServer::new(source).await.stdout_bytes()
}

#[tokio::test]
async fn test_split_empty_delimiter_gives_one_element_per_byte() {
    // Issue #3: `"abc".split("")` -> ["a", "b", "c"]; `"".split("")` -> [].
    let source = "start:\n\
                  \tlist<string> letters = \"abc\".split(\"\")\n\
                  \tprint(letters.length())\n\
                  \tlist<string> nothing = \"\".split(\"\")\n\
                  \tprint(nothing.length())\n\
                  \titerate ch in letters\n\
                  \t\tprint(ch)\n";
    let out = start_output(source).await;
    assert_eq!(
        String::from_utf8_lossy(&out),
        "3\n0\na\nb\nc\n",
        "split(\"\") output"
    );
}

#[tokio::test]
async fn test_print_writes_each_byte_of_a_multibyte_character() {
    // Issue #4: iterating "héllo" yields one-byte strings, two of them the
    // halves of "é"; print writes each byte unchanged with its newline.
    let source = "start:\n\
                  \titerate ch in \"h\u{e9}llo\"\n\
                  \t\tprint(ch)\n";
    let out = start_output(source).await;
    assert_eq!(
        out,
        [
            0x68, 0x0a, 0xc3, 0x0a, 0xa9, 0x0a, 0x6c, 0x0a, 0x6c, 0x0a, 0x6f, 0x0a
        ],
        "stdout as text: {:?}",
        String::from_utf8_lossy(&out)
    );
}
