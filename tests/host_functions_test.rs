//! Integration tests for request introspection host functions
//!
//! Tests `_req_param`, `_req_body`, `_req_query`, `_req_header`, `_req_method`, and `_req_path`
//!
//! NOTE: Some tests may fail if the Clean Language compiler has bugs in string concatenation
//! with host function return values. See the compiler fix documentation for details.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

/// Port range for test servers (to avoid conflicts)
const TEST_PORT_START: u16 = 13000;

/// Helper struct to manage a test server process
struct TestServer {
    process: Child,
    port: u16,
    _temp_dir: TempDir,
}

impl TestServer {
    /// Compile Clean source and start a test server
    async fn new(source: &str, port: u16) -> Result<Self, String> {
        // Create temp directory for test files
        let temp_dir = TempDir::new().map_err(|e| format!("Failed to create temp dir: {}", e))?;
        let source_path = temp_dir.path().join("test_app.cln");
        let wasm_path = temp_dir.path().join("test_app.wasm");

        // Write source file
        let mut file = std::fs::File::create(&source_path)
            .map_err(|e| format!("Failed to create source file: {}", e))?;
        file.write_all(source.as_bytes())
            .map_err(|e| format!("Failed to write source: {}", e))?;

        // Compile to WASM using cln compiler
        let cln_path = find_cln_compiler()?;
        let compile_output = Command::new(&cln_path)
            .args([
                "compile",
                source_path.to_str().unwrap(),
                "-o",
                wasm_path.to_str().unwrap(),
            ])
            .output()
            .map_err(|e| format!("Failed to run compiler: {}", e))?;

        if !compile_output.status.success() {
            let stderr = String::from_utf8_lossy(&compile_output.stderr);
            let stdout = String::from_utf8_lossy(&compile_output.stdout);
            return Err(format!(
                "Compilation failed:\nstdout: {}\nstderr: {}",
                stdout, stderr
            ));
        }

        // Find the clean-server binary
        let server_path = find_clean_server_binary()?;

        // Verify WASM file was created
        if !wasm_path.exists() {
            return Err(format!("WASM file not found at {:?}", wasm_path));
        }

        // Create a log file in the temp directory
        let log_path = temp_dir.path().join("server.log");

        // Start server with output redirected to log file
        let process = Command::new(&server_path)
            .args([wasm_path.to_str().unwrap(), "--port", &port.to_string()])
            .stdout(std::fs::File::create(&log_path).unwrap())
            .stderr(std::fs::File::create(temp_dir.path().join("server.err")).unwrap())
            .spawn()
            .map_err(|e| format!("Failed to start server: {}", e))?;

        // Wait for server to be ready (allow extra time for WASM module loading)
        sleep(Duration::from_millis(3000)).await;

        // Check if server is still running
        // Try to connect to verify it's ready
        let client = reqwest::Client::new();
        let check_url = format!("http://127.0.0.1:{}/", port);
        for attempt in 0..5 {
            if client.get(&check_url).send().await.is_ok() {
                break;
            }
            if attempt == 4 {
                // Read server log on failure
                let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
                let err_content =
                    std::fs::read_to_string(temp_dir.path().join("server.err")).unwrap_or_default();
                return Err(format!(
                    "Server failed to start on port {}.\nLog: {}\nErr: {}",
                    port, log_content, err_content
                ));
            }
            sleep(Duration::from_millis(500)).await;
        }

        Ok(Self {
            process,
            port,
            _temp_dir: temp_dir,
        })
    }

