//! Helpers shared by the integration tests that start the `clean-server`
//! binary or call the `cln` compiler.
//!
//! Setup problems here are test failures, never skips: a test that cannot
//! compile its program or start its server has checked nothing, so it must
//! not report success.

#![allow(dead_code)] // Each test binary uses its own subset of these helpers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a freshly spawned server may take to start serving. Startup
/// compiles the WASM module with Cranelift; with a debug build and every test
/// in a binary starting its own server at once, that can take several
/// seconds, so the limit is generous. A server that exits early fails at
/// once instead of waiting this out.
pub const SERVER_START_TIMEOUT: Duration = Duration::from_secs(60);

/// Pick a free port that no other test in this binary has been given.
///
/// `portpicker` only checks that a port is free at the moment of the call;
/// it does not reserve it. Tests run in parallel, so two of them could draw
/// the same port: one test's readiness probe then connects to the other
/// test's server. Ports are therefore handed out at most once per process.
pub fn pick_port() -> u16 {
    static TAKEN: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
    loop {
        let port = portpicker::pick_unused_port().expect("no free TCP port available");
        if TAKEN.lock().expect("port registry poisoned").insert(port) {
            return port;
        }
    }
}

/// Wait until `child` — and not some other process — serves `port`.
///
/// A successful TCP connect alone does not prove readiness: the port could
/// belong to someone else. The server logs `Server listening on
/// http://<addr>` just before it binds, so readiness is that line in *this*
/// child's stderr, the child still running, and the port accepting
/// connections. The server must be started with `--host 127.0.0.1`.
pub fn wait_for_server(
    child: &mut Child,
    port: u16,
    stderr_log: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let listening = format!("Server listening on http://127.0.0.1:{port}");
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("clean-server exited before serving: {status}"));
        }
        let logged = std::fs::read_to_string(stderr_log)
            .map(|log| log.contains(&listening))
            .unwrap_or(false);
        if logged && std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "clean-server did not serve 127.0.0.1:{port} within {timeout:?}"
    ))
}

/// The old-toolchain compiler, `~/.cleen/bin/cln`.
///
/// Plain `cln` on PATH is deliberately not consulted: on a developer machine
/// it may be a different toolchain. CI installs the compiler at this path
/// (see `.github/workflows/ci.yml`, job `e2e`), so a missing compiler is a
/// failure, not a reason to skip.
pub fn cln_compiler() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME is not set; cannot locate ~/.cleen/bin/cln");
    let cln = PathBuf::from(home).join(".cleen/bin/cln");
    assert!(
        cln.exists(),
        "cln compiler not found at {}. Install it with `cleen install latest` \
         (CI installs it from the Ivan-Pasco/clean-language-compiler-old release).",
        cln.display()
    );
    cln
}

/// Compile `source_path` to `wasm_path` with `cln compile <extra_args>`.
/// Returns the compiler's combined output on failure.
pub fn compile_cln(
    source_path: &Path,
    wasm_path: &Path,
    extra_args: &[&str],
) -> Result<(), String> {
    let output = std::process::Command::new(cln_compiler())
        .arg("compile")
        .args(extra_args)
        .arg(source_path)
        .arg("-o")
        .arg(wasm_path)
        .output()
        .map_err(|e| format!("failed to run cln: {e}"))?;
    if !output.status.success() || !wasm_path.exists() {
        return Err(format!(
            "cln compile {} failed ({}):\nstdout: {}\nstderr: {}",
            source_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}
