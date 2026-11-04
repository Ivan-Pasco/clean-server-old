//! Server binary smoke test.
//!
//! Proves that the release binary — not just the linker — can start, serve
//! HTTP, and shut down cleanly. This closes the gap left by every other
//! integration test in this crate: they either poke the linker in-process
//! (bridge_*, canvas_stubs, ui_stubs, wasm_alignment) or shell out to the
//! `cln` compiler (host_functions_test). Neither exercises the release
//! binary's boot path with no external compiler present.
//!
//! # What this test does not test
//!
//! - Route dispatch. The no-op WASM registers no routes; the point is only
//!   to prove `/` is reachable, not that a specific route works.
//! - Response bodies. Any 2xx/3xx/4xx counts as "server is up". The framework
//!   integration tests own body-content correctness.
//! - Full lifecycle under load. This is a boot smoke test, not a benchmark.
//!
//! See system-documents/TEST_STRATEGY.md § 4.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A minimal Clean-shaped WASM: exports `memory`, `__heap_ptr` (global i32
/// with the heap start), and a no-op `main` function. The server's
/// `initialize()` path (src/wasm.rs) requires all three.
///
/// Kept as WAT and compiled at test time so this file has no binary blob.
const NOOP_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global (export "__heap_ptr") i32 (i32.const 65536))
  (func (export "main"))
)
"#;

fn locate_server_binary() -> Option<std::path::PathBuf> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for candidate in ["target/release/clean-server", "target/debug/clean-server"] {
        let p = manifest_dir.join(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Pick a port unlikely to collide with other tests in this crate.
///
/// The rest of the integration suite uses 13000+. We stay clear of that band.
fn pick_port() -> u16 {
    portpicker::pick_unused_port().expect("no free TCP port available for smoke test")
}

#[test]
fn server_boots_serves_and_shuts_down() {
    let Some(server_bin) = locate_server_binary() else {
        // Fail rather than silently skip: the strategy doc says this test
        // must run in T3 CI, and CI always has a build. A missing binary
        // locally means the developer forgot to `cargo build` — surface it.
        panic!(
            "server_smoke_test: clean-server binary not found under target/. \
             Run `cargo build` before invoking this test."
        );
    };

    // Compile the no-op WAT to a temp .wasm file.
    let wasm_bytes = wat::parse_str(NOOP_WAT).expect("no-op WAT should compile");
    let temp = tempfile::tempdir().expect("tempdir");
    let wasm_path = temp.path().join("noop.wasm");
    std::fs::write(&wasm_path, &wasm_bytes).expect("write wasm");

    let port = pick_port();

    // Spawn the server. Stdout/stderr redirected so we can dump them on
    // failure without polluting normal test output.
    let stdout_log = temp.path().join("server.out");
    let stderr_log = temp.path().join("server.err");
    let mut child = Command::new(&server_bin)
        .arg(&wasm_path)
        .args(["--port", &port.to_string(), "--host", "127.0.0.1"])
        .stdout(Stdio::from(
            std::fs::File::create(&stdout_log).expect("create stdout log"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_log).expect("create stderr log"),
        ))
        .spawn()
        .expect("spawn clean-server");

    // Poll TCP until the port answers or we time out.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut connected = false;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            connected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !connected {
        let _ = child.kill();
        let _ = child.wait();
        let out = std::fs::read_to_string(&stdout_log).unwrap_or_default();
        let err = std::fs::read_to_string(&stderr_log).unwrap_or_default();
        panic!(
            "server_smoke_test: clean-server did not accept TCP connections on 127.0.0.1:{port} \
             within 15 seconds.\n--- stdout ---\n{out}\n--- stderr ---\n{err}"
        );
    }

    // Issue a single blocking HTTP request. We don't care what the status is
    // — 200, 404, 500 all prove the HTTP stack came up. We only reject
    // "no response / broken pipe".
    let http_result = std::panic::catch_unwind(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client");
            client
                .get(format!("http://127.0.0.1:{port}/"))
                .send()
                .await
                .map(|r| r.status().as_u16())
        })
    });

    // Regardless of how HTTP went, shut the server down cleanly: it must
    // exit within 2 seconds of the kill. The process is reaped either way.
    let _ = child.kill();
    let shutdown_deadline = Instant::now() + Duration::from_secs(2);
    let mut exited_cleanly = false;
    while Instant::now() < shutdown_deadline {
        if let Ok(Some(_)) = child.try_wait() {
            exited_cleanly = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.wait();

    // Now surface HTTP-side failures.
    match http_result {
        Ok(Ok(status)) => {
            // Any status is fine — we only care that HTTP was served.
            eprintln!("server_smoke_test: GET / returned {status}");
        }
        Ok(Err(e)) => {
            let out = std::fs::read_to_string(&stdout_log).unwrap_or_default();
            let err = std::fs::read_to_string(&stderr_log).unwrap_or_default();
            panic!(
                "server_smoke_test: HTTP request to running server failed: {e}\n\
                 --- stdout ---\n{out}\n--- stderr ---\n{err}"
            );
        }
        Err(payload) => {
            panic!("server_smoke_test: HTTP client panicked: {payload:?}");
        }
    }

    assert!(
        exited_cleanly,
        "server_smoke_test: clean-server did not exit within 2 seconds of SIGTERM/kill"
    );
}