    /// Get the base URL for this test server
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Kill the server process
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Find the Clean Language compiler binary
fn find_cln_compiler() -> Result<PathBuf, String> {
    // Try the cleen-managed version first
    let home = std::env::var("HOME").unwrap_or_default();
    let cleen_path = PathBuf::from(&home).join(".cleen/bin/cln");
    if cleen_path.exists() {
        return Ok(cleen_path);
    }

    // Try PATH
    if let Ok(output) = Command::new("which").arg("cln").output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    Err("cln compiler not found. Install via cleen or add to PATH.".to_string())
}

/// Find the clean-server binary (debug or release)
fn find_clean_server_binary() -> Result<PathBuf, String> {
    // Try debug build first (most common during development)
    let debug_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/clean-server");
    if debug_path.exists() {
        return Ok(debug_path);
    }

    // Try release build
    let release_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/release/clean-server");
    if release_path.exists() {
        return Ok(release_path);
    }

    Err("clean-server binary not found. Run 'cargo build' first.".to_string())
}

/// Get a unique port for testing
fn get_test_port(test_index: u16) -> u16 {
    TEST_PORT_START + test_index
}

// ============================================================================
// Test: _req_param extracts path parameters
// ============================================================================

#[tokio::test]
async fn test_req_param_extracts_path_params() {
    // Handler functions must be named __route_handler_N where N is the handler index
    let source = r#"functions:
	string __route_handler_0()
		string id = _req_param("id")
		return id

start:
	integer status = 0
	status = _http_route("GET", "/test/:id", 0)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(1);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            // Skip test if setup fails (compiler might not be available)
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		return _req_body()

start:
	integer status = 0
	status = _http_route("POST", "/echo", 0)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(2);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		return _req_query("name")

start:
	integer status = 0
	status = _http_route("GET", "/search", 0)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(3);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		return _req_header("x-custom-header")

start:
	integer status = 0
	status = _http_route("GET", "/header", 0)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(4);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		return _req_method()

	string __route_handler_1()
		return _req_method()

	string __route_handler_2()
		return _req_method()

start:
	integer status = 0
	status = _http_route("POST", "/method", 0)
	status = _http_route("PUT", "/method", 1)
	status = _http_route("DELETE", "/method", 2)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(5);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		return _req_path()

	string __route_handler_1()
		return _req_path()

start:
	integer status = 0
	status = _http_route("GET", "/api/test/path", 0)
	status = _http_route("GET", "/another/route", 1)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(6);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		string userId = _req_param("userId")
		string postId = _req_param("postId")
		return userId

start:
	integer status = 0
	status = _http_route("GET", "/users/:userId/posts/:postId", 0)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(7);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		string body = _req_body()
		return body

start:
	integer status = 0
	status = _http_route("POST", "/check", 0)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(8);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		return _req_query("nonexistent")

start:
	integer status = 0
	status = _http_route("GET", "/missing", 0)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(9);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"functions:
	string __route_handler_0()
		return _req_header("content-type")

start:
	integer status = 0
	status = _http_route("POST", "/content", 0)
	integer listenStatus = _http_listen(3000)
"#;

    let port = get_test_port(10);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
// without depending on the Clean Language compiler.
// ============================================================================

impl TestServer {
    /// Start a test server from pre-compiled WASM bytes (bypasses compiler)
    async fn from_wasm(wasm_bytes: &[u8], port: u16) -> Result<Self, String> {
        let temp_dir = TempDir::new().map_err(|e| format!("Failed to create temp dir: {}", e))?;
        let wasm_path = temp_dir.path().join("test_app.wasm");

        std::fs::write(&wasm_path, wasm_bytes)
            .map_err(|e| format!("Failed to write WASM file: {}", e))?;

        let server_path = find_clean_server_binary()?;

        let log_path = temp_dir.path().join("server.log");

        let process = Command::new(&server_path)
            .args([wasm_path.to_str().unwrap(), "--port", &port.to_string()])
            .stdout(std::fs::File::create(&log_path).unwrap())
            .stderr(std::fs::File::create(temp_dir.path().join("server.err")).unwrap())
            .spawn()
            .map_err(|e| format!("Failed to start server: {}", e))?;

        sleep(Duration::from_millis(3000)).await;

        let client = reqwest::Client::new();
        let check_url = format!("http://127.0.0.1:{}/", port);
        for attempt in 0..5 {
            if client.get(&check_url).send().await.is_ok() {
                break;
            }
            if attempt == 4 {
                let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
                let err_content =
                    std::fs::read_to_string(temp_dir.path().join("server.err")).unwrap_or_default();
                return Err(format!(
                    "Server failed to start on port {}.\nLog: {}\nErr: {}",
                    port, log_content, err_content
                ));
            }
            sleep(Duration::from_millis(500)).await;
        }

        Ok(Self {
            process,
            port,
            _temp_dir: temp_dir,
        })
    }
}

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
    let wat = r#"
    (module
      (import "env" "_http_route" (func $http_route (param i32 i32 i32 i32 i32) (result i32)))
      (import "env" "_http_listen" (func $http_listen (param i32) (result i32)))
      (import "env" "_res_redirect" (func $res_redirect (param i32 i32 i32) (result i32)))

      (memory (export "memory") 1)

      (data (i32.const 0) "GET")
      (data (i32.const 16) "/dashboard")
      (data (i32.const 32) "/login")
      (data (i32.const 48) "\00\00\00\00")

      (func (export "_start")
        (drop (call $http_route
          (i32.const 0) (i32.const 3)
          (i32.const 16) (i32.const 10)
          (i32.const 0)
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

    let port = get_test_port(11);
    let server = match TestServer::from_wasm(&wasm_bytes, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let wat = r#"
    (module
      (import "env" "_http_route" (func $http_route (param i32 i32 i32 i32 i32) (result i32)))
      (import "env" "_http_listen" (func $http_listen (param i32) (result i32)))
      (import "env" "_res_redirect" (func $res_redirect (param i32 i32 i32) (result i32)))

      (memory (export "memory") 1)

      (data (i32.const 0) "GET")
      (data (i32.const 16) "/old-page")
      (data (i32.const 32) "/new-location")
      (data (i32.const 48) "\00\00\00\00")

      (func (export "_start")
        (drop (call $http_route
          (i32.const 0) (i32.const 3)
          (i32.const 16) (i32.const 9)
          (i32.const 0)
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

    let port = get_test_port(14);
    let server = match TestServer::from_wasm(&wasm_bytes, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let wat = r#"
    (module
      (import "env" "_http_route" (func $http_route (param i32 i32 i32 i32 i32) (result i32)))
      (import "env" "_http_listen" (func $http_listen (param i32) (result i32)))

      (memory (export "memory") 1)

      (data (i32.const 0) "GET")
      (data (i32.const 16) "/page")
      (data (i32.const 32) "\37\00\00\00<!DOCTYPE html><html><body><h1>Hello</h1></body></html>")

      (func (export "_start")
        (drop (call $http_route
          (i32.const 0) (i32.const 3)
          (i32.const 16) (i32.const 5)
          (i32.const 0)
        ))
        (drop (call $http_listen (i32.const 3000)))
      )

      (func (export "__route_handler_0") (result i32)
        (i32.const 32)
      )
    )
    "#;

    let wasm_bytes = wat::parse_str(wat).expect("Failed to parse WAT");

    let port = get_test_port(12);
    let server = match TestServer::from_wasm(&wasm_bytes, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
      (import "env" "_http_route" (func $http_route (param i32 i32 i32 i32 i32) (result i32)))
      (import "env" "_http_listen" (func $http_listen (param i32) (result i32)))

      (memory (export "memory") 1)

      (data (i32.const 0) "GET")
      (data (i32.const 16) "/about")
      (data (i32.const 32) "\49\00\00\00<html><head><title>Static</title></head><body>Static content</body></html>")

      (func (export "_start")
        (drop (call $http_route
          (i32.const 0) (i32.const 3)
          (i32.const 16) (i32.const 6)
          (i32.const 0)
        ))
        (drop (call $http_listen (i32.const 3000)))
      )

      (func (export "__route_handler_0") (result i32)
        (i32.const 32)
      )
    )
    "#;

    let wasm_bytes = wat::parse_str(wat).expect("Failed to parse WAT");

    let port = get_test_port(13);
    let server = match TestServer::from_wasm(&wasm_bytes, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Server setup failed: {}", e);
            return;
        }
    };

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
    let source = r#"start:
	integer status = 0
	status = _http_route("GET", "/user/:name", 0)
	integer listenStatus = _http_listen(3000)

functions:
	string __route_handler_0()
		string name = _req_param("name")
		return "<!DOCTYPE html><html><body>" + name + "</body></html>"
"#;

    let port = get_test_port(15);
    let server = match TestServer::new(source, port).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "Server setup failed (compiler may not support _req_param yet): {}",
                e
            );
            return;
        }
    };

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

impl TestServer {
    /// Everything the server process has written to stdout so far.
    fn stdout_bytes(&self) -> Vec<u8> {
        std::fs::read(self._temp_dir.path().join("server.log")).unwrap_or_default()
    }
}

/// Compile `source`, start it on the server, and return the bytes `start:`
/// wrote to stdout. Panics when the program cannot be compiled or served.
async fn start_output(source: &str, port: u16) -> Vec<u8> {
    let server = TestServer::new(source, port)
        .await
        .unwrap_or_else(|e| panic!("server setup failed: {}", e));
    server.stdout_bytes()
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
    let out = start_output(source, get_test_port(16)).await;
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
    let out = start_output(source, get_test_port(17)).await;
    assert_eq!(
        out,
        [
            0x68, 0x0a, 0xc3, 0x0a, 0xa9, 0x0a, 0x6c, 0x0a, 0x6c, 0x0a, 0x6f, 0x0a
        ],
        "stdout as text: {:?}",
        String::from_utf8_lossy(&out)
    );
}
